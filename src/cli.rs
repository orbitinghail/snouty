use std::num::NonZeroU64;

use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};

use color_eyre::Section;
use color_eyre::eyre::Report;

use crate::api::{RunStatus, SEARCH_DEFAULT_LIMIT};
use crate::error::user_error;
use crate::features::{self, Feature};
use crate::time::HumanDuration;
use crate::vtime::VTime;

/// Every `RunStatus` variant, used to enumerate valid `--status` values in
/// error output. The generated enum offers no iteration, so this array is the
/// source of truth; [`assert_run_statuses_complete`] forces an update here
/// whenever the generated enum gains a variant.
const ALL_RUN_STATUSES: [RunStatus; 6] = [
    RunStatus::Starting,
    RunStatus::InProgress,
    RunStatus::Completed,
    RunStatus::Cancelled,
    RunStatus::Incomplete,
    RunStatus::Unknown,
];

/// Compile-time guard: matching every variant with no wildcard arm makes this
/// fail to compile when the generated `RunStatus` gains a variant, which is
/// the cue to extend [`ALL_RUN_STATUSES`].
const fn assert_run_statuses_complete(status: RunStatus) {
    match status {
        RunStatus::Starting
        | RunStatus::InProgress
        | RunStatus::Completed
        | RunStatus::Cancelled
        | RunStatus::Incomplete
        | RunStatus::Unknown => {}
    }
}

const _: () = assert_run_statuses_complete(RunStatus::Starting);

/// clap value parser for `--status` that keeps a friendly, enumerated error
/// message (the generated `RunStatus::from_str` only says "invalid value").
fn parse_run_status(value: &str) -> Result<RunStatus, String> {
    value.parse::<RunStatus>().map_err(|_| {
        let valid = ALL_RUN_STATUSES.map(|s| s.to_string()).join(", ");
        format!("invalid status: '{value}'\nvalid values: {valid}")
    })
}

/// clap value parser for `runs wait --poll-interval`: a [`HumanDuration`] of
/// at least 1 minute — polling faster cannot observe a run (which takes
/// minutes to hours) any sooner, and only hammers the API.
fn parse_poll_interval(value: &str) -> Result<HumanDuration, String> {
    let interval = value.parse::<HumanDuration>().map_err(|e| e.to_string())?;
    if interval.seconds() < 60 {
        return Err("poll interval must be at least 1 minute".to_string());
    }
    Ok(interval)
}

#[derive(Parser)]
#[command(name = "snouty")]
#[command(about = "CLI for the Antithesis API", long_about = None)]
// SNOUTY_VERSION (from build.rs) is the crate version plus the build's git sha
// when known, so `--version` and the `version` subcommand print the same string.
#[command(version = env!("SNOUTY_VERSION"))]
pub struct Cli {
    /// Output JSON where supported (NDJSON for list/stream commands, pretty JSON otherwise)
    // High display_order so the two global flags sort to the bottom of every
    // command's option list instead of wedging between that command's own flags.
    #[arg(long, global = true, display_order = 1000)]
    pub json: bool,

    /// Log API requests to stderr (authentication tokens redacted)
    #[arg(long, global = true, display_order = 1001)]
    pub verbose: bool,

    /// Path to the snouty settings file (default: ./.snouty.toml; overrides SNOUTY_SETTINGS_PATH)
    #[arg(long, global = true, display_order = 1002)]
    pub settings: Option<std::path::PathBuf>,

    /// Settings profile to select (overrides ANTITHESIS_PROFILE)
    #[arg(long, global = true, value_parser = validate_non_empty, display_order = 1003)]
    pub profile: Option<String>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Launch a test run
    #[command(long_about = r#"Launch a test run

Example:
  snouty launch --webhook basic_test --config ./config \
    --test-name "my-test" \
    --description "nightly test run" \
    --duration 30 \
    --recipients "team@example.com"

The -c/--config flag points at a local directory containing docker-compose.yaml
(this is the config image source, unrelated to snouty's own settings file).
Images required for the run need to have been built already. Pushing happens
automatically.

Alternatively, pass a pre-built config image directly:
  snouty launch --webhook basic_test \
    --config-image us-central1-docker.pkg.dev/proj/repo/config:latest \
    --duration 30

Extra parameters can be passed with --param:
  snouty launch -w basic_test --duration 30 \
    --param antithesis.integrations.github.token=TOKEN \
    --param my.custom.property=value

Additional container images that the config parser can't discover (e.g. an
image referenced only in a Kubernetes CRD field) can be registered with the
antithesis.images param, a semicolon-delimited [REGISTRY/]NAME(:TAG|@DIGEST)
list:
  snouty launch -w basic_k8s_test --config ./config --duration 30 \
    --param 'antithesis.images=app@sha256:...;db:latest'

Add --json for machine-readable output. The launch response prints as one
JSON object:
  snouty launch --json -w basic_test --duration 30 | jq -r .runId

Tenant and repository may be set via the environment variables below, or in a
settings file (./.snouty.toml by default; see the global --settings/--profile
flags and the README). Environment variables take precedence.

Environment variables (override any settings file):
  ANTITHESIS_TENANT       Your Antithesis tenant name (required).
  ANTITHESIS_API_KEY      API key authentication (preferred).
  ANTITHESIS_USERNAME     Username (deprecated; required when API key is not set).
  ANTITHESIS_PASSWORD     Password (deprecated; required when API key is not set).
  ANTITHESIS_REPOSITORY   Container registry for pushing images (required with --config).
  SNOUTY_CONTAINER_ENGINE Force "docker" or "podman" (auto-detected by default)."#)]
    Launch(LaunchArgs),

    /// Deprecated: use `launch` instead
    #[command(hide = true)]
    Run(LaunchArgs),

    /// Interact with test runs
    #[command(
        long_about = r#"Interact with test runs

List, inspect, and view logs for Antithesis test runs.

When no subcommand is given, lists all runs (same as `snouty runs list`).

Examples:
  snouty runs
  snouty runs list --status completed --launcher nightly
  snouty runs show <run_id>
  snouty runs wait <run_id>
  snouty runs properties <run_id>
  snouty runs properties --failing <run_id>
  snouty runs properties <run_id> --name <substring> --detail
  snouty runs build-logs <run_id>
  snouty runs logs <run_id> <hash> [vtime]
  snouty runs events <run_id> -m <query>

Add --json for machine-readable output. Every subcommand prints JSON in place
of its table or its rendered events:
  snouty --json runs list | jq -r .run_id"#,
        subcommand_required = false
    )]
    Runs {
        #[command(subcommand)]
        command: Option<RunsCommands>,
    },

    /// Launch a debugging session
    #[command(long_about = r#"Launch a debugging session

