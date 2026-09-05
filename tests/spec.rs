use snouty::testutils::{
    MockApiServer, OCIRegistry, available_runtimes, filtered_path_without_binary, skip_or_fail,
};
use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Stdio;
use std::thread;
use testscript_rs::testscript;

fn err(msg: String) -> testscript_rs::Error {
    testscript_rs::Error::Generic(msg)
}

/// Resolve a spec-supplied path to a concrete filesystem path.
///
/// `${VAR}` references are expanded from the test environment first, then a
/// leading `~` (bare or `~/...`) expands to the test's isolated `$HOME` — the
/// same `HOME` the snouty subprocess sees, so a spec can point at the global
/// `settings.toml` that `snouty login` writes under it. A remaining relative
/// path is resolved against the spec's working directory, matching where inline
/// `-- file --` fixtures land.
fn resolve_spec_path(
    env: &testscript_rs::TestEnvironment,
    raw: &str,
) -> testscript_rs::Result<std::path::PathBuf> {
    let expanded = env.substitute_env_vars(raw);
    if let Some(rest) = expanded.strip_prefix('~') {
        let home = env
            .env_vars
            .get("HOME")
            .ok_or_else(|| err("`~` used in a path but HOME is not set".to_string()))?;
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        return Ok(std::path::Path::new(home).join(rest));
    }

    let path = std::path::PathBuf::from(expanded);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(env.current_dir.join(path))
    }
}

/// `file <path> <pattern>`: assert the contents of the file at `<path>` match the
/// regex `<pattern>`, mirroring the built-in `stdout`/`stderr` matchers (combine
/// with a leading `!` to assert the pattern is absent). `<path>` may start with
/// `~` to reference the test's isolated `$HOME` (see [`resolve_spec_path`]).
fn cmd_file(
    env: &mut testscript_rs::TestEnvironment,
    args: &[String],
) -> testscript_rs::Result<()> {
    let (path_arg, pattern) = match args {
        [path, rest @ ..] if !rest.is_empty() => (path, rest.join(" ")),
        _ => return Err(err("file requires <path> <pattern>".to_string())),
    };
    let path = resolve_spec_path(env, path_arg)?;
    let re = regex::Regex::new(&pattern)
        .map_err(|e| err(format!("invalid file pattern `{pattern}`: {e}")))?;
    let contents = std::fs::read_to_string(&path)
        .map_err(|e| err(format!("could not read {}: {e}", path.display())))?;
    if !re.is_match(&contents) {
        return Err(err(format!(
            "file {} does not match /{pattern}/\ncontents:\n{contents}",
            path.display()
        )));
    }
    Ok(())
}

fn cmd_set_env(
    env: &mut testscript_rs::TestEnvironment,
    args: &[String],
) -> testscript_rs::Result<()> {
    // Usage: set-env KEY value...
    // Interpolates ${VAR} references in value using env.env_vars.
    if args.len() < 2 {
        return Err(err("set-env requires KEY and value".to_string()));
    }
    let key = &args[0];
    let raw_value = args[1..].join(" ");
    let value = env.substitute_env_vars(&raw_value);
    env.set_env_var(key, &value);
    Ok(())
}

