use std::io::{self, ErrorKind, IsTerminal, Read};
use std::process::Command;

use clap::{CommandFactory, Parser};
use clap_complete::Shell;
use log::{debug, info};

use color_eyre::Section;
use color_eyre::eyre::{Context, Result};
use semver::Version;
use snouty::OutputOptions;
use snouty::api::AntithesisApi;
use snouty::auth::initialize_credential_store;
use snouty::cli::{
    Cli, Commands, DebugArgs, LaunchArgs, UpdateArgs, UpdateChannel, gated_command_error,
};
use snouty::compose;
use snouty::config;
use snouty::container;
use snouty::docs;
use snouty::error::user_error;
use snouty::features;
use snouty::login::cmd_login;
use snouty::params::{
    ANT_CONFIG_IMAGE, ANT_DEBUGGING_INPUT_HASH, ANT_DEBUGGING_RUN_ID, ANT_DEBUGGING_SESSION_ID,
    ANT_DEBUGGING_VTIME, ANT_DESCRIPTION, ANT_DURATION, ANT_EVENT_DESCRIPTION,
    ANT_FILTER_LOGS_MATCHING, ANT_IS_EPHEMERAL, ANT_REPORT_RECIPIENTS, ANT_SOURCE, ANT_TEST_NAME,
    Params,
};
use snouty::settings::Settings;
use snouty::validate;

fn read_stdin() -> Result<String> {
    let mut buf = String::new();
    io::stdin()
        .read_to_string(&mut buf)
        .wrap_err("invalid arguments: failed to read stdin")?;
    let buf = buf.trim().to_string();
    Ok(buf)
}

fn get_stdin_params() -> Result<Params> {
    let input = read_stdin()?;
    debug!("parsing input as JSON");
    let value: serde_json::Value =
        json5::from_str(&input).wrap_err("invalid arguments: invalid JSON")?;
    Params::from_json(&value)
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Drop the "Backtrace omitted. Run with RUST_BACKTRACE=1…" footer: it's noise
    // on every error, and outright misleading on user errors built with
    // `suppress_backtrace` (where RUST_BACKTRACE does nothing). A genuine fault
    // still prints its backtrace when RUST_BACKTRACE is set.
    //
    // Color the report only on a real terminal — piped/redirected stderr (and the
    // test/gallery captures) gets plain text, not ANSI escapes.
    let theme = if std::io::stderr().is_terminal() {
        color_eyre::config::Theme::dark()
    } else {
        color_eyre::config::Theme::new()
    };
    color_eyre::config::HookBuilder::default()
        .theme(theme)
        .display_env_section(false)
        .install()
        .unwrap();
    env_logger::Builder::from_default_env()
        .format(|buf, record| {
            use std::io::Write;
            writeln!(buf, "{}", record.args())
        })
        .init();
    if let Err(report) = run(Cli::parse()).await {
        // One rendering for every error: `render_report` collapses the chain
        // index for single errors and wraps overlong prose, both printing
        // concerns that belong here rather than in the messages. User-facing
        // failures are built with `user_error`/4xx `suppress_backtrace`, so they
        // print message + any `.note()`/`.suggestion()` hints with no backtrace;
        // genuine internal faults keep theirs.
        let rendered = snouty::error::render_report(&report);
        eprintln!("{}", snouty::wrap_if_tty(&rendered));
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let Cli {
        json,
        verbose,
        settings: settings_path,
        profile,
        command,
    } = cli;
    if json && let Some(name) = json_unaware_command_name(&command) {
        eprintln!("warning: --json has no effect for `snouty {name}`");
    }
    // The global output flags travel together from here on; every command
    // takes them as one value instead of a swappable positional bool pair.
    let output = OutputOptions { json, verbose };

    // A gated command hides itself as the parser is built (see the `hide`
    // attribute on `RunsCommands::Exec`), but a hidden subcommand is still
    // callable — so refuse it here too.
    if let Some(report) = gated_command_error(&command, &features::enabled()) {
        return Err(report);
    }

    if let Err(err) = initialize_credential_store() {
        eprintln!("warning: Could not initialize system keychain credential storage: {err:?}");
    }

    // Resolve all settings up front. A broken/unreadable settings file fails
    // here, cleanly and once, before any command runs — including commands that
    // don't read settings. That keeps dispatch a single flat match.
    let settings = Settings::resolve(settings_path, profile.clone());
    let result = match command {
        Commands::Completions { shell } => cmd_completions(shell),
        Commands::Version => {
            // SNOUTY_VERSION (set by build.rs) is the crate version plus the
            // build's git sha when known — the same string clap's --version
            // prints, so the two stay consistent.
            println!("snouty {}", env!("SNOUTY_VERSION"));
            Ok(())
        }
        Commands::Update(args) => cmd_update(args, &settings?),
        Commands::Docs { offline, command } => docs::cmd_docs(command, offline, output).await,
        Commands::Login { tenant, repository } => {
            cmd_login(tenant, repository, profile.as_deref(), &settings?).await
        }
        Commands::Launch(args) => {
            info!("launching test with webhook: {}", args.webhook);
            cmd_launch(args, &settings?, output).await
        }
        Commands::Run(args) => {
            eprintln!("warning: `snouty run` is deprecated, use `snouty launch` instead");
            info!("launching test with webhook: {}", args.webhook);
            cmd_launch(args, &settings?, output).await
        }
        Commands::Runs { command } => snouty::runs::cmd_runs(command, &settings?, output).await,
        Commands::Debug(args) => {
            info!("starting debug session");
            cmd_debug(args, &settings?, output).await
        }
        Commands::Validate(args) => validate::cmd_validate(args, &settings?).await,
        Commands::Doctor(args) => {
            snouty::doctor::cmd_doctor(&settings?, output, args.offline).await
        }
    };

    suppress_broken_pipe(result)
}

/// When our output is piped into something that exits early (e.g. `snouty
/// runs list | head`), writes to stdout fail with BrokenPipe. That's a
/// normal way for a pipeline to end, not an error — exit quietly. Network
/// errors don't take this path: they surface as reqwest errors, whose
/// underlying io::Error sits in reqwest's source chain and is not
/// downcastable from the report (see the tests below).
fn suppress_broken_pipe(result: Result<()>) -> Result<()> {
    match result {
        Err(err)
            if err
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == ErrorKind::BrokenPipe) =>
        {
            Ok(())
        }
        other => other,
    }
}