Identify the target run with exactly one of --run-id (preferred) or
--session-id.

Using CLI arguments:
  snouty debug \
    --run-id 9043254f65c9c65d63fe043a0abfc7fc-53-1 \
    --input-hash 6057726200491963783 \
    --vtime 329.8037810830865 \
    --description "debug this moment" \
    --recipients "team@example.com"

Add --json for machine-readable output. The response prints as one JSON
object:
  snouty debug --json --run-id <run_id> --input-hash <hash> --vtime <vtime> |
    jq -r .runId"#)]
    Debug(DebugArgs),

    /// Output shell completions
    #[command(long_about = r#"Output shell completions

Writes a completion script for SHELL to stdout; install it by sourcing it from
your shell config, e.g.:
  snouty completions zsh > ~/.zfunc/_snouty
  snouty completions bash | sudo tee /etc/bash_completion.d/snouty"#)]
    Completions {
        /// Shell to generate completions for
        shell: clap_complete::Shell,
    },

    /// Validate local Antithesis setup
    #[command(long_about = r#"Validate local Antithesis setup

Compose configs:
  Runs docker-compose locally and watches for the setup-complete event to
  confirm instrumentation is working. After setup-complete is detected,
  discovers test commands from /opt/antithesis/test/v1 inside the
  running containers and validates their structure.

  Before starting anything, validate resolves the compose file twice — with
  your shell and under a scrubbed environment matching the hermetic Antithesis
  environment — and fails if any ${VAR} resolves differently, catching setups
  that only work locally because a value came from your shell. Use
  --allow-compose-divergence to downgrade that to a warning.

  Test commands are discovered by scanning /opt/antithesis/test/v1 from each
  running container for {test_name}/{command} entries. Test commands are
  validated to have recognized prefixes and at least one driver or anytime
  test command when any are present. Test commands are not executed.

  The setup-complete event is watched through a temp directory bind-mounted
  into each container. If your container engine runs inside a VM or on
  another machine and does not share this machine's temp directory, snouty
  will not be able to see the setup-complete event. Set SNOUTY_TEMP_DIR to a
  directory under a path the VM shares with write access, or share this
  machine's temp directory with the VM.

Kubernetes configs:
  Runs docker.io/antithesishq/k8s-validator against the manifests/
  directory to perform static analysis of the manifests. --timeout,
  --keep-running, and --allow-compose-divergence have no effect here (no
  workloads or containers are started, and there is no docker-compose config
  to render).

Example:
  snouty validate ./config
  snouty validate ./config --timeout 10
  snouty validate ./k8s-config"#)]
    Validate(ValidateArgs),

    /// Check environment configuration
    #[command(long_about = r#"Check environment configuration

Verifies that your environment is properly configured for Antithesis testing.
Runs health checks — container runtime, docker compose, your credentials
(stored by `snouty login`, or set in the ANTITHESIS_* environment variables),
and API connectivity — then prints the resolved settings (tenant, repository,
container engine) so you can confirm what snouty will use.

snouty prefers an API key (full API access); a username and password is
deprecated auth, accepted only by `snouty launch` and `snouty debug`.

When credentials are configured, doctor also contacts the Antithesis API to
report the API and tenant versions and confirm connectivity. Pass --offline to
skip that network call.

Exits non-zero if any required check fails. Pass --json for a machine-readable
report (e.g. to gate CI).

Example:
  snouty doctor
  snouty doctor --json | jq -r .settings.tenant
  snouty doctor --offline"#)]
    Doctor(DoctorArgs),

    /// Print version information
    Version,

    /// Check for and install updates
    #[command(long_about = r#"Check for and install updates

Runs the bundled `snouty-update` helper, which checks for a newer release and
replaces the snouty binary in place. Does nothing if `snouty-update` is not
installed alongside snouty.

Pass a version to install a specific release instead of the latest, including
pre-releases:
  snouty update 0.6.0
  snouty update 0.6.0-rc.1

The update channel decides what "latest" means: `stable` (the default)
installs the latest release, `unstable` also considers pre-releases but still
installs the latest release when it is newer than every pre-release. Set the
channel with the `update_channel` setting (or SNOUTY_UPDATE_CHANNEL), and
override it for one run with --channel:
  snouty update --channel unstable

Installing a version older than the one you're running is a downgrade and
requires --force."#)]
    Update(UpdateArgs),

    /// Search Antithesis documentation
    #[command(long_about = r#"Search Antithesis documentation

Full-text search over a local copy of the Antithesis docs, auto-updated before
each use unless --offline.

Search for a page, browse the tree to find one, then show it. `sqlite` prints
the path to the local database for querying it directly.

Examples:
  snouty docs search fault injection
  snouty docs tree sdk
  snouty docs show getting_started

Add --json for machine-readable output. Only `search` prints JSON; the other
subcommands print text either way:
  snouty --json docs search fault injection | jq -r '.[].path'"#)]
    Docs {
        /// Don't check for documentation updates
        #[arg(long)]
        offline: bool,

        #[command(subcommand)]
        command: DocsCommands,
    },

    /// Sign in and store your snouty configuration
    #[command(long_about = r#"Sign in and store your snouty configuration

Provide configuration and authentication information to persist in the global
snouty settings file, optionally under a named profile. Sensitive information and
information not provided via args are asked for at the terminal, so this command
needs an interactive session.

NOTE: `snouty login` will offer to reuse your existing configuration values, including
any sourced from a local .snouty.toml file or the file specified by --settings or via
the SNOUTY_SETTINGS_PATH environment variable. However, snouty login will save the
specified configuration and credentials to the "global" files in your home directory.

Examples:
  snouty login
  snouty login --tenant "mytenant" --repository "repository"
  snouty login --profile "profile""#)]
    Login {
        #[arg(long, value_parser = validate_non_empty)]
        tenant: Option<String>,

        #[arg(long, value_parser = validate_non_empty)]
        repository: Option<String>,
    },
}

fn validate_non_empty(value: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        Err("Value may not be empty or whitespace".to_owned())
    } else {
        Ok(value.to_owned())
    }
}