fn cmd_remove_image(
    env: &mut testscript_rs::TestEnvironment,
    args: &[String],
) -> testscript_rs::Result<()> {
    // Usage: remove-image <name:tag>
    // Drops one tag from the engine's local store, so a later command sees
    // the image as absent. Interpolates ${VAR} references in the tag.
    let [image_ref] = args else {
        return Err(err("remove-image requires <name:tag>".to_string()));
    };
    let image_ref = env.substitute_env_vars(image_ref);
    ENGINE_CTX.with_borrow(|ctx| {
        let ctx = ctx
            .as_ref()
            .ok_or_else(|| err("ENGINE_CTX not set".to_string()))?;
        let output = std::process::Command::new(ctx.engine.name())
            .args(["rmi", &image_ref])
            .output()
            .map_err(|e| err(format!("remove-image: {e}")))?;
        if !output.status.success() {
            return Err(err(format!(
                "remove-image {image_ref}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    })
}

fn cmd_substitute(
    env: &mut testscript_rs::TestEnvironment,
    args: &[String],
) -> testscript_rs::Result<()> {
    // Usage: substitute <path>
    // Replaces ${VAR} references inside <path> with the environment's values.
    // A txtar fixture is written verbatim, so this is the only way a compose
    // fixture can name the registry the harness starts on a free port.
    let [path_arg] = args else {
        return Err(err("substitute requires <path>".to_string()));
    };
    let path = resolve_spec_path(env, path_arg)?;
    let contents = std::fs::read_to_string(&path)
        .map_err(|e| err(format!("could not read {}: {e}", path.display())))?;
    let substituted = env.substitute_env_vars(&contents);
    std::fs::write(&path, substituted)
        .map_err(|e| err(format!("could not write {}: {e}", path.display())))?;
    Ok(())
}

// --- Engine context (thread-local so fn-pointer commands can access it) ---

struct EngineContext {
    engine: Box<dyn snouty::container::ContainerRuntime>,
    built_images: Vec<String>,
}

#[derive(Clone, Copy)]
struct EngineSpecCase {
    file: &'static str,
    needs_registry: bool,
}

thread_local! {
    static ENGINE_CTX: RefCell<Option<EngineContext>> = const { RefCell::new(None) };
}

// --- Shared command handlers (function pointers for testscript CommandFn) ---

/// System env vars forwarded to child processes (container tools, coverage).
///
/// `TMPDIR` matters on macOS: podman recomputes the machine API socket path
/// from it on every invocation, so dropping it makes `podman machine inspect`
/// report a `/tmp` fallback path the socket was never bound at.
///
/// DBus configuration is deliberately omitted from FORWARDED_ENV_VARS since
/// it represents a global state that might leak into or out of tests.
const FORWARDED_ENV_VARS: &[&str] = &["PATH", "HOME", "LLVM_PROFILE_FILE", "TMPDIR"];

/// Build a `Command` for the snouty binary with a clean environment.
///
/// Clears the parent env, forwards [`FORWARDED_ENV_VARS`], and applies
/// the test environment's `env_vars`.
fn snouty_cmd(env: &testscript_rs::TestEnvironment, args: &[String]) -> std::process::Command {
    let bin = env!("CARGO_BIN_EXE_snouty");
    let mut cmd = std::process::Command::new(bin);
    cmd.args(args)
        .current_dir(&env.current_dir)
        .env_clear()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for var in FORWARDED_ENV_VARS {
        if let Ok(v) = std::env::var(var) {
            cmd.env(var, v);
        }
    }
    // Disable keychain access -- this isn't something we can mock for each test case
    cmd.env("SNOUTY_DISABLE_KEYCHAIN_CREDENTIAL_STORAGE", "1");
    cmd.env(
        "XDG_CONFIG_HOME",
        isolated_xdg_config_home(&env.current_dir),
    );
    // Isolate the API response cache per spec. Without this, snouty falls back
    // to the real system temp dir, and a cached response from an earlier spec
    // run (mock servers reuse ephemeral localhost ports) could leak in. The
    // snouty-specific variable leaves XDG_RUNTIME_DIR alone: rootless podman
    // resolves its API socket under XDG_RUNTIME_DIR, so overriding that would
    // break the podman engine specs.
    let cache_dir = env.current_dir.join("api-cache-isolation");
    std::fs::create_dir_all(&cache_dir).expect("create API cache isolation dir");
    cmd.env("SNOUTY_API_CACHE_DIR", &cache_dir);
    for (k, v) in &env.env_vars {
        cmd.env(k, v);
    }
    cmd
}

/// The isolated `XDG_CONFIG_HOME` for a spec's snouty subprocess: a per-spec dir
/// with no `snouty/` subdir, so a developer's or CI's real
/// `~/.config/snouty/settings.toml` can't leak in and change resolved
/// tenant/repository/etc. (The project file is already isolated — each spec runs
/// in its own work dir, so `./.snouty.toml` doesn't exist unless the spec makes
/// it.)
///
/// `XDG_CONFIG_HOME` is shared, though: on macOS, podman keeps its *machine
/// connection* under `$XDG_CONFIG_HOME/containers`, so an empty dir makes the
/// subprocess's `podman info` lose the VM and fail with a bogus local-socket
/// path — breaking the podman engine specs. So we re-expose the real podman
/// config (written under `$HOME/.config/containers` at machine-init, before this
/// override takes effect) via a symlink: podman still resolves its connection
/// while snouty stays isolated. Harmless on Linux, where podman reaches its
/// socket via `XDG_RUNTIME_DIR` and never consults this directory.
fn isolated_xdg_config_home(current_dir: &std::path::Path) -> std::path::PathBuf {
    let dir = current_dir.join("xdg-config-isolation");
    std::fs::create_dir_all(&dir).expect("create XDG_CONFIG_HOME isolation dir");

    if let Some(home) = std::env::var_os("HOME") {
        let real_containers = std::path::PathBuf::from(home)
            .join(".config")
            .join("containers");
        let link = dir.join("containers");
        // `symlink_metadata` (unlike `exists`) detects an existing link even if
        // its target is gone, so repeated calls within one spec don't re-link.
        if real_containers.is_dir() && link.symlink_metadata().is_err() {
            std::os::unix::fs::symlink(&real_containers, &link)
                .expect("symlink podman containers config into isolated XDG_CONFIG_HOME");
        }
    }
    dir
}

fn cmd_snouty(
    env: &mut testscript_rs::TestEnvironment,
    args: &[String],
) -> testscript_rs::Result<()> {
    let start = std::time::Instant::now();
    let expanded: Vec<String> = args.iter().map(|a| env.substitute_env_vars(a)).collect();
    let label = expanded.join(" ");
    let mut cmd = snouty_cmd(env, &expanded);
    if env.next_stdin.is_some() {
        cmd.stdin(Stdio::piped());
    }

    let mut child = cmd.spawn().map_err(|e| err(format!("spawn snouty: {e}")))?;
    if let Some(data) = env.next_stdin.take() {
        // snouty is free to exit before it reads all of stdin — `runs exec` with
        // a SCRIPT argument never reads it at all — and that breaks the pipe.
        // A broken pipe here is not a failure, so let the spec's own assertions
        // on exit status and output judge the run. Every other error still
        // fails, because it means the write itself went wrong.
        //
        // Linux nearly always hides this: a small write lands in the pipe buffer
        // and returns before snouty exits. macOS loses that race often enough to
        // fail a spec about one run in three.
        match child.stdin.take().unwrap().write_all(&data) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
            Err(e) => return Err(err(format!("write stdin: {e}"))),
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|e| err(format!("wait snouty: {e}")))?;
    eprintln!("[{:.1}s] snouty {label}", start.elapsed().as_secs_f64());
    let success = output.status.success();
    let stderr_str = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout_str = String::from_utf8_lossy(&output.stdout).into_owned();
    env.last_output = Some(output);
    if !success {
        return Err(err(format!(
            "snouty exited with non-zero status\nstderr:\n{stderr_str}\nstdout:\n{stdout_str}"
        )));
    }
    Ok(())
}

fn cmd_mock_server(
    env: &mut testscript_rs::TestEnvironment,
    args: &[String],
) -> testscript_rs::Result<()> {
    // Usage: mock-server <status> <body>
    // Starts a TCP mock HTTP server, sets ANTITHESIS_BASE_URL and auth env vars.
    if args.len() < 2 {
        return Err(err("mock-server requires <status> <body>".to_string()));
    }

    if is_staging() {
        propagate_antithesis_env(env)?;
        return Ok(());
    }

    let status: u16 = args[0]
        .parse()
        .map_err(|e| err(format!("invalid status code: {e}")))?;
    let body = args[1..].join(" ");

    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|e| err(format!("failed to bind: {e}")))?;
    let addr = listener
        .local_addr()
        .map_err(|e| err(format!("failed to get addr: {e}")))?;
    let url = format!("http://{addr}");

    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = [0u8; 4096];
            let _ = Read::read(&mut stream, &mut buf);

            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len(),
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });

    env.set_env_var("ANTITHESIS_BASE_URL", &url);
    env.set_env_var("ANTITHESIS_USERNAME", "testuser");
    env.set_env_var("ANTITHESIS_PASSWORD", "testpass");
    env.set_env_var("ANTITHESIS_TENANT", "testtenant");
    Ok(())
}

fn cmd_env_from_json(
    env: &mut testscript_rs::TestEnvironment,
    args: &[String],
) -> testscript_rs::Result<()> {
    // Usage: env_from_json <line_index> <json_key>
    // Parses the previous command's stdout as NDJSON, extracts <json_key>
    // from line <line_index> (0-based) and stores it as $R_<json_key>.
    if args.len() != 2 {
        return Err(err(
            "env_from_json requires <line_index> <json_key>".to_string()
        ));
    }
    let line_idx: usize = args[0]
        .parse()
        .map_err(|_| err("line_index must be a non-negative integer".to_string()))?;
    let key = &args[1];

    let output = env
        .last_output
        .as_ref()
        .ok_or_else(|| err("no previous command output".to_string()))?;
    let stdout = std::str::from_utf8(&output.stdout)
        .map_err(|e| err(format!("stdout is not valid UTF-8: {e}")))?;
    let lines: Vec<&str> = stdout.lines().collect();
    let line = lines.get(line_idx).ok_or_else(|| {
        err(format!(
            "stdout has only {} line(s); cannot read line {}",
            lines.len(),
            line_idx
        ))
    })?;
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|e| err(format!("parse JSON line: {e}")))?;
    let extracted = match value.get(key) {
        Some(serde_json::Value::Null) | None => {
            return Err(err(format!("key '{key}' not found in JSON")));
        }
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    };
    env.set_env_var(&format!("R_{key}"), &extracted);
    Ok(())
}

fn cmd_mock_runs_server(
    env: &mut testscript_rs::TestEnvironment,
    args: &[String],
) -> testscript_rs::Result<()> {
    let empty = match args {
        [] => false,
        [mode] if mode == "empty" => true,
        _ => {
            return Err(err(
                "mock-runs-server accepts either no arguments or 'empty'".to_string(),
            ));
        }
    };

    if is_staging() {
        if empty {
            return Err(err(
                "mock-runs-server empty is not supported against staging; gate the block with [!staging]".to_string(),
            ));
        }
        propagate_antithesis_env(env)?;
        return Ok(());
    }

    let server = if empty {
        MockApiServer::start_empty()
    } else {
        MockApiServer::start()
    };
    env.set_env_var("ANTITHESIS_BASE_URL", server.url());
    env.set_env_var("ANTITHESIS_API_KEY", server.token());
    env.set_env_var("ANTITHESIS_TENANT", "testtenant");
    env.last_output = Some(std::process::Output {
        status: std::process::ExitStatus::default(),
        stdout: format!("{}\n", server.token()).into_bytes(),
        stderr: Vec::new(),
    });
    std::mem::forget(server);
    Ok(())
}

fn cmd_mock_proxy(
    env: &mut testscript_rs::TestEnvironment,
    _args: &[String],
) -> testscript_rs::Result<()> {
    // Usage: mock-proxy
    //
    // Starts the mock Antithesis API behind an in-process HTTP forward proxy and
    // points snouty's proxy env var (ANTITHESIS_HTTPS_PROXY) at it. It sets the
    // API key and tenant but deliberately does NOT set ANTITHESIS_BASE_URL: the
    // spec sets that to an unresolvable host, so a request can only reach the
    // mock by traversing the proxy. That makes a successful `snouty runs` proof
    // that the proxy setting is honored end to end.
    if is_staging() {
        return Err(err(
            "mock-proxy is not supported against staging; guard the block with [!staging]"
                .to_string(),
        ));
    }

    let server = MockApiServer::start();
    let mock_addr = server
        .url()
        .strip_prefix("http://")
        .ok_or_else(|| err("mock server url missing http scheme".to_string()))?
        .to_string();
    let token = server.token().to_string();
    // Keep the mock server (and its listener thread) alive for the process.
    std::mem::forget(server);

    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| err(format!("bind proxy: {e}")))?;
    let proxy_addr = listener
        .local_addr()
        .map_err(|e| err(format!("proxy addr: {e}")))?;

    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mock_addr = mock_addr.clone();
            thread::spawn(move || {
                // A forwarding failure just drops the connection; snouty then
                // reports a request error and the spec's assertion fails loudly.
                let _ = proxy_forward(stream, &mock_addr);
            });
        }
    });

    env.set_env_var("ANTITHESIS_HTTPS_PROXY", &format!("http://{proxy_addr}"));
    env.set_env_var("ANTITHESIS_API_KEY", &token);
    env.set_env_var("ANTITHESIS_TENANT", "testtenant");
    Ok(())
}