fn json_unaware_command_name(command: &Commands) -> Option<&'static str> {
    match command {
        Commands::Launch(_)
        | Commands::Run(_)
        | Commands::Runs { .. }
        | Commands::Docs { .. }
        | Commands::Debug { .. }
        | Commands::Doctor(_) => None,
        Commands::Validate(_) => Some("validate"),
        Commands::Completions { .. } => Some("completions"),
        Commands::Version => Some("version"),
        Commands::Update(_) => Some("update"),
        Commands::Login { .. } => Some("login"),
    }
}

async fn cmd_launch(
    args: LaunchArgs,
    settings: &Settings,
    OutputOptions { json, verbose }: OutputOptions,
) -> Result<()> {
    let mut params = Params::new();

    if let Some(test_name) = args.test_name {
        params.insert(ANT_TEST_NAME, test_name);
    }
    if let Some(description) = args.description {
        params.insert(ANT_DESCRIPTION, description);
    }
    if let Some(duration) = args.duration {
        // The API wants a bare minute count, not the human `1h30m` Display form.
        params.insert(ANT_DURATION, duration.minutes().to_string());
    }
    let has_source = if let Some(source) = args.source {
        params.insert(ANT_SOURCE, source);
        true
    } else {
        false
    };
    if args.ephemeral {
        params.insert(ANT_IS_EPHEMERAL, "true");
    }
    if let Some(recipients) = args.recipients {
        params.insert(ANT_REPORT_RECIPIENTS, recipients);
    }

    if let Some(config_image) = args.config_image {
        params.insert(ANT_CONFIG_IMAGE, config_image);
    }

    if let Some(filter_logs_matching) = args.filter_logs_matching {
        params.insert(ANT_FILTER_LOGS_MATCHING, filter_logs_matching);
    }

    let config_image_ref = if let Some(config_dir) = args.config {
        let detected = config::detect_config(&config_dir)?;
        let registry = snouty::settings::require(settings.repository(), "repository")?;

        let image_ref = container::generate_image_ref(registry);
        params.insert(ANT_CONFIG_IMAGE, &image_ref);
        Some((detected, registry.to_owned(), image_ref))
    } else {
        None
    };

    if !args.params.is_empty() {
        let extra = Params::from_key_value_pairs(&args.params)?;

        // Check for conflicts with typed flags already set in params
        for key in extra.as_map().keys() {
            if params.contains_key(key) {
                return Err(user_error(format!(
                    "invalid arguments: '{key}' cannot be overridden via --param"
                ))
                .suggestion("use the dedicated flag instead"));
            }
        }

        params.merge(extra);
    }

    if params.is_empty() {
        return Err(user_error("invalid arguments: no parameters provided"));
    }

    params.validate_test_params()?;

    if let Some((detected, registry, config_image)) = config_image_ref {
        let rt = container::runtime(settings)?;
        container::warn_ambiguous_engine(settings, rt.as_ref(), json);

        // For compose configs, every service image is pinned to its local
        // digest (snouty pulls only a missing image below a `private_registries`
        // prefix): served from a registry confirmed to already have it, or
        // pushed to the Antithesis registry. The compose file is then
        // canonicalized, digest-pinned, and baked into the
        // config image, so the platform runs exactly what was resolved here.
        // k8s configs reference images by name in the manifests and the
        // platform pulls them itself.
        let pinned_config = match &detected {
            config::Config::Compose(compose_config) => {
                let compose = compose::DockerCompose::resolve(rt.as_ref(), compose_config.clone())?;
                let pinned_yaml =
                    compose.pin_images(rt.as_ref(), &registry, settings.private_registries())?;
                let staged = compose::stage_pinned_config(compose_config.dir(), &pinned_yaml)?;
                rt.build_and_push_config_image(staged.path(), &config_image)?
            }
            config::Config::Kubernetes(_) => {
                rt.build_and_push_config_image(detected.dir(), &config_image)?
            }
        };
        params.insert(
            ANT_CONFIG_IMAGE,
            container::strip_registry(&pinned_config, &registry),
        );
    }

    // No --source means the run is ephemeral. Set the flag and tell the user
    // now — after validation and the config image build have succeeded — so the
    // notice lands right before the run is sent rather than ahead of work that
    // might still fail.
    if !has_source {
        params.insert(ANT_IS_EPHEMERAL, "true");
        eprintln!(
            "Starting an ephemeral run; its findings will not be retained as historic \
             results. Pass --source to record property history across runs."
        );
    }

    let response = launch_webhook(&args.webhook, params, settings, verbose).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else {
        println!(
            "Test run started: run_id {}",
            response.run_id.as_deref().unwrap_or("(unknown)")
        );
    }

    Ok(())
}

