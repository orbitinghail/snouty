use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use chrono::Utc;
use color_eyre::{
    Section, SectionExt,
    eyre::{Context, Result, eyre},
};

use crate::error::user_error;
use crate::process::output_with_timeout;
use crate::settings::Settings;

/// Wall-clock budget for each synchronous docker/podman call made while
/// discovering test commands (`ps`, `exec test -d`, `cp`). Discovery runs on
/// the current-thread runtime between the two `validate` timeout windows, so a
/// wedged daemon or a flapping container could otherwise block it — and the
/// whole CLI — forever, with neither `--timeout` nor ctrl+c able to interrupt a
/// blocking `Command`. These calls are normally sub-second; the generous bound
/// only exists to convert an indefinite hang into a clear error.
pub const DISCOVERY_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// The platform a test run executes. The config image is built for it, and a
/// private image is pulled for it.
pub const AMD64_PLATFORM: &str = "linux/amd64";

/// A container image's CPU architecture, as reported by the runtime.
///
/// snouty only cares whether an image is runnable on Antithesis, which is
/// x86-64 only — so this distinguishes [`Amd64`](Self::Amd64) from everything
/// else, keeping the raw runtime string for [`Other`](Self::Other) so error
/// messages can name the offending architecture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Architecture {
    /// `amd64` (x86-64) — the only architecture Antithesis runs.
    Amd64,
    /// Any other architecture; carries the runtime's raw name for diagnostics.
    Other(String),
}

impl From<&str> for Architecture {
    fn from(arch: &str) -> Self {
        match arch {
            "amd64" => Architecture::Amd64,
            other => Architecture::Other(other.to_string()),
        }
    }
}

impl std::fmt::Display for Architecture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Architecture::Amd64 => f.write_str("amd64"),
            Architecture::Other(arch) => f.write_str(arch),
        }
    }
}

/// Trait representing a container runtime (podman or docker).
pub trait ContainerRuntime: Send + Sync {
    /// The CLI command name (e.g. "podman" or "docker").
    fn name(&self) -> &str;

    /// The underlying engine ("podman" or "docker"), independent of which binary
    /// invokes it. These diverge only for podman-in-disguise (podman installed
    /// as the `docker` binary), where [`name`](Self::name) is `"docker"` but the
    /// engine is really podman.
    fn engine_kind(&self) -> &'static str;

    /// Clone into a boxed trait object.
    fn clone_box(&self) -> Box<dyn ContainerRuntime>;