/// A minimal HTTP forward proxy for one request/response exchange.
///
/// reqwest, configured with an HTTP proxy for an `http://` target, sends the
/// proxy a request line in absolute form (`GET http://host/path HTTP/1.1`). We
/// rewrite it to origin form (`GET /path HTTP/1.1`), relay it to `mock_addr`
/// (ignoring the — unresolvable — target host), and copy the response back
/// verbatim. The mock closes the connection after responding (`Connection:
/// close`), so `read_to_end` returns the full response and reqwest opens a
/// fresh connection per request (one exchange per proxy connection).
fn proxy_forward(mut client: TcpStream, mock_addr: &str) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = client.read(&mut chunk)?;
        if n == 0 {
            return Ok(()); // client closed before sending a full request head
        }
        buf.extend_from_slice(&chunk[..n]);
        let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };

        let headers_end = pos + 4;
        let head = String::from_utf8_lossy(&buf[..headers_end]).into_owned();
        let content_length = proxy_content_length(&head);
        let mut body = buf[headers_end..].to_vec();
        while body.len() < content_length {
            let n = client.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }

        let mut upstream = TcpStream::connect(mock_addr)?;
        upstream.write_all(proxy_rewrite_request_line(&head).as_bytes())?;
        upstream.write_all(&body)?;
        let mut response = Vec::new();
        upstream.read_to_end(&mut response)?;
        client.write_all(&response)?;
        return Ok(());
    }
}