async fn launch_webhook(
    webhook: &str,
    params: Params,
    settings: &Settings,
    verbose: bool,
) -> Result<snouty::api::LaunchResponse> {
    params.validate_test_params()?;

    eprintln!(
        "\nRequesting Antithesis test run with params:\n{}",
        serde_json::to_string_pretty(&params.to_redacted_map())?
    );

    let api = AntithesisApi::new_for_launch(settings, verbose)?;
    api.launch_test(webhook, &params).await
}

fn debug_typed_params(args: &DebugArgs) -> Params {
    let mut params = Params::new();

    if let Some(run_id) = &args.run_id {
        params.insert(ANT_DEBUGGING_RUN_ID, run_id.as_str());
    }
    if let Some(session_id) = &args.session_id {
        params.insert(ANT_DEBUGGING_SESSION_ID, session_id.as_str());
    }
    if let Some(input_hash) = &args.input_hash {
        params.insert(ANT_DEBUGGING_INPUT_HASH, input_hash.as_str());
    }
    if let Some(vtime) = &args.vtime {
        params.insert(ANT_DEBUGGING_VTIME, vtime.to_string());
    }
    if let Some(description) = &args.description {
        params.insert(ANT_EVENT_DESCRIPTION, description.as_str());
    }
    if let Some(recipients) = &args.recipients {
        params.insert(ANT_REPORT_RECIPIENTS, recipients.as_str());
    }

    params
}