    /// Return a `Command` pre-configured with the runtime binary and given args.
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(self.name());
        cmd.args(args);
        cmd
    }

    /// Like [`command`](Self::command) but returns a [`tokio::process::Command`].
    fn tokio_command(&self, args: &[&str]) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(self.name());
        cmd.args(args);
        cmd
    }

    /// The `DOCKER_HOST` value `docker-compose` should use to drive this
    /// runtime's container engine, or `None` to use docker-compose's default
    /// (the Docker daemon). Returns `Err` when the engine's API socket is
    /// required but unavailable. The default targets the Docker daemon.
    fn engine_docker_host(&self) -> Result<Option<String>> {
        Ok(None)
    }

    /// Push the image to the registry, returning the pinned image reference
    /// (e.g. `example.com/foo/image@sha256:...`).
    fn image_push(&self, image_ref: &str) -> Result<String>;

    /// Return whether the image is available in the local image store.
    fn image_exists(&self, image_ref: &str) -> Result<bool> {
        let runtime = self.name();
        let output = Command::new(runtime)
            .args(["image", "inspect", image_ref])
            .output()
            .wrap_err(format!("failed to run '{runtime} image inspect'"))?;
        image_exists_from_inspect_output(runtime, image_ref, output)
    }

    /// Return the local image architecture (for example `amd64` or `arm64`).
    fn image_architecture(&self, image_ref: &str) -> Result<Architecture> {
        let runtime = self.name();
        let output = Command::new(runtime)
            .args([
                "image",
                "inspect",
                "--format",
                "{{.Architecture}}",
                image_ref,
            ])
            .output()
            .wrap_err(format!("failed to run '{runtime} image inspect'"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(eyre!("'{runtime} image inspect {image_ref}' failed"))
                .with_section(move || stdout.trim().to_string().header("Stdout:"))
                .with_section(move || stderr.trim().to_string().header("Stderr:"));
        }

        let architecture = std::str::from_utf8(&output.stdout)
            .wrap_err("failed to parse image inspect output")?
            .trim();

        if architecture.is_empty() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(eyre!(
                "'{runtime} image inspect {image_ref}' returned empty architecture"
            ))
            .with_section(move || stderr.header("Stderr:"));
        }

        Ok(Architecture::from(architecture))
    }

    /// Read the `repo@sha256:...` digest references the local store associates
    /// with an image.
    ///
    /// Entries recorded by pulls are ground truth (the digest the registry
    /// actually served), but both podman and docker (containerd store) also
    /// synthesize entries for names an image was merely tagged with — so an
    /// entry does NOT prove the named registry serves the digest. Verify with
    /// [`remote_manifest`](Self::remote_manifest) before relying on one.
    fn image_repo_digests(&self, image_ref: &str) -> Result<Vec<String>> {
        let runtime = self.name();
        let output = Command::new(runtime)
            .args(["image", "inspect", image_ref])
            .output()
            .wrap_err_with(|| format!("failed to run '{runtime} image inspect'"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(eyre!("'{runtime} image inspect {image_ref}' failed"))
                .with_section(move || stderr.trim().to_string().header("Stderr:"));
        }
        let parsed: serde_json::Value = serde_json::from_slice(&output.stdout)
            .wrap_err_with(|| format!("failed to parse '{runtime} image inspect' output"))?;
        Ok(parsed
            .get(0)
            .and_then(|img| img.get("RepoDigests"))
            .and_then(|d| d.as_array())
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Ask the registry named by `image_ref` (typically `repo@sha256:...`)
    /// whether it serves that manifest — a manifest-only API round trip via
    /// `{runtime} manifest inspect`, using the runtime's own auth and TLS
    /// configuration. Never pulls or pushes anything.
    ///
    /// Every failure (missing manifest, auth, unreachable registry, or a
    /// runtime that can't inspect this manifest kind — podman errors on
    /// single-platform manifests) maps to [`RemoteManifest::NotFound`]:
    /// callers fall back to pushing, so degraded networks or runtime quirks
    /// can only cause extra uploads, never a bad pin.
    fn remote_manifest(&self, image_ref: &str) -> RemoteManifest;

    /// Tag an image with a new reference.
    fn image_tag(&self, src: &str, dst: &str) -> Result<()> {
        let runtime = self.name();
        let output = Command::new(runtime)
            .args(["tag", src, dst])
            .output()
            .wrap_err(format!("failed to run '{runtime} tag'"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(eyre!("'{runtime} tag {src} {dst}' failed"))
                .with_section(move || stderr.trim().to_string().header("Stderr:"));
        }
        Ok(())
    }

    /// Pull `image_ref` for [`AMD64_PLATFORM`], with the runtime's own
    /// credentials.
    fn image_pull(&self, image_ref: &str) -> Result<()> {
        pull_via_cli(self.name(), None, image_ref)
    }

    /// Build a container image from a directory.
    ///
    /// When `dockerfile` is `Some`, the given path is passed via `-f`.
    /// When `None`, a scratch image containing the directory contents is built
    /// via an implicit `FROM scratch\nCOPY . /\n` Dockerfile piped to stdin.
    ///
    /// When `platform` is `Some`, passes `--platform <platform>` to the build.
    fn build_image(
        &self,
        dir: &Path,
        image_ref: &str,
        dockerfile: Option<&Path>,
        platform: Option<&str>,
    ) -> Result<()> {
        let runtime = self.name();
        let scratch = dockerfile.is_none();

        let mut cmd = Command::new(runtime);
        cmd.args(["build", "-t", image_ref]);
        if let Some(platform) = platform {
            cmd.args(["--platform", platform]);
        }
        if let Some(df) = dockerfile {
            cmd.args(["-f", &df.display().to_string()]);
        } else {
            cmd.args(["-f", "-"]);
        }
        cmd.arg(".")
            .current_dir(dir)
            .stderr(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped());
        if scratch {
            cmd.stdin(std::process::Stdio::piped());
        }

        let mut child = cmd
            .spawn()
            .wrap_err(format!("failed to start '{runtime} build'"))?;

        if scratch && let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(b"FROM scratch\nCOPY . /\n")
                .wrap_err("failed to write Dockerfile to stdin")?;
        }

        let output = child
            .wait_with_output()
            .wrap_err(format!("failed to wait for '{runtime} build'"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(eyre!("'{runtime} build' failed"))
                .with_section(move || stdout.trim().to_string().header("Stdout:"))
                .with_section(move || stderr.trim().to_string().header("Stderr:"));
        }

        Ok(())
    }

    /// Build a scratch image from the contents of `config_dir` and push it.
    /// Callers are responsible for validating the directory beforehand
    /// (typically via [`crate::config::detect_config`]). Returns the pinned
    /// image reference.
    ///
    /// The image is always built for `linux/amd64` (x86-64) because Antithesis
    /// does not support arm64, and the host may well be an arm machine.
    fn build_and_push_config_image(&self, config_dir: &Path, image_ref: &str) -> Result<String> {
        eprintln!("Building config image: {}", image_ref);
        self.build_image(config_dir, image_ref, None, Some(AMD64_PLATFORM))?;

        eprintln!("Pushing config image: {}", image_ref);
        // image_push pins to `name:tag@digest`, but Antithesis's config-image
        // validator rejects a reference carrying both a :tag and an @digest
        // (a bug in the validator), so identify the image by digest alone.
        let pinned = digest_only_ref(&self.image_push(image_ref)?);
        eprintln!("Config image pushed successfully: {pinned}");
        Ok(pinned)
    }

    /// Copy the Antithesis test-template tree (`/opt/antithesis/test/v1`)
    /// out of a container into `dst`. Returns [`TestTemplates::Absent`]
    /// when the directory does not exist inside the container — a normal
    /// outcome for services that don't define any test commands. Real cp
    /// failures (permission denied, runtime errors, etc.) are returned as
    /// `Err` with stderr attached.
    ///
    /// When `running` is true, performs an unambiguous pre-flight existence
    /// check via `runtime exec <id> test -d <path>`. When false (stopped or
    /// not-yet-started containers — exec is unavailable), falls back to
    /// attempting the cp and inspecting stderr to distinguish absence from
    /// real errors.
    fn extract_test_templates(
        &self,
        container_id: &str,
        dst: &Path,
        running: bool,
    ) -> Result<TestTemplates> {
        if running {
            match self.container_path_kind(container_id, TEST_TEMPLATES_PATH)? {
                PathKind::Directory => {}
                PathKind::Missing => return Ok(TestTemplates::Absent),
                PathKind::OtherOrUnknown => {
                    // exec succeeded but path isn't a directory, or the
                    // pre-flight is inconclusive. Fall through to cp and let
                    // it produce the diagnostic.
                }
            }
        }

        let runtime = self.name();
        let src_arg = format!("{container_id}:{TEST_TEMPLATES_PATH}");
        let mut cp_cmd = Command::new(runtime);
        cp_cmd.args(["cp", &src_arg, &dst.display().to_string()]);
        let output = output_with_timeout(cp_cmd, DISCOVERY_COMMAND_TIMEOUT)
            .wrap_err_with(|| format!("failed to run '{runtime} cp'"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr_indicates_missing_source(&stderr) {
                return Ok(TestTemplates::Absent);
            }
            return Err(eyre!("'{runtime} cp' failed"))
                .with_section(move || stderr.trim().to_string().header("Stderr:"));
        }

        Ok(TestTemplates::Present)
    }

    /// Probe whether `path` exists inside `container_id` (must be running).
    /// Runs `runtime exec <id> test -d <path>`; exit 0 → Directory, exit 1 →
    /// Missing, any other failure (no shell, container died, etc.) →
    /// OtherOrUnknown so callers fall back to cp.
    fn container_path_kind(&self, container_id: &str, path: &str) -> Result<PathKind> {
        let runtime = self.name();
        let mut exec_cmd = Command::new(runtime);
        exec_cmd.args(["exec", container_id, "test", "-d", path]);
        let output = output_with_timeout(exec_cmd, DISCOVERY_COMMAND_TIMEOUT)
            .wrap_err_with(|| format!("failed to run '{runtime} exec'"))?;
        match output.status.code() {
            Some(0) => Ok(PathKind::Directory),
            Some(1) => Ok(PathKind::Missing),
            _ => Ok(PathKind::OtherOrUnknown),
        }
    }
}

/// Result of [`ContainerRuntime::container_path_kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// Path exists and is a directory.
    Directory,
    /// Path does not exist inside the container.
    Missing,
    /// Pre-flight inconclusive (no `test` binary, exec failed, etc.).
    OtherOrUnknown,
}

/// Standard path inside a container where Antithesis test templates live.
pub const TEST_TEMPLATES_PATH: &str = "/opt/antithesis/test/v1";

/// Result of [`ContainerRuntime::extract_test_templates`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestTemplates {
    /// Templates were present and copied into the destination directory.
    Present,
    /// The container does not have an Antithesis test-templates directory.
    Absent,
}

/// Match patterns `docker cp` and `podman cp` emit when the source path
/// doesn't exist inside the container. Used only as a fallback for stopped
/// containers where the `test -d` pre-flight can't run.
///
/// The loose substring `"no such file"` (without "or directory") is
/// deliberately excluded — it matches destination-side messages too.
fn stderr_indicates_missing_source(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("could not find the file")
        || s.contains("no such file or directory")
        || s.contains("does not exist in container")
        || s.contains("could not be found on container")
}

#[derive(Clone)]
pub struct PodmanRuntime {
    cmd: String,
}

impl PodmanRuntime {
    pub fn new(cmd: impl Into<String>) -> Self {
        Self { cmd: cmd.into() }
    }

    /// The host-side path of the podman machine's forwarded API socket, or
    /// `None` when there's no machine (e.g. native Linux podman). Used on macOS,
    /// where podman runs in a VM and the host can't reach the in-VM socket.
    fn podman_machine_socket(&self) -> Option<String> {
        let output = Command::new(&self.cmd)
            .args([
                "machine",
                "inspect",
                "--format",
                "{{.ConnectionInfo.PodmanSocket.Path}}",
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if path.is_empty() || path == "<no value>" {
            None
        } else {
            Some(resolve_machine_socket_path(path))
        }
    }
}

/// Resolve the podman machine API socket path to one that actually exists.
///
/// podman recomputes this path from the current `TMPDIR` on every invocation
/// (falling back to `/tmp` when unset), while the socket itself was bound
/// under the `TMPDIR` in effect at `podman machine start`. When those
/// environments differ, the reported path doesn't exist — probe the sibling
/// locations the path is derived from before giving up. If no candidate
/// exists, return the reported path unchanged and let docker-compose surface
/// its connection error naming it.
fn resolve_machine_socket_path(reported: String) -> String {
    let reported_path = Path::new(&reported);
    if reported_path.exists() {
        return reported;
    }
    let Some(file_name) = reported_path.file_name() else {
        return reported;
    };
    let mut candidates = Vec::new();
    if let Some(tmpdir) = std::env::var_os("TMPDIR") {
        candidates.push(
            std::path::PathBuf::from(tmpdir)
                .join("podman")
                .join(file_name),
        );
    }
    candidates.push(std::path::PathBuf::from("/tmp/podman").join(file_name));
    for candidate in candidates {
        if candidate.exists() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    reported
}

impl ContainerRuntime for PodmanRuntime {
    fn name(&self) -> &str {
        &self.cmd
    }

    fn engine_kind(&self) -> &'static str {
        "podman"
    }

    fn clone_box(&self) -> Box<dyn ContainerRuntime> {
        Box::new(self.clone())
    }

    /// Point docker-compose at podman's Docker-compatible API socket so podman
    /// backs Compose. Only the socket path is needed here; if the socket isn't
    /// actually listening, docker-compose surfaces its own clear, platform-
    /// agnostic connection error naming the path.
    fn engine_docker_host(&self) -> Result<Option<String>> {
        // On macOS, podman runs inside a VM. `podman info` reports the
        // in-VM socket path, which the host can't reach — the host-forwarded
        // socket comes from `podman machine inspect` instead.
        if cfg!(target_os = "macos")
            && let Some(path) = self.podman_machine_socket()
        {
            return Ok(Some(format!("unix://{path}")));
        }

        let output = Command::new(&self.cmd)
            .args(["info", "--format", "{{.Host.RemoteSocket.Path}}"])
            .output()
            .wrap_err_with(|| format!("failed to run '{} info'", self.cmd))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(eyre!(
                "'{} info' failed while detecting the podman API socket",
                self.cmd
            ))
            .with_section(move || stderr.trim().to_string().header("Stderr:"));
        }

        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if path.is_empty() {
            return Err(eyre!(
                "could not determine podman's API socket path; set DOCKER_HOST to a reachable container engine"
            ));
        }
        Ok(Some(format!("unix://{path}")))
    }

    fn remote_manifest(&self, image_ref: &str) -> RemoteManifest {
        remote_manifest_via_cli(&self.cmd, PODMAN_INSECURE_FLAG, image_ref)
    }

    fn image_pull(&self, image_ref: &str) -> Result<()> {
        pull_via_cli(&self.cmd, Some(PODMAN_INSECURE_FLAG), image_ref)
    }

    fn image_push(&self, image_ref: &str) -> Result<String> {
        let mut args = vec!["push"];
        if is_plain_http_registry(image_ref) {
            args.push(PODMAN_INSECURE_FLAG);
        }

        let digestfile =
            tempfile::NamedTempFile::new().wrap_err("failed to create temporary digest file")?;
        let digestfile_arg = format!("--digestfile={}", digestfile.path().display());
        args.push(&digestfile_arg);

        args.push(image_ref);

        let output = Command::new(&self.cmd)
            .args(&args)
            .output()
            .wrap_err(format!("failed to run '{} push'", self.cmd))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(eyre!("'{} push' failed", self.cmd))
                .with_section(move || stdout.trim().to_string().header("Stdout:"))
                .with_section(move || stderr.trim().to_string().header("Stderr:"));
        }

        let digest = std::fs::read_to_string(digestfile.path())
            .wrap_err("failed to read digest file")?
            .trim()
            .to_string();
        Ok(pinned_image_ref(image_ref, &digest))
    }
}

#[derive(Clone)]
pub struct DockerRuntime {
    cmd: String,
}

impl DockerRuntime {
    pub fn new(cmd: impl Into<String>) -> Self {
        Self { cmd: cmd.into() }
    }
}

impl ContainerRuntime for DockerRuntime {
    fn clone_box(&self) -> Box<dyn ContainerRuntime> {
        Box::new(self.clone())
    }

    fn name(&self) -> &str {
        &self.cmd
    }

    fn engine_kind(&self) -> &'static str {
        "docker"
    }

    fn remote_manifest(&self, image_ref: &str) -> RemoteManifest {
        remote_manifest_via_cli(&self.cmd, "--insecure", image_ref)
    }

    fn image_push(&self, image_ref: &str) -> Result<String> {
        let output = Command::new(&self.cmd)
            .args(["push", image_ref])
            .output()
            .wrap_err(format!("failed to run '{} push'", self.cmd))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(eyre!("'{} push' failed", self.cmd))
                .with_section(move || stdout.trim().to_string().header("Stdout:"))
                .with_section(move || stderr.trim().to_string().header("Stderr:"));
        }

        // docker push prints "digest: sha256:... size: ..." on the last relevant line.
        let stdout = String::from_utf8_lossy(&output.stdout);
        let digest = parse_docker_push_digest(&stdout)?;
        Ok(pinned_image_ref(image_ref, &digest))
    }
}