/// Parse `Content-Length` from an HTTP header block (0 if absent).
fn proxy_content_length(head: &str) -> usize {
    head.lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())
                .flatten()
        })
        .unwrap_or(0)
}

/// Rewrite the request line of `head` from absolute form to origin form,
/// leaving every subsequent header (and the terminating blank line) untouched.
fn proxy_rewrite_request_line(head: &str) -> String {
    let request_line = head.split("\r\n").next().unwrap_or("");
    let mut parts = request_line.splitn(3, ' ');
    let method = parts.next().unwrap_or("GET");
    let uri = parts.next().unwrap_or("/");
    let version = parts.next().unwrap_or("HTTP/1.1");

    // `http://host[:port]/path?query` -> `/path?query`.
    let origin = match uri.split_once("://") {
        Some((_, authority_and_path)) => match authority_and_path.find('/') {
            Some(idx) => &authority_and_path[idx..],
            None => "/",
        },
        None => uri,
    };

    // `head[request_line.len()..]` keeps the leading "\r\n", the remaining
    // headers, and the trailing "\r\n\r\n" exactly as received.
    format!("{method} {origin} {version}{}", &head[request_line.len()..])
}

fn is_staging() -> bool {
    std::env::var("SNOUTY_STAGING")
        .ok()
        .is_some_and(|v| !v.is_empty() && v != "0")
}