fn debug_params(args: DebugArgs) -> Result<Params> {
    let mut params = if args.stdin {
        get_stdin_params()?
    } else {
        Params::default()
    };
    params.merge(debug_typed_params(&args));

    if params.is_empty() {
        return Err(user_error("invalid arguments: no parameters provided"));
    }

    Ok(params)
}

async fn cmd_debug(
    args: DebugArgs,
    settings: &Settings,
    OutputOptions { json, verbose }: OutputOptions,
) -> Result<()> {
    let mut params = debug_params(args)?;
    // Every path into the moment has merged by now — the typed flags and raw
    // JSON on stdin — so normalizing here makes both send the same text. It
    // runs before validation, so a bad vtime reports itself rather than
    // surfacing as a schema error.
    params.normalize_debug_vtime()?;
    params.validate_debugging_params()?;
    params.ensure_single_debug_target()?;

    eprintln!(
        "\nRequesting the Antithesis multiverse debugger with params:\n{}",
        serde_json::to_string_pretty(&params.to_redacted_map())?
    );

    let api = AntithesisApi::new_for_launch(settings, verbose)?;
    let response = api.launch_debugging(&params).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else {
        println!(
            "Debugging session started: run_id {}",
            response.run_id.as_deref().unwrap_or("(unknown)")
        );
    }

    Ok(())
}

fn cmd_completions(shell: Shell) -> Result<()> {
    // `Cli::command()` already reflects the feature gate, though it only goes
    // so far: clap_complete emits hidden subcommands into the candidate list
    // for bash and zsh, so a gated-off command can still be tab-completed —
    // and is then refused by `gated_command_error`.
    let mut cmd = Cli::command();
    let bin_name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, bin_name, &mut io::stdout());
    Ok(())
}

/// Validate a requested update target against the running version.
///
/// Errors if `requested` isn't valid semver, or if it's older than `current`
/// and the downgrade wasn't forced. `current` is snouty's own
/// `CARGO_PKG_VERSION`, which is always valid semver. Semver pre-release
/// ordering applies, so e.g. `0.6.0-rc.1` is a downgrade from `0.6.0`.
fn check_update_target(requested: &str, current: &str, force: bool) -> Result<()> {
    let target = Version::parse(requested).map_err(|_| {
        user_error(format!(
            "invalid version `{requested}`: expected a semver release like 0.6.0 or 0.6.0-rc.1"
        ))
    })?;
    let current = Version::parse(current).expect("snouty's own version is always valid semver");
    if target < current && !force {
        return Err(user_error(format!(
            "{requested} is older than the installed snouty {current}; this would be a downgrade"
        ))
        .suggestion("re-run with --force to install an older version"));
    }
    Ok(())
}