/// Generate a unique image reference with a timestamp + random suffix tag.
pub fn generate_image_ref(registry: &str) -> String {
    let ts = Utc::now().format("%Y%m%d-%H%M%S");
    let mut buf = [0u8; 2];
    getrandom::fill(&mut buf).expect("failed to get random bytes");
    let suffix = format!("{:02x}{:02x}", buf[0], buf[1]);
    format!(
        "{}/snouty-config:{}-{}",
        registry.trim_end_matches('/'),
        ts,
        suffix
    )
}

/// Check whether a binary is genuinely docker or podman-in-disguise.
///
/// Asks with `--version` rather than the `version` subcommand: podman-docker
/// installs `docker` as a shell script that execs podman, so `docker --version`
/// answers `podman version 4.9.3` — enough to tell them apart. The subcommand
/// would work too, but it queries the engine's daemon, which costs a round trip
/// on every call and blocks indefinitely when `DOCKER_HOST` points somewhere
/// that accepts connections but never answers (a firewalled remote, a wedged
/// dockerd). `--version` is answered by the client alone, so it stays instant
/// no matter what state the daemon is in.
pub fn is_podman_in_disguise(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| version_output_is_podman(&o.stdout))
}

/// Whether a `--version` banner came from podman. Split out from
/// [`is_podman_in_disguise`] so callers that already ran `--version` can reuse
/// their output instead of spawning the binary a second time.
fn version_output_is_podman(stdout: &[u8]) -> bool {
    String::from_utf8_lossy(stdout)
        .to_lowercase()
        .contains("podman")
}

/// Return the auto-detected global container runtime, preferring podman over docker.
///
/// The result is NOT cached, so make sure you hold on to the result if you need to use it more than once
///
/// Set `SNOUTY_CONTAINER_ENGINE=podman` or `=docker` to force a specific runtime.
pub fn runtime(settings: &Settings) -> Result<Box<dyn ContainerRuntime>> {
    // An explicit engine setting (from SNOUTY_CONTAINER_ENGINE or a settings
    // file) short-circuits auto-detection.
    if let Some(engine) = settings.container_engine() {
        return match engine {
            "podman" => Ok(Box::new(PodmanRuntime::new("podman"))),
            "docker" => Ok(Box::new(DockerRuntime::new("docker"))),
            other => Err(eyre!(
                "unsupported container engine '{other}': expected 'podman' or 'docker'"
            )),
        };
    }

    // Try podman first
    match Command::new("podman").arg("--version").output() {
        Ok(output) if output.status.success() => {
            return Ok(Box::new(PodmanRuntime::new("podman")));
        }
        Ok(_) => {} // podman found but failed, try docker
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {} // not installed
        Err(e) => return Err(eyre!("failed to check podman: {e}")),
    }

    // Fall back to docker
    match Command::new("docker").arg("--version").output() {
        Ok(output) if output.status.success() => {
            // The banner we just captured already says whether this `docker` is
            // really podman, so decide from it rather than asking again.
            if version_output_is_podman(&output.stdout) {
                log::warn!("podman not found as 'podman', but 'docker' is podman");
                return Ok(Box::new(PodmanRuntime::new("docker")));
            }
            log::warn!("podman not found, falling back to docker");
            Ok(Box::new(DockerRuntime::new("docker")))
        }
        Ok(_) => Err(eyre!(
            "'docker --version' failed; unable to find working container runtime"
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(eyre!("neither podman nor docker is installed"))
        }
        Err(e) => Err(eyre!("failed to check docker: {e}")),
    }
}

/// Warn the user which container engine snouty picked, but only when the pick
/// was genuinely ambiguous: several engines are installed and none was chosen
/// explicitly, so snouty had to break the tie. When only one engine exists
/// there's nothing to disambiguate, and an explicit `SNOUTY_CONTAINER_ENGINE` /
/// `container_engine` setting means the user already chose — both stay silent,
/// as does machine-readable (`json`) output. Prints to stderr (never stdout, so
/// it can't contaminate `--json`). The note points at the override so a
/// surprising pick is easy to fix.
pub fn warn_ambiguous_engine(settings: &Settings, rt: &dyn ContainerRuntime, json: bool) {
    if json || settings.container_engine().is_some() {
        return;
    }
    // Only worth a warning when more than one engine is installed — otherwise
    // the pick was forced, not a choice snouty made on the user's behalf.
    let available = available_engines();
    if available.len() < 2 {
        return;
    }
    // Report the real engine, not the binary name: for podman-in-disguise the
    // command is `docker` but the engine is podman.
    let engine = rt.engine_kind();
    let detected = available
        .iter()
        .map(|e| e.engine_kind())
        .collect::<Vec<_>>()
        .join(" and ");
    eprintln!(
        "Warning: {detected} detected; using {engine} \
         (override with SNOUTY_CONTAINER_ENGINE)."
    );
}

/// Return all container runtimes available on this machine.
/// Skips `docker` if it is actually podman in disguise.
pub fn available_engines() -> Vec<Box<dyn ContainerRuntime>> {
    let mut engines: Vec<Box<dyn ContainerRuntime>> = Vec::new();
    if Command::new("podman")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        engines.push(Box::new(PodmanRuntime::new("podman")));
    }
    // One `docker --version` answers both questions: whether docker is here at
    // all, and whether it is really podman wearing docker's name.
    if Command::new("docker")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| !version_output_is_podman(&o.stdout))
    {
        engines.push(Box::new(DockerRuntime::new("docker")));
    }
    engines
}