fn propagate_antithesis_env(env: &mut testscript_rs::TestEnvironment) -> testscript_rs::Result<()> {
    let tenant = std::env::var("ANTITHESIS_TENANT")
        .map_err(|_| err("SNOUTY_STAGING is set but ANTITHESIS_TENANT is not".to_string()))?;
    let has_bearer = std::env::var("ANTITHESIS_API_KEY").is_ok();
    let has_basic = std::env::var("ANTITHESIS_USERNAME").is_ok()
        && std::env::var("ANTITHESIS_PASSWORD").is_ok();
    if !has_bearer && !has_basic {
        return Err(err(
            "SNOUTY_STAGING is set but no credentials found (set ANTITHESIS_API_KEY or ANTITHESIS_USERNAME+ANTITHESIS_PASSWORD)"
                .to_string(),
        ));
    }
    for var in [
        "ANTITHESIS_BASE_URL",
        "ANTITHESIS_API_KEY",
        "ANTITHESIS_USERNAME",
        "ANTITHESIS_PASSWORD",
        "ANTITHESIS_TENANT",
    ] {
        env.env_vars.remove(var);
    }
    env.set_env_var("ANTITHESIS_TENANT", &tenant);
    for var in [
        "ANTITHESIS_BASE_URL",
        "ANTITHESIS_API_KEY",
        "ANTITHESIS_USERNAME",
        "ANTITHESIS_PASSWORD",
    ] {
        if let Ok(v) = std::env::var(var) {
            env.set_env_var(var, &v);
        }
    }
    Ok(())
}