#[derive(Subcommand)]
pub enum DocsCommands {
    /// Search the documentation
    #[command(long_about = r#"Search the documentation

Uses full-text search across the Antithesis documentation database.
The database is automatically updated before each search unless --offline is passed to the docs command.

Prints ranked matches (title and page path); pass a path to `snouty docs show`.
Use --list to print only the paths.

By default the query is searched as literal text. Pass --match to treat the
query as a raw SQLite FTS5 expression instead, enabling operators like
AND/OR/NOT/NEAR, "quoted phrases", `title:` column filters, and `prefix*`.

Examples:
  snouty docs search fault injection
  snouty docs search "config image"
  snouty docs search moment.branch
  snouty docs search sdk setup
  snouty docs search --match 'sdk NOT java'

Add --json for machine-readable output. The matches print as one JSON array
of {path, title, snippet} objects, or of paths with --list:
  snouty --json docs search fault injection | jq -r '.[].path'"#)]
    Search {
        /// Print only matching page paths, one per line
        #[arg(short = 'l', long)]
        list: bool,

        /// Maximum number of results to return
        #[arg(short = 'n', long, default_value = "10")]
        limit: usize,

        /// Treat the query as a raw FTS5 expression (AND/OR/NOT/NEAR, "phrases",
        /// title: filters, prefix*) instead of literal text
        #[arg(short = 'm', long = "match")]
        match_mode: bool,

        /// Search query
        query: Vec<String>,
    },
    /// Print a tree of documentation paths
    #[command(long_about = r#"Print a tree of documentation paths

Builds a directory-like tree from all page paths stored in the documentation database.

Examples:
  snouty docs tree
  snouty docs tree --depth 2
  snouty docs tree -d 2
  snouty docs tree sdk

This command prints text only. --json has no effect on it."#)]
    Tree {
        /// Limit output to nodes at this depth or shallower
        #[arg(short = 'd', long)]
        depth: Option<std::num::NonZeroUsize>,

        /// Optional case-insensitive filter applied to page paths and titles
        filter: Option<String>,
    },

    /// Show full contents of a documentation page
    #[command(long_about = r#"Show full contents of a documentation page

Displays the full markdown content of a page by its path.
If the exact path is not found, suggests similar pages.

This command prints text only. --json has no effect on it."#)]
    Show {
        /// Page path (e.g. "getting_started/overview")
        path: String,
    },

    /// Print the path to the cached SQLite database
    #[command(long_about = r#"Print the path to the cached SQLite database

Useful for directly querying the documentation database with external tools.

This command prints the path only. --json has no effect on it."#)]
    Sqlite,
}

#[derive(Args)]
pub struct UpdateArgs {
    /// Release to install (e.g. 0.6.0 or 0.6.0-rc.1). Defaults to the latest release.
    pub version: Option<String>,

    /// Install the requested version even if it is older than the current one (a downgrade)
    #[arg(long)]
    pub force: bool,

    /// Update channel to use, overriding the `update_channel` setting
    #[arg(long, value_enum)]
    pub channel: Option<UpdateChannel>,
}

/// Which releases `snouty update` considers when no explicit version is given.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, ValueEnum)]
pub enum UpdateChannel {
    /// Install the latest release
    #[default]
    Stable,
    /// Also consider pre-releases, but prefer the latest release when it is newer
    Unstable,
}

impl UpdateChannel {
    pub const STABLE: &'static str = "stable";
    pub const UNSTABLE: &'static str = "unstable";

    pub fn as_str(self) -> &'static str {
        match self {
            UpdateChannel::Stable => Self::STABLE,
            UpdateChannel::Unstable => Self::UNSTABLE,
        }
    }
}

impl std::str::FromStr for UpdateChannel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            Self::STABLE => Ok(UpdateChannel::Stable),
            Self::UNSTABLE => Ok(UpdateChannel::Unstable),
            other => Err(format!(
                "expected `{}` or `{}`, got `{other}`",
                Self::STABLE,
                Self::UNSTABLE
            )),
        }
    }
}

#[derive(Args)]
pub struct ValidateArgs {
    /// Path to config directory containing either docker-compose.yaml or a
    /// manifests/ subdirectory (Kubernetes manifests).
    pub config: std::path::PathBuf,

    /// Maximum seconds to wait for containers to start and reach setup-complete
    #[arg(long, default_value = "120")]
    pub timeout: u64,

    /// Leave containers running after validation for manual inspection
    #[arg(long)]
    pub keep_running: bool,

    /// Warn instead of failing when docker-compose.yaml renders differently in
    /// the hermetic Antithesis environment than it does on this machine
    #[arg(long)]
    pub allow_compose_divergence: bool,
}

#[derive(Args)]
pub struct DoctorArgs {
    /// Skip the network check (don't contact the Antithesis API for versions)
    #[arg(long)]
    pub offline: bool,
}

#[derive(Args)]
pub struct LaunchArgs {
    /// Webhook endpoint name (e.g., basic_test, basic_k8s_test)
    #[arg(short, long)]
    pub webhook: String,

    /// Local config dir (docker-compose.yaml or a manifests/ subdir), auto-built
    /// and pushed as the config image. Compose service images must already exist
    /// locally, unless the `private_registries` setting lists their registry.
    #[arg(short, long, conflicts_with = "config_image")]
    pub config: Option<std::path::PathBuf>,

    /// Pre-built config image reference (e.g., us-central1-docker.pkg.dev/proj/repo/config:latest)
    #[arg(long)]
    pub config_image: Option<String>,

    /// Test name
    #[arg(long)]
    pub test_name: Option<String>,

    /// Test description
    #[arg(long)]
    pub description: Option<String>,

    /// Test duration in minutes, or h/m units (e.g. 90m, 2h, 1h30m)
    // `HumanDuration: FromStr` gives clap the parser; we send it to the API as
    // `.minutes().to_string()`, the (possibly fractional) minute count it wants.
    #[arg(long)]
    pub duration: Option<HumanDuration>,

    /// Mark the test run as ephemeral. Ephemeral runs will not appear in future reports as a historic result.
    #[arg(long)]
    pub ephemeral: bool,

    /// Identifier that groups property history in reports — runs sharing a
    /// --source share history (e.g. per-branch)
    #[arg(long)]
    pub source: Option<String>,

    /// Report recipients (semicolon-delimited email addresses)
    #[arg(long)]
    pub recipients: Option<String>,