/// Build a pinned image reference (`name:tag@digest`) from a tagged ref and a
/// digest. The tag is kept as human-readable provenance — when inspecting an
/// Antithesis run, the reference still shows which tag the digest came from.
/// If the ref already carries a digest, it's replaced.
pub fn pinned_image_ref(image_ref: &str, digest: &str) -> String {
    match image_ref.rfind('@') {
        Some(at) => format!("{}@{}", &image_ref[..at], digest),
        None => format!("{image_ref}@{digest}"),
    }
}

/// Drop the `:tag` from a digest-pinned reference, leaving `name@digest`.
/// The digest fully identifies the image, and Antithesis's config-image
/// validator rejects a reference that carries both a tag and a digest.
fn digest_only_ref(image_ref: &str) -> String {
    match image_ref.rfind('@') {
        Some(at) => format!("{}{}", image_repo(image_ref), &image_ref[at..]),
        None => image_ref.to_string(),
    }
}

/// Result of [`ContainerRuntime::remote_manifest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteManifest {
    /// The registry doesn't serve this manifest, or it couldn't be checked.
    NotFound,
    /// A multi-platform manifest list. `has_amd64` reports whether it offers
    /// an amd64 entry for the platform to pull.
    List { has_amd64: bool },
    /// A single-platform manifest. Its architecture is the architecture of
    /// the local image carrying the same digest.
    Single,
}

/// Podman's spelling for a plain-HTTP registry; docker's is `--insecure`.
const PODMAN_INSECURE_FLAG: &str = "--tls-verify=false";

/// Whether `image_ref` names a local registry, which a test harness serves
/// over plain HTTP.
fn is_plain_http_registry(image_ref: &str) -> bool {
    image_ref.starts_with("localhost") || image_ref.starts_with("127.0.0.1")
}

/// Run `{runtime} manifest inspect` and classify the result. `insecure_flag`
/// is the runtime's spelling for plain-HTTP registries (`--insecure` for
/// docker, `--tls-verify=false` for podman), applied only to local registries.
fn remote_manifest_via_cli(runtime: &str, insecure_flag: &str, image_ref: &str) -> RemoteManifest {
    let mut args = vec!["manifest", "inspect"];
    if is_plain_http_registry(image_ref) {
        args.push(insecure_flag);
    }
    args.push(image_ref);
    match Command::new(runtime).args(&args).output() {
        Ok(output) if output.status.success() => classify_manifest_json(&output.stdout),
        _ => RemoteManifest::NotFound,
    }
}

/// Run `{runtime} pull` for [`AMD64_PLATFORM`]. `insecure_flag` is the
/// runtime's spelling for plain-HTTP registries, applied only to local
/// registries; docker has none, because it configures those on the daemon.
fn pull_via_cli(runtime: &str, insecure_flag: Option<&str>, image_ref: &str) -> Result<()> {
    let mut args = vec!["pull", "--platform", AMD64_PLATFORM];
    if let Some(flag) = insecure_flag
        && is_plain_http_registry(image_ref)
    {
        args.push(flag);
    }
    args.push(image_ref);
    let output = Command::new(runtime)
        .args(&args)
        .output()
        .wrap_err(format!("failed to run '{runtime} pull'"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!("'{runtime} pull {image_ref}' failed"))
            .with_section(move || stderr.trim().to_string().header("Stderr:"));
    }
    Ok(())
}

/// Classify `manifest inspect` JSON output: a `manifests` array marks a
/// manifest list (scan its platforms for amd64); any other valid manifest
/// JSON is a single-platform manifest.
fn classify_manifest_json(stdout: &[u8]) -> RemoteManifest {
    let Ok(doc) = serde_json::from_slice::<serde_json::Value>(stdout) else {
        return RemoteManifest::NotFound;
    };
    match doc.get("manifests").and_then(|m| m.as_array()) {
        Some(entries) => RemoteManifest::List {
            has_amd64: entries.iter().any(|entry| {
                entry
                    .get("platform")
                    .and_then(|p| p.get("architecture"))
                    .and_then(|a| a.as_str())
                    == Some("amd64")
            }),
        },
        None => RemoteManifest::Single,
    }
}

/// The repository part of an image reference: strips any `@digest` suffix and
/// any `:tag` (a colon counts as a tag separator only after the last `/`;
/// before it, it's a registry port). The cut is at the *first* colon after
/// the last `/` — the same spot as the last colon on any valid reference,
/// but stable on degenerate names with several colons (`a:b:c`), which keeps
/// this function and [`digest_only_ref`] idempotent (#209).
pub fn image_repo(image_ref: &str) -> &str {
    let no_digest = match image_ref.rfind('@') {
        Some(at) => &image_ref[..at],
        None => image_ref,
    };
    let name_start = no_digest.rfind('/').unwrap_or(0);
    match no_digest[name_start..].find(':') {
        Some(colon) => &no_digest[..name_start + colon],
        None => no_digest,
    }
}

/// The tag of an image reference, or `latest` when untagged.
pub fn image_ref_tag(image_ref: &str) -> &str {
    let no_digest = match image_ref.rfind('@') {
        Some(at) => &image_ref[..at],
        None => image_ref,
    };
    let repo = image_repo(image_ref);
    if no_digest.len() > repo.len() {
        &no_digest[repo.len() + 1..]
    } else {
        "latest"
    }
}

/// The rule a container runtime uses to read `ghcr.io/org/app` as hosted and
/// `org/app` as Docker Hub shorthand. An uppercase letter marks a host too,
/// because a path segment holds no uppercase.
fn is_registry_host(segment: &str) -> bool {
    segment.contains('.')
        || segment.contains(':')
        || segment == "localhost"
        || segment.chars().any(|c| c.is_ascii_uppercase())
}

/// The address `image` names, or `None` when `image` names no address and a
/// runtime reads it as Docker Hub shorthand.
pub fn registry_host(image: &str) -> Option<&str> {
    match image.split_once('/') {
        Some((host, _)) if is_registry_host(host) => Some(host),
        _ => None,
    }
}

/// The path segment that holds copies of images from other registries. It
/// carries no dot and no colon, so no runtime reads it as a registry host.
pub const MIRROR_PREFIX: &str = "snouty-mirror";

/// Whether `segment` satisfies the reference grammar for one path segment: an
/// alphanumeric opens and closes it, and `.`, `_`, and `-` join the rest.
fn is_path_segment(segment: &str) -> bool {
    let alnum = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    segment.starts_with(alnum)
        && segment.ends_with(alnum)
        && segment
            .chars()
            .all(|c| alnum(c) || matches!(c, '.' | '_' | '-'))
}