fn cmd_build_image(
    env: &mut testscript_rs::TestEnvironment,
    args: &[String],
) -> testscript_rs::Result<()> {
    // Usage: build-image [--platform <platform>] <name:tag> <dir>
    // Builds a container image from <dir> (relative to work_dir), tagged as
    // {registry}/<name:tag> so it matches compose references.
    // If <dir> contains a Dockerfile it is used; otherwise a scratch image
    // containing the directory contents is built.
    // Registry and engine come from the ENGINE_CTX thread-local.
    let (platform, image_ref, dir_arg) = match args {
        [image_ref, dir_arg] => (None, image_ref.to_string(), dir_arg.to_string()),
        [flag, platform, image_ref, dir_arg] if flag == "--platform" => (
            Some(platform.to_string()),
            image_ref.to_string(),
            dir_arg.to_string(),
        ),
        _ => {
            return Err(err(
                "build-image requires [--platform <platform>] <name:tag> <dir>".to_string(),
            ));
        }
    };
    let start = std::time::Instant::now();
    let label = args.join(" ");
    ENGINE_CTX.with_borrow_mut(|ctx| {
        let ctx = ctx
            .as_mut()
            .ok_or_else(|| err("ENGINE_CTX not set".to_string()))?;
        let dir = env.work_dir.join(dir_arg);
        let dockerfile = dir.join("Dockerfile");
        let dockerfile = dockerfile.exists().then_some(dockerfile.as_path());
        ctx.engine
            .build_image(&dir, &image_ref, dockerfile, platform.as_deref())
            .map_err(|e| err(format!("build-image: {e}")))?;
        eprintln!(
            "[{:.1}s] build-image {label}",
            start.elapsed().as_secs_f64()
        );
        ctx.built_images.push(image_ref);
        Ok(())
    })
}

fn requested_runtime_matches(runtime_name: &str) -> Result<bool, String> {
    match std::env::var("SNOUTY_TEST_RUNTIME") {
        Ok(requested) => match requested.as_str() {
            "docker" | "podman" => Ok(requested == runtime_name),
            _ => Err(format!(
                "invalid SNOUTY_TEST_RUNTIME `{requested}`: expected `docker` or `podman`"
            )),
        },
        Err(std::env::VarError::NotPresent) => Ok(true),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err("SNOUTY_TEST_RUNTIME must be valid UTF-8".to_string())
        }
    }
}

fn find_runtime(runtime_name: &str) -> Option<Box<dyn snouty::container::ContainerRuntime>> {
    available_runtimes()
        .into_iter()
        .find(|runtime| runtime.name() == runtime_name)
}

fn cleanup_engine_images(runtime_name: &str, built_images: &[String], registry_addr: Option<&str>) {
    // The engine spec cases run as separate `#[test]` processes in parallel and
    // share one local `containers/storage`. A forced image removal here mutates
    // that store's layer database while a sibling test's `podman build` is
    // enumerating images for its build-cache check ("getting top layer info"),
    // intermittently yielding `layer not known` (issue #136). On CI the runner
    // is ephemeral and thrown away after the job, so there is nothing to clean
    // up — skip the `rmi` entirely there to remove the writer that races those
    // concurrent builds. Locally we still clean up so a dev's image store does
    // not accumulate junk across runs.
    if std::env::var_os("CI").is_some() {
        eprintln!(
            "CI is set: skipping {} image cleanup (ephemeral runner; avoids racing concurrent builds, issue #136)",
            runtime_name
        );
        return;
    }
    for image in built_images {
        let _ = std::process::Command::new(runtime_name)
            .args(["rmi", "-f", image])
            .output();
        if let Some(registry_addr) = registry_addr {
            let prefixed = format!("{registry_addr}/{image}");
            let _ = std::process::Command::new(runtime_name)
                .args(["rmi", "-f", &prefixed])
                .output();
        }
    }
}