    /// Suppress log lines matching this RE2 pattern during fuzzing. Matching is
    /// unanchored and case-sensitive by default (use `(?i)` for case-insensitive).
    /// Suppressed lines stay available in multiverse debugging. RE2 syntax: no
    /// lookahead/lookbehind or backreferences. Max 1023 bytes. Requires tenant
    /// release 59 or newer.
    #[arg(long)]
    pub filter_logs_matching: Option<String>,

    /// Extra parameters as key=value pairs (repeatable)
    #[arg(long = "param")]
    pub params: Vec<String>,
}

#[derive(Args)]
pub struct DebugArgs {
    /// Read parameters from stdin (JSON)
    #[arg(long)]
    pub stdin: bool,

    /// Run ID of the test run to debug (preferred; mutually exclusive with --session-id)
    #[arg(long)]
    pub run_id: Option<String>,

    /// Session ID of the test run to debug (mutually exclusive with --run-id)
    #[arg(long)]
    pub session_id: Option<String>,

    /// Input hash identifying the moment to debug
    #[arg(long, allow_hyphen_values = true)]
    pub input_hash: Option<String>,

    /// Virtual time identifying the moment to debug
    #[arg(long)]
    pub vtime: Option<VTime>,

    /// Debugging session description
    #[arg(long)]
    pub description: Option<String>,

    /// Report recipients (semicolon-delimited email addresses)
    #[arg(long)]
    pub recipients: Option<String>,
}

/// The block-rendering paragraph `runs events` and `runs search` share in
/// their long help. A macro rather than a `const` so both call sites can
/// splice it into their `concat!`-built literals (`concat!` takes literals
/// only, and a macro expansion is one).
macro_rules! classified_blocks_help {
    () => {
        "Matching events print as classified blocks: a `moment HASH VTIME` divider\n\
         opens each timeline segment (feed its HASH and VTIME into `runs logs` to see\n\
         the surrounding logs), and each event under it renders on one line with the\n\
         Antithesis event shapes — SDK assertions, faults, container lifecycle, test\n\
         composer — each in their own concise form."
    };
}

/// `runs search`'s long help. Static text: the `search_long_about` test
/// keeps it in sync with [`event_set_dsl::VERBS`] (every verb must appear)
/// and holds every line to 78 columns, since clap prints long_about
/// verbatim.
const SEARCH_LONG_ABOUT: &str = concat!(
    r#"Run an event-set DSL query against a run's events.

This command is gated behind the `runs-search` unstable feature, because the
events-search API does not honor its documented contract yet on current
tenants. Enable it by setting SNOUTY_UNSTABLE_FEATURES=runs-search. An
unstable feature can change or go away in any release.

QUERY is a pipeline of dot-separated verbs applied to the run's event stream,
evaluated left to right; each verb narrows, reshapes, or combines the set of
events flowing through it.

Verbs:
  matches({f: "x"})        keep events whose fields equal every given value
  contains({f: "x"})       keep events whose fields contain every substring
  not_matches({f: "x"})    drop exact matches
  excludes({f: "x"})       drop substring matches
  filter(ev => expr)       keep events where the JS expression is truthy
  map(ev => expr)          reshape each event (ev.add_fields({...}) adds)
  flatmap(ev => expr)      map, then flatten the result one level
  narrow(["f1", "f2"])     keep only the listed fields
  fold((s, ev) => e, s0)   thread state along each timeline, annotating
  union(set, ...)          OR this set with others, deduplicated
  intersect(set, ...)      AND this set with others
  difference(set)          subtract another event set from this one
  distinct_by_moment(set)  union, keeping one event per vtime
  with_last({n: set})      annotate each event with the nearest earlier
                           event from `set` in its own timeline
  with_next({n: set})      the same, looking forward

The string verbs (matches/contains/not_matches/excludes) address four fields:
output_text, container, stream, and source (the emitter's name). Every other
field is reachable from JS through `ev`, e.g. `ev.moment.vtime`.

Query snippets (each is a complete QUERY, ready to paste):

  # log text contains a substring
  contains({output_text: "connection refused"})

  # log text matches a regex
  filter(ev => /timed?.?out/i.test(ev.output_text || ""))

  # a substring anywhere in the raw event JSON
  filter(ev => JSON.stringify(ev).includes("needle"))

  # one container's stderr
  matches({container: "etcd0", stream: "error"})

  # errors from everything except a noisy container
  contains({output_text: "error"}).not_matches({container: "setup"})

  # events in a vtime window
  filter(ev => ev.moment.vtime > 100 && ev.moment.vtime < 150)

  # one assertion's evaluations, by id, that came up false
  filter(ev => ev.antithesis_assert?.hit
    && ev.antithesis_assert.id == "acks are durable"
    && !ev.antithesis_assert.condition)

  # hit sometimes assertions that evaluated true (also: always, reachability)
  filter(ev => ev.antithesis_assert?.assert_type == "sometimes"
    && ev.antithesis_assert.hit && ev.antithesis_assert.condition)

  # each crash annotated with the nearest earlier fault
  contains({output_text: "fatal"}).with_last({fault: filter(ev => ev.fault)})

"#,
    classified_blocks_help!(),
    r#" Rows reshaped by map/narrow/fold
print as raw JSON.

Add --json for machine-readable output. Each event prints as one JSON
object on its own line:
  snouty --json runs search <run_id> 'contains({output_text: "err"})' \
    | jq -r .moment.vtime"#
);

#[derive(Subcommand)]
pub enum RunsCommands {
    /// List all runs
    #[command(
        long_about = r#"List recent runs (the default when `snouty runs` runs with no subcommand).

Columns: RUN ID, STATUS, CREATED, TEST NAME. Use --detail for the full
description and launcher.

Add --json for machine-readable output. Each run prints as one JSON object on
its own line, in the order the server returns them:
  snouty --json runs list | jq -r .run_id"#
    )]
    List(RunsListArgs),

    /// Show details of a specific run
    #[command(
        long_about = r#"Show a run's metadata: id, status, timestamps, launcher, and description.

Two fields report time and they mean different things. Duration is the
workload length requested at launch. Elapsed is wall-clock time, which also
spans provisioning, setup and teardown, so the two legitimately differ.
Source is the `antithesis.source` the run was launched from, when the
launcher recorded one.

Incomplete runs also show the failure moment (Failure Hash/VTime) to pass to
`runs logs`. Use --web to open the triage report in a browser.

Examples:
  snouty runs show <run_id>
  snouty runs show <run_id> --web