/// The name a copy of `image` takes below the tenant's repository:
/// `ghcr.io/org/app:v1` becomes `snouty-mirror/ghcr.io/org/app:v1`. A
/// reference that names no host is a copy of nothing and is returned
/// unchanged.
///
/// [`MIRROR_PREFIX`] opens the path, so the host that follows it is a path
/// segment rather than an address, and deleting the prefix restores the
/// original reference.
///
/// Two edits satisfy the reference grammar: a port takes `__` in place of its
/// colon, and an uppercase host folds to lowercase. Neither edit merges two
/// hosts that hold different bytes.
///
/// # Errors
///
/// An address that the two edits leave illegal, such as the IPv6 literal
/// `[::1]:5000`.
pub fn mirror_path(image: &str) -> Result<String> {
    let Some(host) = registry_host(image) else {
        return Ok(image.to_string());
    };
    let rest = &image[host.len() + 1..];
    let segment = host.replace(':', "__").to_ascii_lowercase();
    if !is_path_segment(&segment) {
        return Err(user_error(format!(
            "snouty copies `{image}` to `{MIRROR_PREFIX}/{segment}/{rest}` in your \
             repository, and `{segment}` is not a legal image path segment. Push the \
             image to your repository yourself, and name that copy in the compose file."
        )));
    }
    Ok(format!("{MIRROR_PREFIX}/{segment}/{rest}"))
}

/// `image` without the address of `registry`, which is the spelling this
/// machine uses to reach that registry — often a proxy or mirror alias that a
/// test run cannot resolve. The platform resolves the bare name that remains
/// against the tenant's own repository.
///
/// An image outside `registry` keeps its address, because nothing else names
/// those bytes.
pub fn strip_registry(image: &str, registry: &str) -> String {
    let prefix = format!("{}/", registry.trim_end_matches('/'));
    image.strip_prefix(&prefix).unwrap_or(image).to_string()
}

/// Expand Docker Hub shorthand so repository names compare reliably across
/// runtimes and reference styles: `nginx` → `docker.io/library/nginx`,
/// `user/app` → `docker.io/user/app`, `index.docker.io/...` → `docker.io/...`.
/// Repositories naming any other registry (first component with a dot, a
/// port, or `localhost`) pass through unchanged.
pub fn normalize_repo(repo: &str) -> String {
    let (registry, rest) = match registry_host(repo) {
        Some(host) => (canonical_host(host), &repo[host.len() + 1..]),
        None => ("docker.io", repo),
    };
    if registry == "docker.io" && !rest.contains('/') {
        format!("{registry}/library/{rest}")
    } else {
        format!("{registry}/{rest}")
    }
}

/// Docker Hub under one spelling: a runtime reads `index.docker.io` as
/// `docker.io`, so repository names compare by the latter.
fn canonical_host(host: &str) -> &str {
    if host == "index.docker.io" {
        "docker.io"
    } else {
        host
    }
}

/// A registry, or a repository path below one, that a test run cannot pull
/// from: the `private_registries` setting lists them. snouty pulls such an
/// image with this machine's credentials and copies the bytes into the tenant
/// repository, so a pin never names the private address.
///
/// The value is normalized the way [`normalize_repo`] reads an image: a name
/// with no registry host is a Docker Hub namespace, so `acme` and
/// `docker.io/acme` name one prefix. No `library/` is inserted, because a
/// prefix names a namespace, never one repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryPrefix(String);

impl std::str::FromStr for RegistryPrefix {
    type Err = color_eyre::Report;

    fn from_str(value: &str) -> Result<Self> {
        let value = value.trim().trim_end_matches('/');
        if value.is_empty() {
            return Err(user_error("a registry prefix must not be empty"));
        }
        if value.contains("://") {
            return Err(user_error(format!(
                "registry prefix `{value}` carries a URL scheme; write the host and path \
                 only, such as `ghcr.io/acme`"
            )));
        }
        if image_repo(value) != value {
            return Err(user_error(format!(
                "registry prefix `{value}` names a tag or digest; write the registry or \
                 repository path only, such as `ghcr.io/acme`"
            )));
        }
        let (host, path) = match registry_host(value) {
            Some(host) => (host, &value[host.len() + 1..]),
            None if is_registry_host(value) => (value, ""),
            None => ("docker.io", value),
        };
        let host = canonical_host(host);
        Ok(Self(if path.is_empty() {
            host.to_string()
        } else {
            format!("{host}/{path}")
        }))
    }
}

impl std::fmt::Display for RegistryPrefix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl RegistryPrefix {
    /// Whether `image`'s repository is this prefix or lies below it.
    pub fn matches(&self, image: &str) -> bool {
        normalize_repo(image_repo(image))
            .strip_prefix(&self.0)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    }
}

/// Whether any prefix in `private_registries` serves `image`.
pub fn is_private_image(image: &str, private_registries: &[RegistryPrefix]) -> bool {
    private_registries
        .iter()
        .any(|prefix| prefix.matches(image))
}

/// All digests the local store records for `repo` among `repo_digests`
/// entries, comparing normalized repository names. Multiple entries per repo
/// are common — a pull records both the per-arch manifest digest and the
/// manifest-list digest.
pub fn digests_for_repo(repo: &str, repo_digests: &[String]) -> Vec<String> {
    let want = normalize_repo(repo);
    repo_digests
        .iter()
        .filter_map(|entry| {
            let (entry_repo, digest) = entry.rsplit_once('@')?;
            (digest.starts_with("sha256:") && normalize_repo(entry_repo) == want)
                .then(|| digest.to_string())
        })
        .collect()
}

/// Parse the image digest from `docker push` stdout.
///
/// Docker prints a line like: `latest: digest: sha256:abc123... size: 1234`
fn parse_docker_push_digest(stdout: &str) -> Result<String> {
    let mut found: Option<String> = None;
    for line in stdout.lines() {
        if let Some(rest) = line.find("digest: ").map(|i| &line[i + 8..]) {
            let digest = rest.split_whitespace().next().unwrap_or("");
            if digest.starts_with("sha256:") {
                if found.is_some() {
                    return Err(eyre!(
                        "ambiguous: multiple digests found in 'docker push' output"
                    ))
                    .with_section(|| stdout.trim().to_string().header("Stdout:"));
                }
                found = Some(digest.to_string());
            }
        }
    }
    found.ok_or_else(|| {
        eyre!("failed to parse digest from 'docker push' output")
            .section(stdout.trim().to_string().header("Stdout:"))
    })
}