fn run_engine_spec_case(runtime_name: &'static str, case: EngineSpecCase) {
    if !requested_runtime_matches(runtime_name)
        .unwrap_or_else(|e| panic!("invalid test runtime selection: {e}"))
    {
        return;
    }

    let engine = match find_runtime(runtime_name) {
        Some(engine) => engine,
        None => {
            skip_or_fail(&format!("{runtime_name}: no container runtime available"));
            return;
        }
    };

    eprintln!("=== engine specs with: {runtime_name} ({}) ===", case.file);

    let registry = if case.needs_registry {
        match OCIRegistry::start(engine.as_ref()) {
            Some(registry) => Some(registry),
            None => return,
        }
    } else {
        None
    };
    let registry_addr = registry.as_ref().map(OCIRegistry::host_port);

    ENGINE_CTX.set(Some(EngineContext {
        engine: engine.clone_box(),
        built_images: Vec::new(),
    }));

    let name = runtime_name.to_string();
    let registry_addr_for_setup = registry_addr.clone();
    let is_docker = runtime_name == "docker";

    let result = testscript::run("specs_engine")
        .files([case.file])
        .condition("docker", is_docker)
        .setup(move |env| {
            env.set_env_var("RUST_LOG", "debug");
            env.set_env_var("SNOUTY_CONTAINER_ENGINE", &name);
            if let Some(addr) = registry_addr_for_setup.as_deref() {
                env.set_env_var("ANTITHESIS_REPOSITORY", addr);
            }
            Ok(())
        })
        .command("snouty", cmd_snouty)
        .command("mock-server", cmd_mock_server)
        .command("env_from_json", cmd_env_from_json)
        .command("build-image", cmd_build_image)
        .command("set-env", cmd_set_env)
        .command("substitute", cmd_substitute)
        .command("remove-image", cmd_remove_image)
        .execute();

    let built_images = ENGINE_CTX
        .with_borrow_mut(|ctx| ctx.take().map(|ctx| ctx.built_images).unwrap_or_default());
    cleanup_engine_images(engine.name(), &built_images, registry_addr.as_deref());

    if let Err(e) = result {
        panic!("\n{runtime_name} {}: {e}", case.file);
    }
}

// --- Test functions ---

#[test]
fn spec_tests() {
    let staging = is_staging();
    let specs_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("specs");
    let mut files: Vec<String> = std::fs::read_dir(&specs_dir)
        .expect("read specs/")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "txt"))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    files.sort();

    for file in files {
        let result = testscript::run("specs")
            .files([file.clone()])
            .condition("staging", staging)
            .setup(|env| {
                env.set_env_var("RUST_LOG", "debug");
                if let Some(path) = filtered_path_without_binary("snouty-update") {
                    env.set_env_var("PATH", &path);
                }
                Ok(())
            })
            .command("snouty", cmd_snouty)
            .command("mock-server", cmd_mock_server)
            .command("mock-runs-server", cmd_mock_runs_server)
            .command("mock-proxy", cmd_mock_proxy)
            .command("env_from_json", cmd_env_from_json)
            .command("file", cmd_file)
            .command("set-env", cmd_set_env)
            .command("snouty-bg", |env, args| {
                let child = snouty_cmd(env, args)
                    .spawn()
                    .map_err(|e| err(format!("spawn snouty-bg: {e}")))?;
                env.background_processes.insert("snouty".to_string(), child);
                Ok(())
            })
            .command("isolate-home", |env, args| {
                // Usage: isolate-home <name>
                //
                // `snouty login` persists credentials.toml and settings.toml
                // under the global settings dir, which is `$XDG_CONFIG_HOME/snouty`
                // when that var is set and otherwise `$HOME/.config/snouty`. The
                // shared spec setup pins an isolated XDG_CONFIG_HOME, so here we
                // point HOME at a fresh per-section temp dir and clear
                // XDG_CONFIG_HOME (snouty treats an empty value as unset) so the
                // login writes land under — and are read back from — that HOME.
                // Each <name> gives a section of the spec its own home, keeping its
                // writes isolated from sibling sections and from the developer's
                // real ~/.config.
                let name = args.first().map(String::as_str).unwrap_or("home");
                let home = env.work_dir.join(name);
                std::fs::create_dir_all(&home)
                    .map_err(|e| err(format!("failed to create isolated HOME: {e}")))?;
                let home = home
                    .to_str()
                    .ok_or_else(|| err("isolated HOME path is not valid UTF-8".to_string()))?;
                env.set_env_var("HOME", home);
                env.set_env_var("XDG_CONFIG_HOME", "");
                Ok(())
            })
            .command("setup-docs-db", |env, _args| {
                // Usage: setup-docs-db
                // Seeds an isolated cache home with the fixture docs.db and points
                // the binary at it via XDG_CACHE_HOME (snouty reads the DB from
                // <cache home>/snouty/docs.db).
                let fixture =
                    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/docs.db");
                let cache_home = env.work_dir.join("cache");
                let snouty_dir = cache_home.join("snouty");
                std::fs::create_dir_all(&snouty_dir)
                    .map_err(|e| err(format!("failed to create cache dir: {e}")))?;
                std::fs::copy(&fixture, snouty_dir.join("docs.db"))
                    .map_err(|e| err(format!("failed to copy fixture docs.db: {e}")))?;
                env.set_env_var("XDG_CACHE_HOME", cache_home.to_str().unwrap());
                Ok(())
            })
            .execute();

        match result {
            Ok(()) => {}
            Err(e) if e.to_string().contains("SKIP:") => {
                eprintln!("skipping {file}");
            }
            Err(e) => panic!("\n{e}"),
        }
    }
}