Add --json for machine-readable output. The run prints as one JSON object.
With --web it prints the report URL as {"url": ...} and opens no browser:
  snouty --json runs show <run_id> | jq -r .status"#
    )]
    Show {
        /// Run ID
        run_id: String,

        /// Open the run's triage report in a browser instead of printing details
        #[arg(short = 'w', long)]
        web: bool,
    },

    /// Wait for a run to reach a terminal state
    #[command(
        long_about = r#"Wait for a run to reach a terminal state (completed, cancelled, or incomplete).

Polls the run's status until it is terminal, then reports the final status and
exits 0 whatever that status is; the run's outcome is in the output, not the
exit code. A run that reports status `unknown` fails the command instead:
snouty cannot tell whether such a run will still make progress, so the caller
decides what to do.

The wait is unbounded unless --timeout is given, and the command is safe to
interrupt and re-run: waiting holds no state beyond the run id, so re-running
resumes the wait.

Examples:
  snouty runs wait <run_id>
  snouty runs wait <run_id> --timeout 2h
  snouty launch --json -w basic_test ... | jq -r .runId | xargs snouty runs wait

Add --json for machine-readable output. The final status prints as one JSON
object:
  snouty --json runs wait <run_id> | jq -r .status"#
    )]
    Wait {
        /// Run ID
        run_id: String,

        /// Time between status checks, in minutes or h/m/s units (e.g. 90s;
        /// minimum 1 minute)
        #[arg(long, default_value = "1m", value_parser = parse_poll_interval)]
        poll_interval: HumanDuration,

        /// Give up after this long (minutes, or h/m/s units, e.g. 2h);
        /// without it the wait is unbounded
        #[arg(long)]
        timeout: Option<HumanDuration>,
    },

    /// List property results for a run
    #[command(
        long_about = r#"List a run's property (assertion) results, one table per group.

Each table is headed by its group; columns are STATUS, EXAMPLES, NAME (failing
first). EXAMPLES is the example count, shown as examples/counterexamples when a
property has counterexamples.

Narrow with --name and/or --group (both case-insensitive substring matches);
add --detail to expand the matches into their examples and counter-example
moments instead of the table.

Examples:
  snouty runs properties <run_id> --failing
  snouty runs properties <run_id> --name eventually_validate --detail
  snouty runs properties <run_id> --group Unreachable --detail

Add --json for machine-readable output. Each property prints as one JSON
object on its own line. --json is mutually exclusive with --detail:
  snouty --json runs properties <run_id> --failing | jq -r .name"#
    )]
    Properties {
        /// Run ID
        run_id: String,

        /// Show only passing properties
        #[arg(long, conflicts_with = "failing")]
        passing: bool,

        /// Show only failing properties
        #[arg(long)]
        failing: bool,

        /// Only properties whose name contains this substring (case-insensitive)
        #[arg(long)]
        name: Option<String>,

        /// Only properties whose group contains this substring (case-insensitive)
        #[arg(long)]
        group: Option<String>,

        /// Expand each matching property into its examples / counter-example
        /// moments, instead of the summary table
        #[arg(short = 'd', long)]
        detail: bool,
    },

    /// Stream build logs for a run
    #[command(
        long_about = r#"Stream a run's build and setup logs: everything the platform did
before the test started.

Output: each line is `timestamp [stream] line`, where stream is `stdout` or
`stderr`. The whole build is streamed, so expect thousands of lines on a real
run. Grep the stream tag to narrow it, and read `[stderr]` first when a
launch failed.

Examples:
  snouty runs build-logs <run_id>
  snouty runs build-logs <run_id> | grep '\[stderr\]'
  snouty runs build-logs <run_id> | grep -i 'error\|denied'

Add --json for machine-readable output. Each log line prints as one JSON
object on its own line:
  snouty --json runs build-logs <run_id> | jq -r .text"#
    )]
    BuildLogs {
        /// Run ID
        run_id: String,
    },

    /// Stream moment logs for a run
    #[command(
        long_about = r#"Stream the logs along one branch of the run's multiverse.

INPUT_HASH identifies the branch: the hash of every input fed to the
simulation from the root moment to the branch's start. Logs stream from the
root (or --begin-vtime) to the branch's current end; a run in progress can
extend the branch, so the same INPUT_HASH can return more logs later. Give
VTIME to end the stream at that moment instead.

Output: a `moment HASH VTIME` divider opens each timeline segment, and each
event under it renders on one line as `VTIME [source] payload` — Antithesis
event shapes (SDK assertions, faults, container lifecycle, test composer)
each in their own concise form.

Add --json for machine-readable output. Each event prints as one JSON object
on its own line, and --raw passes the server's events through unchanged:
  snouty --json runs logs <run_id> <hash> | jq -r .moment.vtime"#
    )]
    Logs {
        /// Run ID
        run_id: String,

        /// Input hash identifying the branch to stream
        #[arg(allow_hyphen_values = true)]
        input_hash: String,

        /// Virtual time of the moment to end the stream at; omit it to stream to the branch's current end
        // Typed, so a malformed vtime is rejected by clap instead of by the
        // server. `allow_hyphen_values` is kept here (unlike `runs exec`),
        // because this command has always accepted a hyphen-led vtime.
        #[arg(allow_hyphen_values = true)]
        vtime: Option<VTime>,

        /// Start from this virtual time instead of the root
        #[arg(long, allow_hyphen_values = true)]
        begin_vtime: Option<VTime>,

        /// Start from this input hash (optimization; must be paired with --begin-vtime)
        #[arg(long, allow_hyphen_values = true, requires = "begin_vtime")]
        begin_input_hash: Option<String>,

        #[command(flatten)]
        render: EventOutputArgs,
    },

    /// Execute a command in a run's live session
    // Gated behind the `runs-exec` feature. `hide` is an expression, so the
    // decision is made when the command is built — the feature comes from the
    // environment, which needs no parse to read. Hiding only keeps it out of
    // `--help`; invoking it while disabled is refused by
    // [`gated_command_error`].
    #[command(
        hide = !features::is_enabled(Feature::RunsExec),
        long_about = r#"Execute a bash script in a run's live session, at a moment.

This command is gated behind the `runs-exec` unstable feature, because the
Antithesis API it calls is unstable and unavailable on most tenants. Enable it
by setting SNOUTY_UNSTABLE_FEATURES=runs-exec. An unstable feature can change
or go away in any release.