fn image_exists_from_inspect_output(
    runtime: &str,
    image_ref: &str,
    output: std::process::Output,
) -> Result<bool> {
    if output.status.success() {
        return Ok(true);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if image_inspect_reports_missing_image(&stderr) {
        return Ok(false);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Err(eyre!("'{runtime} image inspect {image_ref}' failed"))
        .with_section(move || stdout.trim().to_string().header("Stdout:"))
        .with_section(move || stderr.trim().to_string().header("Stderr:"))
}

fn image_inspect_reports_missing_image(stderr: &str) -> bool {
    let stderr = stderr.trim().to_ascii_lowercase();
    stderr.contains("no such image")
        || stderr.contains("no such object")
        || stderr.contains("image not known")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutils::{OCIRegistry, require_runtimes};
    use hegel::generators::{self, Generator};

    use std::os::unix::process::ExitStatusExt;

    /// A text generator whose output never contains `@` — for digest values,
    /// which never carry an `@` in practice (`sha256:...`), so the
    /// re-pinning/idempotence reasoning holds.
    fn no_at() -> impl Generator<String> {
        generators::text().filter(|s: &String| !s.contains('@'))
    }

    /// Expanding a repository to its canonical `registry/path` form is stable:
    /// normalizing an already-normalized repo is a no-op. Run over arbitrary
    /// text so odd inputs (empty, leading slash, multi-byte) are covered too.
    #[hegel::test]
    fn normalize_repo_is_idempotent(tc: hegel::TestCase) {
        let repo = tc.draw(generators::text());
        let once = normalize_repo(&repo);
        let twice = normalize_repo(&once);
        assert_eq!(once, twice);
    }

    /// A prefix reads the same way twice: parsing the spelling a prefix
    /// prints returns that prefix. Run over arbitrary text so every rejected
    /// spelling is exercised too.
    #[hegel::test]
    fn registry_prefix_parse_is_idempotent(tc: hegel::TestCase) {
        let text = tc.draw(generators::text());
        let Ok(once) = text.parse::<RegistryPrefix>() else {
            return;
        };
        let twice: RegistryPrefix = once.to_string().parse().expect("a prefix prints a prefix");
        assert_eq!(once, twice);
    }

    /// Every image whose path continues below a prefix matches it, whatever
    /// the path segment and tag spell.
    #[hegel::test]
    fn registry_prefix_matches_every_image_below_it(tc: hegel::TestCase) {
        let Ok(prefix) = tc.draw(generators::text()).parse::<RegistryPrefix>() else {
            return;
        };
        let segment = tc.draw(generators::text());
        let tag = tc.draw(generators::text());
        let image = format!("{prefix}/{segment}:{tag}");
        assert!(prefix.matches(&image), "{prefix} does not match {image}");
    }

    /// A copy always hides the address it came from — that is the whole
    /// reason the copy exists — and copying an address-free path changes
    /// nothing, so a second launch reuses the first launch's path. Run over
    /// arbitrary text because `mirror_path` slices on the byte offset of the
    /// first `/`, which must never split a multi-byte character.
    #[hegel::test]
    fn mirror_path_hides_the_address_and_is_idempotent(tc: hegel::TestCase) {
        let image = tc.draw(generators::text());
        let Ok(once) = mirror_path(&image) else {
            return;
        };
        assert_eq!(registry_host(&once), None, "{image} copied to {once}");
        let twice = mirror_path(&once).expect("a copy names no address to rewrite");
        assert_eq!(once, twice);
    }

    /// The copy renames no image: everything below the address survives the
    /// copy byte for byte, so deleting `MIRROR_PREFIX` and the address
    /// segment names the source repository again.
    #[hegel::test]
    fn mirror_path_keeps_the_repository_path(tc: hegel::TestCase) {
        let image = tc.draw(generators::text());
        let Ok(copy) = mirror_path(&image) else {
            return;
        };
        let Some(host) = registry_host(&image) else {
            assert_eq!(copy, image);
            return;
        };
        let rest = copy
            .strip_prefix(&format!("{MIRROR_PREFIX}/"))
            .expect("a copy opens with the prefix");
        let (_, path) = rest.split_once('/').expect("the address is one segment");
        assert_eq!(path, &image[host.len() + 1..]);
    }

    /// `image_repo` and `image_ref_tag` slice on byte offsets found with
    /// `rfind`; on arbitrary (multi-byte) input they must never panic on a
    /// non-char-boundary, and the repo must always be a prefix of the input.
    #[hegel::test]
    fn image_repo_is_a_prefix_and_never_panics(tc: hegel::TestCase) {
        let image_ref = tc.draw(generators::text());
        let repo = image_repo(&image_ref);
        assert!(
            image_ref.starts_with(repo),
            "repo {repo:?} is not a prefix of {image_ref:?}"
        );
        // Just exercising the slicing — the assertion is "it returned".
        let _ = image_ref_tag(&image_ref);
    }

    /// Re-pinning replaces the digest rather than appending a second one:
    /// pinning to `d1` then `d2` is the same as pinning to `d2` directly, and
    /// the result always ends with `@{d2}`.
    #[hegel::test]
    fn pinned_image_ref_replaces_digest(tc: hegel::TestCase) {
        let image_ref = tc.draw(no_at());
        let d1 = tc.draw(no_at());
        let d2 = tc.draw(no_at());
        let repinned = pinned_image_ref(&pinned_image_ref(&image_ref, &d1), &d2);
        assert_eq!(repinned, pinned_image_ref(&image_ref, &d2));
        assert!(repinned.ends_with(&format!("@{d2}")));
    }

    /// Stripping the tag from a digest-pinned reference is stable: a reference
    /// already in `name@digest` form is left unchanged by a second pass.
    #[hegel::test]
    fn digest_only_ref_is_idempotent(tc: hegel::TestCase) {
        let image_ref = tc.draw(generators::text());
        let once = digest_only_ref(&image_ref);
        assert_eq!(digest_only_ref(&once), once);
    }

    /// Classifying `manifest inspect` output must never panic on arbitrary
    /// bytes — malformed JSON classifies as `NotFound`, never a crash.
    #[hegel::test]
    fn classify_manifest_json_never_panics(tc: hegel::TestCase) {
        let bytes = tc.draw(generators::binary());
        let _ = classify_manifest_json(&bytes);
    }

    /// Parsing a `docker push` digest line must never panic on arbitrary text —
    /// it returns an error when no `sha256:` digest is present, never a crash.
    #[hegel::test]
    fn parse_docker_push_digest_never_panics(tc: hegel::TestCase) {
        let stdout = tc.draw(generators::text());
        let _ = parse_docker_push_digest(&stdout);
    }

    #[tokio::test]
    async fn build_and_push_to_mock_registry() {
        let runtimes = require_runtimes();
        if runtimes.is_empty() {
            return;
        }

        for rt in &runtimes {
            eprintln!("testing with runtime: {}", rt.name());
            let registry = match OCIRegistry::start(rt.as_ref()) {
                Some(r) => r,
                None => continue,
            };
            let addr = registry.host_port();

            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join("docker-compose.yaml"),
                "services:\n  app:\n    image: test:latest\n",
            )
            .unwrap();

            // Unique per runtime so iterations sharing a registry don't collide.
            let repo = crate::testutils::unique_image_prefix(&format!("cfg-{}", rt.name()));
            let image_ref = format!("{addr}/{repo}/snouty-config:test");
            rt.build_and_push_config_image(dir.path(), &image_ref)
                .unwrap_or_else(|e| panic!("{}: {e:?}", rt.name()));

            // The config image must always be amd64: Antithesis does not support
            // arm64, and this build runs unchanged on arm hosts.
            let arch = rt
                .image_architecture(&image_ref)
                .unwrap_or_else(|e| panic!("{}: {e:?}", rt.name()));
            assert_eq!(
                arch,
                Architecture::Amd64,
                "{}: config image must be amd64",
                rt.name()
            );

            // Clean up the local image.
            let _ = Command::new(rt.name()).args(["rmi", &image_ref]).output();
        }
    }

    #[test]
    fn stderr_indicates_missing_source_matches_docker_and_podman() {
        assert!(stderr_indicates_missing_source(
            "Error response from daemon: Could not find the file /opt/x in container abc"
        ));
        assert!(stderr_indicates_missing_source(
            "Error: \"/opt/antithesis/test/v1\": no such file or directory"
        ));
        // Podman 4.x phrasings.
        assert!(stderr_indicates_missing_source(
            "Error: \"/opt/antithesis/test/v1\" does not exist in container"
        ));
        assert!(stderr_indicates_missing_source(
            "Error: /opt/antithesis/test/v1 could not be found on container abc"
        ));
        // Real cp errors must not be misclassified.
        assert!(!stderr_indicates_missing_source("Error: permission denied"));
        // The loose "no such file" (without "or directory") is deliberately
        // excluded — it matches destination-side errors and shadows real
        // failures.
        assert!(!stderr_indicates_missing_source(
            "Error: writing to destination: no such file"
        ));
    }

    #[test]
    fn generate_image_ref_format() {
        let image_ref = generate_image_ref("us-central1-docker.pkg.dev/proj/repo");
        assert!(
            image_ref.starts_with("us-central1-docker.pkg.dev/proj/repo/snouty-config:"),
            "got: {image_ref}"
        );
        // Tag should be YYYYMMDD-HHMMSS-XXXX format (20 chars)
        let tag = image_ref.rsplit(':').next().unwrap();
        assert!(
            tag.len() == 20 && tag.chars().nth(8) == Some('-') && tag.chars().nth(15) == Some('-'),
            "tag format should be YYYYMMDD-HHMMSS-XXXX, got: {tag}"
        );
    }

    #[test]
    fn generate_image_ref_strips_trailing_slash() {
        let image_ref = generate_image_ref("registry.example.com/repo/");
        assert!(
            image_ref.starts_with("registry.example.com/repo/snouty-config:"),
            "got: {image_ref}"
        );
    }

    // Real `--version` banners: podman-docker installs `docker` as a script
    // that execs podman, so the disguise announces itself here — no need for
    // the daemon-querying `version` subcommand.
    #[test]
    fn version_output_identifies_podman_in_disguise() {
        // podman-docker 4.9.3's `docker --version`, and podman's own.
        assert!(version_output_is_podman(b"podman version 4.9.3\n"));
        // The subcommand banner, in case a caller ever feeds it in.
        assert!(version_output_is_podman(
            b"Client:       Podman Engine\nVersion:      4.9.3\n"
        ));

        // Genuine docker, standalone and Docker Desktop.
        assert!(!version_output_is_podman(
            b"Docker version 29.1.3, build 29.1.3-0ubuntu3~24.04.2\n"
        ));
        assert!(!version_output_is_podman(
            b"Docker version 27.4.0, build bde2b89\n"
        ));

        // Nothing to read is not podman, and invalid UTF-8 must not panic.
        assert!(!version_output_is_podman(b""));
        assert!(!version_output_is_podman(&[0xff, 0xfe, 0xfd]));
    }

    #[test]
    fn registry_prefix_normalizes_like_an_image_reference() {
        let parse = |text: &str| text.parse::<RegistryPrefix>().unwrap().to_string();
        assert_eq!(parse("ghcr.io"), "ghcr.io");
        assert_eq!(parse("ghcr.io/acme/"), "ghcr.io/acme");
        assert_eq!(parse(" localhost:5000/team "), "localhost:5000/team");
        // A name with no host is a Docker Hub namespace, spelled one way.
        assert_eq!(parse("acme"), "docker.io/acme");
        assert_eq!(parse("docker.io/acme"), "docker.io/acme");
        assert_eq!(parse("index.docker.io/acme"), "docker.io/acme");
        assert_eq!(parse("index.docker.io"), "docker.io");
    }

    #[test]
    fn registry_prefix_rejects_a_scheme_a_tag_and_a_digest() {
        for (text, needle) in [
            ("", "must not be empty"),
            ("  ", "must not be empty"),
            ("https://ghcr.io/acme", "URL scheme"),
            ("ghcr.io/acme/app:v1", "tag or digest"),
            ("ghcr.io/acme/app@sha256:abc", "tag or digest"),
        ] {
            let err = text.parse::<RegistryPrefix>().unwrap_err().to_string();
            assert!(err.contains(needle), "`{text}`: {err}");
        }
    }

    #[test]
    fn registry_prefix_matches_the_namespace_and_nothing_beside_it() {
        let acme: RegistryPrefix = "ghcr.io/acme".parse().unwrap();
        assert!(acme.matches("ghcr.io/acme/app:v1"));
        assert!(acme.matches("ghcr.io/acme/team/app@sha256:abc"));
        assert!(acme.matches("ghcr.io/acme"));
        // A sibling namespace shares the spelling up to the slash only.
        assert!(!acme.matches("ghcr.io/acmeco/app:v1"));
        assert!(!acme.matches("docker.io/acme/app:v1"));

        // A whole registry.
        let ghcr: RegistryPrefix = "ghcr.io".parse().unwrap();
        assert!(ghcr.matches("ghcr.io/anyone/app:v1"));
        assert!(!ghcr.matches("quay.io/anyone/app:v1"));
        assert!(!ghcr.matches("app:v1"));

        // Docker Hub shorthand on either side.
        let hub: RegistryPrefix = "acme".parse().unwrap();
        assert!(hub.matches("acme/app:v1"));
        assert!(hub.matches("index.docker.io/acme/app:v1"));
        // `nginx` is the namespace, not the official `library/nginx` image.
        let nginx: RegistryPrefix = "nginx".parse().unwrap();
        assert!(!nginx.matches("nginx:1.25"));
    }

    #[test]
    fn normalize_repo_expands_docker_hub_shorthand() {
        assert_eq!(normalize_repo("nginx"), "docker.io/library/nginx");
        assert_eq!(normalize_repo("user/app"), "docker.io/user/app");
        assert_eq!(normalize_repo("docker.io/nginx"), "docker.io/library/nginx");
        assert_eq!(
            normalize_repo("index.docker.io/library/nginx"),
            "docker.io/library/nginx"
        );
        assert_eq!(normalize_repo("localhost:5000/app"), "localhost:5000/app");
        assert_eq!(normalize_repo("ghcr.io/org/app"), "ghcr.io/org/app");
    }

    #[test]
    fn mirror_path_keeps_the_source_registry_readable() {
        let mirror = |image: &str| mirror_path(image).expect("legal address");
        assert_eq!(
            mirror("ghcr.io/org/app:v1"),
            "snouty-mirror/ghcr.io/org/app:v1"
        );
        // A port cannot keep its colon inside a path, so it takes `__`.
        assert_eq!(
            mirror("localhost:5000/app:v1"),
            "snouty-mirror/localhost__5000/app:v1"
        );
        assert_eq!(mirror("localhost/app:v1"), "snouty-mirror/localhost/app:v1");
        // A path segment cannot carry uppercase, and the fold names the same
        // registry. A runtime reads `MyRegistry` as an address for the same
        // reason: only an address carries uppercase.
        assert_eq!(
            mirror("Registry.Example.com/org/app:v1"),
            "snouty-mirror/registry.example.com/org/app:v1"
        );
        assert_eq!(
            mirror("MyRegistry/app:v1"),
            "snouty-mirror/myregistry/app:v1"
        );
        assert_eq!(mirror("myapp:latest"), "myapp:latest");
        assert_eq!(mirror("org/app:v1"), "org/app:v1");
        assert_eq!(mirror("app:v1@sha256:abc"), "app:v1@sha256:abc");
    }

    /// An IPv6 literal address keeps its brackets after the two edits, and no
    /// path segment carries a bracket. snouty says so instead of handing the
    /// runtime a reference it rejects.
    #[test]
    fn mirror_path_refuses_an_address_with_no_legal_spelling() {
        let error = mirror_path("[::1]:5000/app:v1").expect_err("brackets are illegal");
        assert!(
            error.to_string().contains("not a legal image path segment"),
            "{error}"
        );
    }

    /// `generate_image_ref` spells the config image, `strip_registry` takes the
    /// address back off, and the platform never sees the address it was pushed
    /// to. The two have to agree on the separator for that to hold.
    #[test]
    fn a_generated_config_ref_strips_back_to_a_bare_name() {
        for registry in [
            "us-central1-docker.pkg.dev/proj/repo",
            "us-central1-docker.pkg.dev/proj/repo/",
        ] {
            let bare = strip_registry(&generate_image_ref(registry), registry);
            assert!(bare.starts_with("snouty-config:"), "got: {bare}");
            assert!(!bare.contains('/'), "got: {bare}");
        }
    }

    #[test]
    fn strip_registry_drops_only_our_own_address() {
        let registry = "us-central1-docker.pkg.dev/proj/repo";
        assert_eq!(
            strip_registry(
                "us-central1-docker.pkg.dev/proj/repo/snouty-config@sha256:abc",
                registry
            ),
            "snouty-config@sha256:abc"
        );
        assert_eq!(
            strip_registry("reg.example.com/repo/app:v1", "reg.example.com/repo/"),
            "app:v1"
        );
        assert_eq!(
            strip_registry("ghcr.io/org/app:v1", registry),
            "ghcr.io/org/app:v1"
        );
        // A prefix that matches only as text, not as a whole path segment.
        assert_eq!(
            strip_registry("reg.example.com/repository/app:v1", "reg.example.com/repo"),
            "reg.example.com/repository/app:v1"
        );
    }

    #[test]
    fn image_repo_and_tag_split_references() {
        assert_eq!(image_repo("redis:7"), "redis");
        assert_eq!(image_ref_tag("redis:7"), "7");
        assert_eq!(image_repo("nginx"), "nginx");
        assert_eq!(image_ref_tag("nginx"), "latest");
        assert_eq!(image_repo("localhost:5000/app:v1"), "localhost:5000/app");
        assert_eq!(image_ref_tag("localhost:5000/app:v1"), "v1");
        assert_eq!(image_repo("localhost:5000/app"), "localhost:5000/app");
        assert_eq!(image_ref_tag("localhost:5000/app"), "latest");
        assert_eq!(
            image_repo("ghcr.io/org/app:v2@sha256:abc"),
            "ghcr.io/org/app"
        );
        assert_eq!(image_ref_tag("ghcr.io/org/app:v2@sha256:abc"), "v2");
    }

    /// Degenerate names with several colons strip to a fixed point in one
    /// pass — the hegel counterexample from #209.
    #[test]
    fn image_repo_is_stable_on_degenerate_refs() {
        assert_eq!(image_repo("::@"), "");
        assert_eq!(digest_only_ref("::@"), "@");
        assert_eq!(digest_only_ref("@"), "@");
        assert_eq!(image_repo("a:b:c"), "a");
        assert_eq!(digest_only_ref("a:b:c@sha256:abc"), "a@sha256:abc");
    }

    #[test]
    fn digests_for_repo_matches_normalized_repos() {
        let entries = vec![
            "redis@sha256:child".to_string(),
            "docker.io/library/redis@sha256:list".to_string(),
            "mirror.example.com/redis@sha256:other".to_string(),
        ];
        // Both Hub spellings match each other; the mirror doesn't.
        assert_eq!(
            digests_for_repo("docker.io/library/redis", &entries),
            vec!["sha256:child".to_string(), "sha256:list".to_string()]
        );
        assert_eq!(
            digests_for_repo("mirror.example.com/redis", &entries),
            vec!["sha256:other".to_string()]
        );
        assert!(digests_for_repo("ghcr.io/redis", &entries).is_empty());
    }

    #[test]
    fn classify_manifest_json_shapes() {
        let list_amd64 = br#"{"manifests": [{"platform": {"architecture": "arm64"}}, {"platform": {"architecture": "amd64"}}]}"#;
        assert_eq!(
            classify_manifest_json(list_amd64),
            RemoteManifest::List { has_amd64: true }
        );
        let list_arm = br#"{"manifests": [{"platform": {"architecture": "arm64"}}]}"#;
        assert_eq!(
            classify_manifest_json(list_arm),
            RemoteManifest::List { has_amd64: false }
        );
        let single = br#"{"schemaVersion": 2, "config": {}, "layers": []}"#;
        assert_eq!(classify_manifest_json(single), RemoteManifest::Single);
        assert_eq!(
            classify_manifest_json(b"not json"),
            RemoteManifest::NotFound
        );
    }

    #[test]
    fn parse_docker_push_digest_typical() {
        let stdout = "\
The push refers to repository [registry.example.com/myimage]
5f70bf18a086: Layer already exists
latest: digest: sha256:abc123def456 size: 1234
";
        let digest = parse_docker_push_digest(stdout).unwrap();
        assert_eq!(digest, "sha256:abc123def456");
    }

    #[test]
    fn parse_docker_push_digest_no_digest() {
        let stdout = "The push refers to repository [registry.example.com/myimage]\n";
        let result = parse_docker_push_digest(stdout);
        assert!(result.is_err());
        let err = format!("{:?}", result.unwrap_err());
        assert!(err.contains("failed to parse digest"), "got: {err}");
    }

    #[test]
    fn parse_docker_push_digest_ambiguous() {
        let stdout = "\
tag1: digest: sha256:aaa111 size: 100
tag2: digest: sha256:bbb222 size: 200
";
        let result = parse_docker_push_digest(stdout);
        assert!(result.is_err());
        let err = format!("{:?}", result.unwrap_err());
        assert!(err.contains("ambiguous"), "got: {err}");
    }

    #[test]
    fn parse_docker_push_digest_empty() {
        let result = parse_docker_push_digest("");
        assert!(result.is_err());
    }

    #[test]
    fn pinned_image_ref_keeps_tag() {
        assert_eq!(
            pinned_image_ref("example.com/foo/image:v1", "sha256:abc123"),
            "example.com/foo/image:v1@sha256:abc123"
        );
    }

    #[test]
    fn pinned_image_ref_without_tag() {
        assert_eq!(
            pinned_image_ref("example.com/foo/image", "sha256:abc123"),
            "example.com/foo/image@sha256:abc123"
        );
    }

    #[test]
    fn pinned_image_ref_with_port_keeps_tag() {
        assert_eq!(
            pinned_image_ref("localhost:5000/image:latest", "sha256:abc123"),
            "localhost:5000/image:latest@sha256:abc123"
        );
    }

    #[test]
    fn pinned_image_ref_port_no_tag() {
        assert_eq!(
            pinned_image_ref("localhost:5000/image", "sha256:abc123"),
            "localhost:5000/image@sha256:abc123"
        );
    }

    #[test]
    fn pinned_image_ref_host_port_nested_path_keeps_tag() {
        assert_eq!(
            pinned_image_ref("myregistry:5000/org/repo/image:v2", "sha256:abc123"),
            "myregistry:5000/org/repo/image:v2@sha256:abc123"
        );
    }

    #[test]
    fn pinned_image_ref_host_port_nested_no_tag() {
        assert_eq!(
            pinned_image_ref("myregistry:5000/org/repo/image", "sha256:abc123"),
            "myregistry:5000/org/repo/image@sha256:abc123"
        );
    }

    #[test]
    fn pinned_image_ref_replaces_existing_digest() {
        assert_eq!(
            pinned_image_ref("registry.example.com/team/service@sha256:old", "sha256:new"),
            "registry.example.com/team/service@sha256:new"
        );
    }

    #[test]
    fn digest_only_ref_strips_tag() {
        assert_eq!(
            digest_only_ref("example.com/foo/snouty-config:20260615-abcd@sha256:abc123"),
            "example.com/foo/snouty-config@sha256:abc123"
        );
    }

    #[test]
    fn digest_only_ref_with_port_strips_tag() {
        assert_eq!(
            digest_only_ref("localhost:5000/snouty-config:latest@sha256:abc123"),
            "localhost:5000/snouty-config@sha256:abc123"
        );
    }

    #[test]
    fn digest_only_ref_already_digest_only() {
        assert_eq!(
            digest_only_ref("example.com/foo/snouty-config@sha256:abc123"),
            "example.com/foo/snouty-config@sha256:abc123"
        );
    }

    #[test]
    fn digest_only_ref_no_digest_unchanged() {
        assert_eq!(
            digest_only_ref("example.com/foo/snouty-config:v1"),
            "example.com/foo/snouty-config:v1"
        );
    }

    #[test]
    fn image_exists_from_inspect_output_accepts_success() {
        let result = image_exists_from_inspect_output(
            "docker",
            "app:latest",
            std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
        )
        .unwrap();

        assert!(result);
    }

    #[test]
    fn image_exists_from_inspect_output_reports_missing_image() {
        let result = image_exists_from_inspect_output(
            "docker",
            "missing:latest",
            std::process::Output {
                status: std::process::ExitStatus::from_raw(1 << 8),
                stdout: Vec::new(),
                stderr: b"Error response from daemon: No such image: missing:latest".to_vec(),
            },
        )
        .unwrap();

        assert!(!result);
    }

    #[test]
    fn image_exists_from_inspect_output_surfaces_runtime_errors() {
        let err = image_exists_from_inspect_output(
            "docker",
            "app:latest",
            std::process::Output {
                status: std::process::ExitStatus::from_raw(1 << 8),
                stdout: Vec::new(),
                stderr: b"Cannot connect to the Docker daemon at unix:///var/run/docker.sock"
                    .to_vec(),
            },
        )
        .unwrap_err();

        let debug = format!("{err:?}");
        assert!(
            debug.contains("Cannot connect to the Docker daemon"),
            "expected daemon error details, got: {debug}"
        );
    }
}