fn cmd_update(args: UpdateArgs, settings: &Settings) -> Result<()> {
    // When a specific version is requested, validate it and refuse an unforced
    // downgrade up front, before bothering to spawn the helper.
    if let Some(version) = &args.version {
        check_update_target(version, env!("CARGO_PKG_VERSION"), args.force)?;
    }

    // The --channel flag overrides the (already validated) setting.
    let channel = args.channel.unwrap_or(settings.update_channel());

    // Attempt to spawn snouty-update and wait for it to finish. An explicit
    // version is forwarded via --version; the helper installs it directly
    // (pre-releases included), so --prerelease is only needed when the
    // unstable channel picks "latest". The helper then installs the greatest
    // version across releases and pre-releases, so a release newer than every
    // pre-release still wins.
    let mut updater = Command::new("snouty-update");
    if let Some(version) = &args.version {
        updater.arg("--version").arg(version);
    } else if channel == UpdateChannel::Unstable {
        updater.arg("--prerelease");
    }
    match updater.status() {
        Ok(status) if status.success() => {
            std::process::exit(0);
        }
        Ok(status) => {
            log::error!("snouty-update failed with exit code {}\n", status);
        }
        Err(err) => {
            if err.kind() == ErrorKind::NotFound {
                log::warn!("snouty-update command not found\n");
            } else {
                log::error!("failed to run snouty-update: {}\n", err);
            }
        }
    }

    // Updater not found, show manual update instructions
    eprintln!(
        "You are running snouty {}.\n\n\
         To check for updates, visit:\n\
         https://github.com/antithesishq/snouty/releases",
        env!("CARGO_PKG_VERSION")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre::{Report, eyre};

    #[test]
    fn suppress_broken_pipe_swallows_stdout_pipe_errors() {
        let err = io::Error::new(ErrorKind::BrokenPipe, "stdout closed");
        assert!(suppress_broken_pipe(Err(Report::new(err))).is_ok());
    }

    #[test]
    fn suppress_broken_pipe_passes_through_other_errors() {
        let io_err = io::Error::other("disk on fire");
        assert!(suppress_broken_pipe(Err(Report::new(io_err))).is_err());
        assert!(suppress_broken_pipe(Err(eyre!("plain error"))).is_err());
    }

    /// A broken connection during an API call must not be mistaken for a
    /// closed stdout pipe. reqwest wraps the socket-level io::Error in its
    /// own error type, whose source chain eyre's downcast does not traverse —
    /// so no io::Error (broken pipe or otherwise) is downcastable from an
    /// API error report, and it can never be suppressed.
    #[tokio::test]
    async fn suppress_broken_pipe_ignores_network_socket_errors() {
        // Grab a port that refuses connections by binding then dropping.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let reqwest_err = reqwest::get(format!("http://{addr}/api/v0/runs"))
            .await
            .unwrap_err();

        // Sanity check: the socket failure really is an io::Error, buried in
        // reqwest's source chain.
        let mut source = std::error::Error::source(&reqwest_err);
        let mut found_io = false;
        while let Some(err) = source {
            found_io |= err.downcast_ref::<io::Error>().is_some();
            source = err.source();
        }
        assert!(
            found_io,
            "expected an io::Error in the reqwest source chain"
        );

        // Wrap it the way api.rs wraps communication errors.
        let report = eyre!(reqwest_err).wrap_err("failed to contact API");

        // The io::Error is not downcastable through reqwest's error...
        assert!(report.downcast_ref::<io::Error>().is_none());
        // ...so the report can never be suppressed.
        assert!(suppress_broken_pipe(Err(report)).is_err());
    }

    #[test]
    fn check_update_target_allows_upgrade() {
        assert!(check_update_target("0.6.0", "0.5.0", false).is_ok());
    }

    #[test]
    fn check_update_target_allows_reinstalling_same_version() {
        assert!(check_update_target("0.5.0", "0.5.0", false).is_ok());
    }

    #[test]
    fn check_update_target_blocks_unforced_downgrade() {
        let err = check_update_target("0.4.0", "0.5.0", false).unwrap_err();
        let rendered = format!("{err}");
        assert!(rendered.contains("downgrade"), "got: {rendered}");
    }

    #[test]
    fn check_update_target_allows_forced_downgrade() {
        assert!(check_update_target("0.4.0", "0.5.0", true).is_ok());
    }

    #[test]
    fn check_update_target_treats_prerelease_as_downgrade_of_release() {
        // Semver orders 0.6.0-rc.1 before the final 0.6.0, so installing the rc
        // over the release is a downgrade.
        let err = check_update_target("0.6.0-rc.1", "0.6.0", false).unwrap_err();
        assert!(format!("{err}").contains("downgrade"));
    }

    #[test]
    fn check_update_target_allows_prerelease_above_current() {
        assert!(check_update_target("0.6.0-rc.1", "0.5.0", false).is_ok());
    }

    #[test]
    fn check_update_target_rejects_invalid_version() {
        let err = check_update_target("not-a-version", "0.5.0", false).unwrap_err();
        assert!(format!("{err}").contains("invalid version"));
    }
}