The run must have a live session (it is in progress). The script executes on
a fresh branch of the multiverse, so it does not disturb the running test.
INPUT_HASH and VTIME identify the moment to execute at; a moment comes from
`runs properties --detail` or `runs events`.

The script's stdout and stderr stream to snouty's stdout and stderr. On exit,
a trailer on stderr documents the branch's end moment, to chain a follow-up
command from. A non-zero exit code, a timeout, or a truncated stream fails
snouty with exit code 1.

Omit SCRIPT to read the script from stdin — a pipe, a redirect, or a heredoc.

Examples:
  snouty runs exec <run_id> <hash> <vtime> 'uname -a'
  echo 'ps aux' | snouty runs exec <run_id> <hash> <vtime>
  snouty runs exec <run_id> <hash> <vtime> < script.sh

Add --json for machine-readable output. Each frame of the stream prints as one
JSON object on its own line, and the trailer is left out:
  snouty --json runs exec <run_id> <hash> <vtime> 'ls' \
    | jq -r 'select(.type == "output").text'"#
    )]
    Exec {
        /// Run ID
        run_id: String,

        /// Input hash of the moment to execute at (with VTIME, picks the timeline)
        // A moment's input hash is routinely negative.
        #[arg(allow_hyphen_values = true)]
        input_hash: String,

        /// Virtual time of the moment to execute at
        // Parsed by `VTime` so a malformed value is rejected by clap, before
        // any API call. No `allow_hyphen_values`: a vtime is seconds since the
        // run began and is never negative, and accepting hyphen-led values
        // here would swallow a misplaced `--timeout` as the vtime.
        vtime: VTime,

        /// Bash script to execute; omit it to read the script from stdin
        script: Option<String>,

        /// Maximum seconds the server waits for the script to exit before
        /// reporting a timeout
        // The API's own default is 30 with a minimum of 0 and no maximum. A
        // 0-second timeout can only ever time out, so the floor here is 1; the
        // ceiling is left to the server rather than guessed at.
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
        timeout: u64,
    },

    /// Search events in a run
    #[command(
        long_about = concat!(
            "Search a run's events for one or more substrings (all must match).\n\n",
            "A term is matched against the text an event carries: log output, an\n\
             assertion's message and source function, and a test-composer command.\n\n",
            classified_blocks_help!(),
            "\n\nMatching runs server-side. More than one term requires the events-search API,\n\
             which is behind the `runs-search` unstable feature\n\
             (SNOUTY_UNSTABLE_FEATURES=runs-search).\n\n\
             Add --json for machine-readable output. Each event prints as one\n\
             JSON object on its own line:\n\
             \x20 snouty --json runs events <run_id> -m error | jq -r .moment.vtime"
        )
    )]
    Events {
        /// Run ID
        run_id: String,

        /// Substring to search for (repeatable; all matches must be present)
        #[arg(short = 'm', long = "match")]
        matches: Vec<String>,

        /// Maximum number of events to print. Raise it to make a search more
        /// exhaustive.
        #[arg(short = 'n', long, default_value_t = SEARCH_DEFAULT_LIMIT)]
        limit: NonZeroU64,

        /// Substrings to match, as a positional alias for `-m` (all must match).
        /// At least one needle (via `-m` or here) is required.
        query: Vec<String>,

        #[command(flatten)]
        render: EventOutputArgs,
    },

    /// Query events with the event-set DSL
    // Gated behind the `runs-search` feature (see `Feature::RunsSearch`): the
    // events-search API does not honor its documented contract yet. Same
    // mechanics as `runs exec` above — `hide` keeps it out of `--help`, and
    // invoking it while disabled is refused by [`gated_command_error`].
    #[command(
        hide = !features::is_enabled(Feature::RunsSearch),
        long_about = SEARCH_LONG_ABOUT
    )]
    Search(RunsSearchArgs),
}

#[derive(Args)]
pub struct RunsSearchArgs {
    /// Run ID
    pub run_id: String,

    /// Event-set DSL query
    pub query: String,

    /// Maximum number of events to print (default 50)
    #[arg(short = 'n', long)]
    pub limit: Option<NonZeroU64>,

    /// Keep the connection open and print new matches as they arrive
    /// (the limit still caps the total)
    #[arg(short = 'f', long)]
    pub follow: bool,

    /// Check the query's syntax without running it
    #[arg(long, conflicts_with = "follow")]
    pub check: bool,

    #[command(flatten)]
    pub render: EventOutputArgs,
}

/// The output-depth flags every event-stream command (`runs logs`,
/// `runs events`, `runs search`) shares. One definition keeps the flags, the
/// help text, and the raw/detail conflict from drifting between commands.
#[derive(Args)]
pub struct EventOutputArgs {
    /// Print the server's events untouched, one JSON object per line,
    /// skipping snouty's normalization (vtime, and fault annotation on
    /// `runs logs`); requires --json
    #[arg(short = 'r', long)]
    pub raw: bool,

    /// Detailed rendering: a full-width vtime on every line, source
    /// locations, and each event's attached details JSON
    #[arg(short = 'd', long, conflicts_with = "raw")]
    pub detail: bool,
}

/// The number of runs `runs list` prints when `--limit` names none.
pub const DEFAULT_RUNS_LIMIT: NonZeroU64 = NonZeroU64::new(10).unwrap();

#[derive(Args)]
pub struct RunsListArgs {
    /// Filter by status (starting, in_progress, completed, cancelled, incomplete, unknown)
    #[arg(short, long, value_parser = parse_run_status)]
    pub status: Option<RunStatus>,

    /// Filter by launcher name
    #[arg(short, long)]
    pub launcher: Option<String>,

    /// Only show runs created after this timestamp (ISO 8601)
    #[arg(long)]
    pub created_after: Option<DateTime<Utc>>,

    /// Only show runs created before this timestamp (ISO 8601)
    #[arg(long)]
    pub created_before: Option<DateTime<Utc>>,

    /// Maximum number of runs to display
    #[arg(short = 'n', long, default_value_t = DEFAULT_RUNS_LIMIT)]
    pub limit: NonZeroU64,

    /// Show a detailed key-value block per run, including the full description
    #[arg(short, long)]
    pub detail: bool,
}

impl Default for RunsListArgs {
    fn default() -> Self {
        Self {
            status: None,
            launcher: None,
            created_after: None,
            created_before: None,
            limit: DEFAULT_RUNS_LIMIT,
            detail: false,
        }
    }
}