macro_rules! engine_spec_case_test {
    ($name:ident, $runtime:literal, $file:literal, $needs_registry:expr) => {
        #[test]
        fn $name() {
            run_engine_spec_case(
                $runtime,
                EngineSpecCase {
                    file: $file,
                    needs_registry: $needs_registry,
                },
            );
        }
    };
}

engine_spec_case_test!(
    podman_engine_launch_config_push_specs,
    "podman",
    "launch_config_push.txt",
    true
);
engine_spec_case_test!(
    podman_engine_launch_config_mirror_specs,
    "podman",
    "launch_config_mirror.txt",
    true
);
engine_spec_case_test!(
    podman_engine_launch_config_private_specs,
    "podman",
    "launch_config_private.txt",
    true
);
engine_spec_case_test!(
    podman_engine_validate_setup_specs,
    "podman",
    "validate_setup.txt",
    false
);
engine_spec_case_test!(
    podman_engine_validate_failures_specs,
    "podman",
    "validate_failures.txt",
    false
);
engine_spec_case_test!(
    podman_engine_validate_network_arch_specs,
    "podman",
    "validate_network_arch.txt",
    false
);
engine_spec_case_test!(
    podman_engine_validate_env_specs,
    "podman",
    "validate_env.txt",
    false
);
engine_spec_case_test!(
    podman_engine_validate_k8s_specs,
    "podman",
    "validate_k8s.txt",
    false
);
engine_spec_case_test!(
    docker_engine_launch_config_push_specs,
    "docker",
    "launch_config_push.txt",
    true
);
engine_spec_case_test!(
    docker_engine_launch_config_mirror_specs,
    "docker",
    "launch_config_mirror.txt",
    true
);
engine_spec_case_test!(
    docker_engine_launch_config_private_specs,
    "docker",
    "launch_config_private.txt",
    true
);
engine_spec_case_test!(
    docker_engine_validate_setup_specs,
    "docker",
    "validate_setup.txt",
    false
);
engine_spec_case_test!(
    docker_engine_validate_failures_specs,
    "docker",
    "validate_failures.txt",
    false
);
engine_spec_case_test!(
    docker_engine_validate_network_arch_specs,
    "docker",
    "validate_network_arch.txt",
    false
);
engine_spec_case_test!(
    docker_engine_validate_env_specs,
    "docker",
    "validate_env.txt",
    false
);
engine_spec_case_test!(
    docker_engine_validate_k8s_specs,
    "docker",
    "validate_k8s.txt",
    false
);