/// The error for invoking a gated command whose feature is off.
///
/// This is the half of the gate that hiding cannot do: a hidden subcommand is
/// still callable. Anyone who types the command already knows it exists, so
/// the error says what is actually wrong and how to fix it, rather than
/// pretending the command is not there. `enabled` names the features that are
/// on — the caller passes them so the decision is testable without touching
/// the environment.
pub fn gated_command_error(command: &Commands, enabled: &[Feature]) -> Option<Report> {
    let gated = match command {
        Commands::Runs {
            command: Some(RunsCommands::Exec { .. }),
        } => (Feature::RunsExec, "snouty runs exec"),
        Commands::Runs {
            command: Some(RunsCommands::Search(_)),
        } => (Feature::RunsSearch, "snouty runs search"),
        _ => return None,
    };
    let (feature, path) = gated;
    if enabled.contains(&feature) {
        return None;
    }

    Some(
        user_error(format!(
            "`{path}` is an unstable feature and is not enabled"
        ))
        .note(format!(
            "enable it by setting {}={}",
            features::UNSTABLE_FEATURES_VAR_NAME,
            feature
        ))
        .note("an unstable feature can change or go away in any release")
        .suggestion(format!("run `{path} --help` for what it does")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("args should parse")
    }

    #[test]
    fn update_channel_parses_its_named_values() {
        assert_eq!(
            UpdateChannel::STABLE.parse::<UpdateChannel>().unwrap(),
            UpdateChannel::Stable
        );
        assert_eq!(
            UpdateChannel::UNSTABLE.parse::<UpdateChannel>().unwrap(),
            UpdateChannel::Unstable
        );
    }

    #[test]
    fn update_channel_rejects_unknown_values() {
        let err = "nightly".parse::<UpdateChannel>().unwrap_err();
        assert_eq!(err, "expected `stable` or `unstable`, got `nightly`");
    }

    #[test]
    fn a_gated_off_command_is_refused_and_an_enabled_one_runs() {
        let exec = parse(&["snouty", "runs", "exec", "RUN", "1", "2.0", "true"]).command;

        // Off: refused with a message that says what is wrong and how to fix
        // it. Whoever typed the command knows it exists, so pretending it does
        // not would only waste their time.
        let err = gated_command_error(&exec, &[]).expect("a gated-off command is refused");
        let rendered = format!("{err:?}");
        assert!(rendered.contains("`snouty runs exec`"), "{rendered}");
        assert!(rendered.contains("unstable feature"), "{rendered}");
        assert!(
            rendered.contains("SNOUTY_UNSTABLE_FEATURES=runs-exec"),
            "{rendered}"
        );
        assert!(rendered.contains("snouty runs exec --help"), "{rendered}");

        // On: allowed through.
        assert!(gated_command_error(&exec, &[Feature::RunsExec]).is_none());
        // An unrelated feature does not enable it.
        assert!(gated_command_error(&exec, &[Feature::Unknown("other".to_string())]).is_some());

        // `runs search` is gated the same way, behind its own feature.
        let search = parse(&["snouty", "runs", "search", "RUN", "q"]).command;
        let err = gated_command_error(&search, &[]).expect("a gated-off command is refused");
        let rendered = format!("{err:?}");
        assert!(
            rendered.contains("SNOUTY_UNSTABLE_FEATURES=runs-search"),
            "{rendered}"
        );
        assert!(gated_command_error(&search, &[Feature::RunsSearch]).is_none());
        assert!(gated_command_error(&search, &[Feature::RunsExec]).is_some());

        // Sibling subcommands are never gated.
        for args in [
            &["snouty", "runs", "logs", "RUN", "1", "2.0"][..],
            &["snouty", "runs"][..],
        ] {
            assert!(gated_command_error(&parse(args).command, &[]).is_none());
        }
    }

    #[test]
    fn duration_flag_parses_into_human_duration() {
        // Parsing/validation lives in `crate::time`; here we just confirm clap
        // wires `--duration` through `HumanDuration: FromStr`.
        let cli = parse(&[
            "snouty",
            "launch",
            "-w",
            "basic_test",
            "--duration",
            "1h30m",
        ]);
        let Commands::Launch(args) = cli.command else {
            panic!("expected launch command");
        };
        assert_eq!(args.duration.unwrap().minutes(), 90.0);
    }

    #[test]
    fn duration_flag_rejects_invalid_value() {
        // `.err()` avoids requiring `Cli: Debug` (which `unwrap_err` would).
        let err =
            Cli::try_parse_from(["snouty", "launch", "-w", "basic_test", "--duration", "1.5h"])
                .err()
                .expect("invalid duration should fail to parse")
                .to_string();
        assert!(err.contains("--duration"), "got: {err}");
        assert!(err.contains("number of minutes"), "got: {err}");
    }

    // The positional `input_hash`/`vtime` and the `--begin-*` flags must all
    // accept hyphen-led values: moment coordinates are routinely negative
    // (e.g. `snouty runs logs RUN -123 -2.0`).
    #[test]
    fn logs_accepts_negative_begin_vtime() {
        let cli = parse(&[
            "snouty",
            "runs",
            "logs",
            "RUN",
            "-123",
            "-2.0",
            "--begin-vtime",
            "-2.0",
            "--begin-input-hash",
            "0",
        ]);
        let Commands::Runs {
            command:
                Some(RunsCommands::Logs {
                    input_hash,
                    vtime,
                    begin_vtime,
                    begin_input_hash,
                    ..
                }),
        } = cli.command
        else {
            panic!("expected `runs logs`");
        };
        assert_eq!(input_hash, "-123");
        assert_eq!(vtime, Some("-2.0".parse::<VTime>().unwrap()));
        assert_eq!(begin_vtime, Some("-2.0".parse::<VTime>().unwrap()));
        assert_eq!(begin_input_hash.as_deref(), Some("0"));
    }

    // VTIME is optional; without it the stream runs to the branch's current
    // end.
    #[test]
    fn logs_accepts_a_missing_vtime() {
        let cli = parse(&["snouty", "runs", "logs", "RUN", "-123"]);
        let Commands::Runs {
            command: Some(RunsCommands::Logs {
                input_hash, vtime, ..
            }),
        } = cli.command
        else {
            panic!("expected `runs logs`");
        };
        assert_eq!(input_hash, "-123");
        assert_eq!(vtime, None);
    }

    // `-r` is the short form of `--raw`; note `-r` must not swallow the
    // hyphen-led positionals that follow it.
    #[test]
    fn logs_accepts_raw_short_flag() {
        let cli = parse(&["snouty", "runs", "logs", "-r", "RUN", "-123", "-2.0"]);
        let Commands::Runs {
            command: Some(RunsCommands::Logs { render, vtime, .. }),
        } = cli.command
        else {
            panic!("expected `runs logs`");
        };
        assert!(render.raw);
        assert_eq!(vtime, Some("-2.0".parse::<VTime>().unwrap()));

        let cli = parse(&["snouty", "runs", "logs", "RUN", "-123", "-2.0"]);
        let Commands::Runs {
            command: Some(RunsCommands::Logs { render, .. }),
        } = cli.command
        else {
            panic!("expected `runs logs`");
        };
        assert!(!render.raw);
    }

    // `runs events` accepts both the documented `-m/--match` form and a
    // backward-compatible trailing positional query; the two are merged.
    #[test]
    fn events_accepts_match_and_positional_query() {
        let cli = parse(&["snouty", "runs", "events", "RUN", "-m", "request"]);
        let Commands::Runs {
            command: Some(RunsCommands::Events { matches, query, .. }),
        } = cli.command
        else {
            panic!("expected `runs events`");
        };
        assert_eq!(matches, vec!["request".to_string()]);
        assert!(query.is_empty());

        let cli = parse(&["snouty", "runs", "events", "RUN", "request", "slow"]);
        let Commands::Runs {
            command: Some(RunsCommands::Events { matches, query, .. }),
        } = cli.command
        else {
            panic!("expected `runs events`");
        };
        assert!(matches.is_empty());
        assert_eq!(query, vec!["request".to_string(), "slow".to_string()]);
    }

    // `runs events --limit` defaults to 50; the server enforces the ceiling.
    #[test]
    fn events_limit_defaults_and_rejects_zero() {
        let cli = parse(&["snouty", "runs", "events", "RUN", "-m", "request"]);
        let Commands::Runs {
            command: Some(RunsCommands::Events { limit, .. }),
        } = cli.command
        else {
            panic!("expected `runs events`");
        };
        assert_eq!(limit, SEARCH_DEFAULT_LIMIT);

        let cli = parse(&[
            "snouty", "runs", "events", "RUN", "-m", "x", "--limit", "998",
        ]);
        let Commands::Runs {
            command: Some(RunsCommands::Events { limit, .. }),
        } = cli.command
        else {
            panic!("expected `runs events`");
        };
        assert_eq!(limit.get(), 998);

        // The limit is a `NonZeroU64`, so clap rejects 0 up front with a plain
        // message.
        let parsed = Cli::try_parse_from(["snouty", "runs", "events", "RUN", "-n", "0"]);
        assert!(parsed.is_err(), "expected --limit 0 to be rejected");
    }

    // `runs search --limit` stays unset unless given, and rejects 0.
    #[test]
    fn search_limit_rejects_zero() {
        let cli = parse(&[
            "snouty", "runs", "search", "RUN", "q", "--follow", "--limit", "998",
        ]);
        let Commands::Runs {
            command: Some(RunsCommands::Search(args)),
        } = cli.command
        else {
            panic!("expected `runs search`");
        };
        assert_eq!(args.limit.map(NonZeroU64::get), Some(998));

        let parsed = Cli::try_parse_from(["snouty", "runs", "search", "RUN", "q", "-n", "0"]);
        assert!(parsed.is_err(), "expected --limit 0 to be rejected");
    }

    // `runs search` takes the run id and one raw DSL query positionally; the
    // mode switches default off and the limit stays unset unless given.
    #[test]
    fn search_parses_query_and_defaults() {
        let cli = parse(&[
            "snouty",
            "runs",
            "search",
            "RUN",
            r#"contains({output_text: "raft"})"#,
        ]);
        let Commands::Runs {
            command: Some(RunsCommands::Search(args)),
        } = cli.command
        else {
            panic!("expected `runs search`");
        };
        assert_eq!(args.run_id, "RUN");
        assert_eq!(args.query, r#"contains({output_text: "raft"})"#);
        assert_eq!(args.limit, None);
        assert!(!args.follow && !args.check);

        let cli = parse(&["snouty", "runs", "search", "RUN", "q", "-n", "7", "-f"]);
        let Commands::Runs {
            command: Some(RunsCommands::Search(args)),
        } = cli.command
        else {
            panic!("expected `runs search`");
        };
        assert_eq!(args.limit.map(NonZeroU64::get), Some(7));
        assert!(args.follow);
    }

    // The two mode switches pick different response modes, so clap rejects
    // the pairing rather than letting the server precedence rules silently
    // ignore one of them.
    #[test]
    fn search_mode_switches_conflict() {
        let parsed = Cli::try_parse_from([
            "snouty", "runs", "search", "RUN", "q", "--check", "--follow",
        ]);
        assert!(parsed.is_err(), "expected --check --follow to conflict");
    }

    // The help is built at runtime so the verb list has one home
    // ([`event_set_dsl::VERBS`]); it must name every verb and stay wrapped —
    // clap prints long_about verbatim, so an over-long line would stick out
    // of the ~78-column help text.
    #[test]
    fn search_long_about_names_every_verb_and_wraps() {
        let about = SEARCH_LONG_ABOUT;
        for verb in crate::event_set_dsl::VERBS {
            assert!(about.contains(verb), "missing verb {verb}");
        }
        for line in about.lines() {
            assert!(line.len() <= 78, "over-long help line: {line}");
        }
    }

    /// `--json` is a global flag, so every long help says what the command
    /// does with it — prints JSON, or ignores the flag. The commands that
    /// warn "--json has no effect" at runtime are the exception: their help
    /// never raises the subject.
    #[test]
    fn every_long_help_says_what_json_does() {
        const NO_JSON: [&str; 5] = ["validate", "completions", "version", "update", "login"];

        fn walk(command: &clap::Command) {
            for sub in command.get_subcommands() {
                if !NO_JSON.contains(&sub.get_name())
                    && let Some(about) = sub.get_long_about()
                {
                    let about = about.to_string();
                    assert!(
                        about.contains("--json"),
                        "`{}` long help says nothing about --json",
                        sub.get_name()
                    );
                }
                walk(sub);
            }
        }

        walk(&<Cli as clap::CommandFactory>::command());
    }
}
