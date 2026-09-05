use color_eyre::eyre::Result;
use serde::{Serialize, Serializer};

use crate::api::{AntithesisApi, ApiVersion, MIN_SEARCH_RELEASE, VersionError};
use crate::attributed_value::AttributedValue;
use crate::auth::AuthenticationInfo;
use crate::compose;
use crate::container;
use crate::features::{self, Feature};
use crate::render::{OutputOptions, render_kv};
use crate::settings::Settings;

/// Outcome of a single health check. Only `Error` fails doctor; `Warn` is
/// surfaced but the run still passes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    Ok,
    Warn,
    Error,
}

/// Severity of an explanatory line printed under a check. Independent of the
/// check's `Status`: an `Ok` check can still carry `Note`s (what a var does),
/// and a failing check pairs its `Error`/`Warning` line with `Note` how-tos.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum Level {
    Note,
    Warning,
    Error,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Level::Note => "NOTE",
            Level::Warning => "WARNING",
            Level::Error => "ERROR",
        }
    }
}

#[derive(Serialize)]
struct Note {
    level: Level,
    text: String,
}

fn status_icon(status: Status) -> console::StyledObject<&'static str> {
    match status {
        Status::Ok => console::style("✓").green(),
        Status::Warn => console::style("⚠").yellow(),
        Status::Error => console::style("✗").red(),
    }
}

fn print_notes(notes: &[Note]) {
    for note in notes {
        let label = match note.level {
            Level::Note => console::style(note.level.label()).dim(),
            Level::Warning => console::style(note.level.label()).yellow(),
            Level::Error => console::style(note.level.label()).red(),
        };
        eprintln!("      {}: {}", label, note.text);
    }
}

/// One health check: local tooling, settings validity, authentication, and
/// API connectivity. The headline `message` states the bare fact; the `notes`
/// carry explanations and how-tos. `name` is a stable machine key for `--json`.
/// Checks own all of doctor's pass/warn/fail semantics — the settings table is
/// purely informational.
#[derive(Serialize)]
struct Check {
    name: &'static str,
    status: Status,
    message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    notes: Vec<Note>,
}

impl Check {
    fn new(name: &'static str, status: Status, message: impl Into<String>) -> Self {
        Self {
            name,
            status,
            message: message.into(),
            notes: Vec::new(),
        }
    }

    fn ok(name: &'static str, message: impl Into<String>) -> Self {
        Self::new(name, Status::Ok, message)
    }

    fn warn(name: &'static str, message: impl Into<String>) -> Self {
        Self::new(name, Status::Warn, message)
    }

    fn fail(name: &'static str, message: impl Into<String>) -> Self {
        Self::new(name, Status::Error, message)
    }

    /// Attach an explanatory line, returning `self` for chaining. Prefer several
    /// short, independent notes over one bundled sentence.
    fn note(mut self, level: Level, text: impl Into<String>) -> Self {
        self.notes.push(Note {
            level,
            text: text.into(),
        });
        self
    }

    fn print(&self) {
        eprintln!("  {} {}", status_icon(self.status), self.message);
        print_notes(&self.notes);
    }
}

/// One row of the resolved-settings table: a setting and the value snouty
/// resolved for it. Purely informational — it carries no status; whether a value
/// is required or optional is a [`Check`] concern.
///
/// `name` is snake_case: `--json` emits it as a key, so
/// `jq .settings.https_proxy` needs no quoting. The human table prints the
/// same string.
struct Setting {
    name: &'static str,
    /// `None` when the setting is not configured. The human table prints a
    /// stand-in word in its place, and `--json` prints `null`, so a script
    /// tests the field instead of matching a sentence.
    value: Option<String>,
}

/// The human table's stand-in for a setting that is not configured.
const NOT_SET: &str = "not set";
/// The same, for the profile: no profile means snouty uses the default one,
/// not that a value is missing.
const DEFAULT_PROFILE: &str = "default";

impl Setting {
    /// A setting snouty resolved to a value.
    fn new(name: &'static str, value: impl Into<String>) -> Self {
        Self {
            name,
            value: Some(value.into()),
        }
    }

    /// A setting that may not be configured.
    fn maybe(name: &'static str, value: Option<impl Into<String>>) -> Self {
        Self {
            name,
            value: value.map(Into::into),
        }
    }

    /// What the human table prints for this setting. An unset one gets the
    /// stand-in word that reads right for it: no profile means the default
    /// profile, whereas no tenant means a gap the user has to fill.
    fn render_value(&self) -> &str {
        self.value.as_deref().unwrap_or(match self.name {
            "profile" => DEFAULT_PROFILE,
            _ => NOT_SET,
        })
    }
}

/// The full doctor report, as emitted by `--json`: the binary `checks` and the
/// informational `settings` table snouty resolved.
#[derive(Serialize)]
struct Report<'a> {
    ok: bool,
    checks: &'a [Check],
    #[serde(serialize_with = "settings_as_map")]
    settings: &'a [Setting],
}

/// Serialize the rows as a JSON object keyed by name, in resolution order, so
/// a caller reads one value with `.settings.tenant`. An unset setting is
/// `null`; the human sentinel stays in the human table.
fn settings_as_map<S: Serializer>(settings: &&[Setting], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_map(settings.iter().map(|s| (s.name, &s.value)))
}

/// The tenant is required by every command, so a missing one fails doctor.
/// (A broken settings file never reaches here — it fails at startup, before
/// doctor runs — so the resolved value is simply present or absent.)
/// `snouty login` sets tenant, repository, and credentials in one pass, so every
/// check that reports one of them missing points at it. Each says so in its own
/// words: on an unconfigured machine all three fire at once, and the same
/// sentence three times reads as noise rather than as advice.
fn tenant_check(tenant: Option<&str>) -> Check {
    match tenant {
        Some(_) => Check::ok("tenant", "tenant is set"),
        None => Check::fail("tenant", "tenant is not set").note(
            Level::Note,
            "run `snouty login` to set it, or set ANTITHESIS_TENANT or add it to a settings file",
        ),
    }
}

/// The repository (a container registry) is only needed to build and push a
/// config image (`snouty launch --config`), so a missing one is a warning, not
/// a failure — read-only use (`snouty runs`, `snouty debug`) doesn't need it.
fn repository_check(repository: Option<&str>) -> Check {
    match repository {
        Some(_) => Check::ok("repository", "repository is set"),
        None => Check::warn("repository", "repository is not set")
            .note(Level::Warning, "repository is needed for `snouty launch`")
            .note(
                Level::Note,
                "run `snouty login` to set it, or set ANTITHESIS_REPOSITORY",
            ),
    }
}

fn authn_checks(sources: &[AttributedValue<AuthenticationInfo>]) -> Vec<Check> {
    let Some((credentials, shadowed)) = sources.split_first() else {
        return vec![missing_credentials_check(
            crate::auth::NO_CREDENTIALS_MESSAGE,
        )];
    };

    let mut checks = match credentials.value() {
        AuthenticationInfo::GithubActionsOidc { .. } => {
            vec![enrich_with_origin(
                Check::ok(
                    "github_actions_oidc_token",
                    "Github Actions OIDC token provided",
                ),
                credentials,
            )]
        }
        AuthenticationInfo::OAuth { .. } => {
            vec![enrich_with_origin(
                Check::ok("oauth_credentials", "OAuth credentials used"),
                credentials,
            )]
        }
        AuthenticationInfo::ApiKey { .. } => {
            vec![enrich_with_origin(
                Check::ok("api_key", "API key provided"),
                credentials,
            )]
        }
        AuthenticationInfo::Password { username, .. } => vec![
            with_credential_remedy(
                Check::warn(
                    CREDENTIALS_CHECK_NAME,
                    "No credentials the API commands accept",
                )
                .note(
                    Level::Warning,
                    "`snouty runs` and other API commands refuse username/password",
                ),
            ),
            enrich_with_origin(
                Check::ok(
                    "basic_auth",
                    format!("Using password credentials for user [{username}]"),
                )
                .note(Level::Warning, crate::auth::PASSWORD_DEPRECATION_SUGGESTION)
                .note(
                    Level::Note,
                    "username/password only enables `snouty launch` and `snouty debug`",
                ),
                credentials,
            ),
        ],
    };

    checks.extend(shadowed_credentials_check(credentials, shadowed));
    checks
}

/// The check that reports a credential shortfall: no credential at all, or
/// only the deprecated username and password.
const CREDENTIALS_CHECK_NAME: &str = "credentials";

fn missing_credentials_check(message: impl Into<String>) -> Check {
    with_credential_remedy(Check::fail(CREDENTIALS_CHECK_NAME, message).note(
        Level::Error,
        "snouty needs credentials to authenticate with Antithesis",
    ))
}

/// The remedy notes for a credential shortfall. Every credential kind except
/// username and password is accepted, so the notes name the ways to get one
/// rather than naming an API key alone.
fn with_credential_remedy(check: Check) -> Check {
    check
        .note(
            Level::Note,
            "run `snouty login` to sign in and store credentials",
        )
        .note(
            Level::Note,
            "or set ANTITHESIS_API_KEY; ask Antithesis support for an API key if you don't have one",
        )
}

/// A warning, not a failure: snouty is authenticated, just not with the
/// credential the user expected.
fn shadowed_credentials_check(
    in_use: &AttributedValue<AuthenticationInfo>,
    shadowed: &[AttributedValue<AuthenticationInfo>],
) -> Option<Check> {
    if shadowed.is_empty() {
        return None;
    }
    let mut check = Check::warn(
        "credential_sources",
        "more than one credential source is available",
    )
    .note(
        Level::Warning,
        format!(
            "snouty uses the {} from {}",
            in_use.value(),
            describe_origin(in_use)
        ),
    );
    for other in shadowed {
        check = check.note(
            Level::Note,
            format!(
                "snouty ignores the {} from {}",
                other.value(),
                describe_origin(other)
            ),
        );
    }
    Some(check.note(
        Level::Note,
        format!("{} to use the next source", drop_action(in_use)),
    ))
}

/// A phrase that reads after "read from" or after the name of a credential
/// kind.
fn describe_origin<T>(attribution: &AttributedValue<T>) -> String {
    match attribution {
        AttributedValue::EnvironmentVariable {
            environment_variable_names,
            ..
        } => format!(
            "the [{}] environment variable{}",
            environment_variable_names.join(", "),
            if environment_variable_names.len() == 1 {
                ""
            } else {
                "s"
            }
        ),
        AttributedValue::SettingsFile {
            settings_file_path,
            profile,
            ..
        } => format!(
            // `Display`, not `Debug`: `{:?}` on a path adds quotes of its
            // own, which read as a second set of brackets around the name.
            "the [{}] profile in {}",
            profile.as_deref().unwrap_or(DEFAULT_PROFILE),
            settings_file_path.display(),
        ),
        AttributedValue::Keychain { entry_name, .. } => {
            format!("system keychain entry named [{entry_name}]")
        }
    }
}

/// An imperative phrase that reads before "to use the next source".
fn drop_action<T>(attribution: &AttributedValue<T>) -> String {
    match attribution {
        AttributedValue::EnvironmentVariable {
            environment_variable_names,
            ..
        } => format!("unset [{}]", environment_variable_names.join(", ")),
        AttributedValue::SettingsFile {
            settings_file_path,
            profile,
            ..
        } => format!(
            "remove the [{}] profile from {}",
            profile.as_deref().unwrap_or(DEFAULT_PROFILE),
            settings_file_path.display(),
        ),
        AttributedValue::Keychain { entry_name, .. } => {
            format!("remove the [{entry_name}] entry from the system keychain")
        }
    }
}

fn enrich_with_origin<T>(check: Check, attribution: &AttributedValue<T>) -> Check {
    check.note(
        Level::Note,
        format!("read from {}", describe_origin(attribution)),
    )
}

/// Binary health checks: local tooling, the required settings
/// (tenant/repository), and authentication. The resolved values themselves are
/// reported separately by [`resolve_settings`].
fn collect_checks(settings: &Settings) -> Vec<Check> {
    let mut checks: Vec<Check> = Vec::new();

    // Container runtime (for building/pushing images). Report the real engine
    // (engine_kind), not the invoking binary (name): for podman-in-disguise the
    // command is `docker` but the engine is podman — and this must agree with
    // the engine `launch`/`validate` announce, which also uses engine_kind.
    match container::runtime(settings) {
        Ok(rt) => checks.push(Check::ok(
            "container_runtime",
            format!("Container runtime: {} detected", rt.engine_kind()),
        )),
        Err(e) => checks.push(
            Check::fail("container_runtime", "Container runtime not detected")
                .note(Level::Error, e.to_string()),
        ),
    }

    // Docker Compose v2 (required for compose configs). Resolves the standalone
    // `docker-compose` binary or the `docker compose` CLI plugin, and reports
    // which one was picked.
    match compose::DockerCompose::probe() {
        Ok((name, version)) => {
            checks.push(Check::ok("docker_compose", format!("{name}: {version}")))
        }
        Err(e) => checks.push(
            Check::fail("docker_compose", "no usable Docker Compose")
                .note(Level::Error, e.to_string()),
        ),
    }

    // Required settings. tenant is needed by every command; repository is
    // launch-only, so a missing one is a warning.
    checks.push(tenant_check(settings.tenant()));
    checks.push(repository_check(settings.repository()));

    // Authentication (synchronous-only by design).
    match AuthenticationInfo::available_ambient_credentials(settings.profile()) {
        Ok(sources) => checks.extend(authn_checks(&sources)),
        Err(err) => checks.push(missing_credentials_check(err.to_string())),
    }

    checks
}

/// The resolved-settings table: the value snouty resolved for each setting and
/// where it came from (env > profile > project/global file). Purely
/// informational — required/optional semantics are reported by [`collect_checks`].
fn resolve_settings(settings: &Settings, features: &[Feature]) -> Vec<Setting> {
    let mut rows = vec![
        Setting::maybe("profile", settings.profile()),
        Setting::maybe("tenant", settings.tenant()),
        Setting::maybe("repository", settings.repository()),
        Setting::maybe("https_proxy", settings.https_proxy()),
        // The explicit override, otherwise auto-detected.
        Setting::new(
            "container_engine",
            settings.container_engine().unwrap_or("auto-detect"),
        ),
        Setting::new("update_channel", settings.update_channel().as_str()),
        Setting::maybe(
            "private_registries",
            match settings.private_registries() {
                [] => None,
                prefixes => Some(
                    prefixes
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
            },
        ),
    ];
    // Only when set: features are opt-in, so an empty row would be noise on
    // every ordinary run.
    if !features.is_empty() {
        let ids: Vec<String> = features.iter().map(Feature::to_string).collect();
        rows.push(Setting::new("features", ids.join(", ")));
    }
    rows
}

/// Print the resolved-settings table, aligned via the shared [`render_kv`] helper
/// (which also sanitizes the values). No status icons — the checks above own
/// pass/warn/fail; this table just reports what snouty resolved, indented to sit
/// under the "Resolved settings" heading.
fn print_settings(settings: &[Setting]) {
    let rows: Vec<(&str, String)> = settings
        .iter()
        .map(|s| (s.name, s.render_value().to_string()))
        .collect();
    for line in render_kv(&rows, 0).lines() {
        eprintln!("  {line}");
    }
}

/// With the `runs-search` unstable feature enabled, `runs events` and
/// `runs search` assume the tenant serves the events-search API instead of
/// probing for it — this check is where that assumption gets verified. Only
/// a confidently-known gap reports: the feature off or an unparsable release
/// version say nothing (the check would guess). Pure so it can be
/// unit-tested without the network.
fn runs_search_release_check(version: &ApiVersion, runs_search_enabled: bool) -> Option<Check> {
    if !runs_search_enabled || version.release? >= MIN_SEARCH_RELEASE {
        return None;
    }
    let (major, minor) = MIN_SEARCH_RELEASE;
    Some(
        Check::fail("runs-search", "tenant serves the events-search API")
            .note(
                Level::Error,
                format!(
                    "the `runs-search` unstable feature is enabled, but tenant release {} \
                     predates the events-search API (added in {major}.{minor}) — \
                     `runs events` and `runs search` will fail",
                    version.release_version
                ),
            )
            .note(
                Level::Note,
                format!(
                    "remove `runs-search` from {} or request that your Antithesis tenant is upgraded",
                    features::UNSTABLE_FEATURES_VAR_NAME
                ),
            ),
    )
}

/// Map a `GET /api/version` probe into a check. Reaching the endpoint at all —
/// even a 404 on an older backend that lacks it — proves connectivity, so only
/// rejected auth, an API error, or a failure to reach the API is a problem.
/// Pure over the result so it can be unit-tested without the network.
fn version_check(host: &str, result: std::result::Result<ApiVersion, VersionError>) -> Check {
    match result {
        Ok(v) => Check::ok("api", "Antithesis API reachable")
            .note(
                Level::Note,
                format!("latest API version: {}", v.latest_api_version),
            )
            .note(
                Level::Note,
                format!("tenant release version: {}", v.release_version),
            ),
        // 404: the version endpoint was added in release 56, so an older tenant
        // 404s — but the request was served, so auth and connectivity are fine.
        // A 404 can also come from a proxy/route in front of the tenant, so we
        // name that possibility rather than asserting the tenant is old.
        Err(VersionError::Http(404)) => Check::ok("api", "Antithesis API reachable").note(
            Level::Warning,
            "GET /api/version returned 404 — your tenant likely predates version \
             reporting (added in release 56); if you expect a current tenant, a \
             proxy or route may be intercepting the request",
        ),
        // 401/403: the request was rejected. Most often the API key is wrong,
        // but a proxy can also reject before the request reaches the API, so we
        // name both rather than blaming the key outright.
        Err(VersionError::Http(status @ (401 | 403))) => {
            Check::fail("api", "Antithesis API rejected authentication")
                .note(Level::Error, format!("the API returned HTTP {status}"))
                .note(
                    Level::Note,
                    "verify ANTITHESIS_API_KEY is valid; if it is, a proxy may be \
                     rejecting the request before it reaches Antithesis",
                )
        }
        // 5xx: we reached something, but it's erroring — connectivity is broken
        // by a server error and auth status is unknown.
        Err(VersionError::Http(status)) if (500..=599).contains(&status) => {
            Check::fail("api", "Antithesis API unavailable").note(
                Level::Error,
                format!("the API returned HTTP {status} (server error)"),
            )
        }
        // Any other unexpected status.
        Err(VersionError::Http(status)) => Check::fail("api", "Antithesis API error").note(
            Level::Error,
            format!("the API returned an unexpected HTTP {status}"),
        ),
        // The server answered, but not with the version payload — the network
        // is fine, so don't send the user debugging connectivity.
        Err(VersionError::BadResponse(err)) => {
            Check::fail("api", "Antithesis API sent an unexpected response").note(
                Level::Error,
                format!("{host} answered, but the response was invalid: {err}"),
            )
        }
        // Couldn't connect at all — connectivity is broken.
        Err(VersionError::Unreachable(err)) => Check::fail("api", "Antithesis API unreachable")
            .note(Level::Error, format!("could not connect to {host}: {err}")),
    }
}

pub async fn cmd_doctor(
    settings: &Settings,
    OutputOptions { json, verbose }: OutputOptions,
    offline: bool,
) -> Result<()> {
    let mut checks = collect_checks(settings);

    // Connectivity + version check (network). Skipped with --offline. Only
    // runs when the resolved credentials work against the full API:
    // /api/version, like every endpoint but launch, rejects username/password
    // auth, so probing it with those credentials would only yield a misleading
    // 403 — and the auth checks above already tell deprecated-credential and
    // unauthenticated users to set a key. The client is built from the
    // resolved settings (base url / tenant), and `verbose` logs the
    // request/response.
    if !offline && let Ok(api) = AntithesisApi::new(settings, verbose) {
        let host = api.host();
        let version = api.get_version().await;
        if let Ok(version) = &version
            && let Some(check) =
                runs_search_release_check(version, features::is_enabled(Feature::RunsSearch))
        {
            checks.push(check);
        }
        checks.push(version_check(&host, version));
    }

    let settings_rows = resolve_settings(settings, &features::enabled());

    // Only the checks carry pass/warn/fail; the settings table is informational.
    let errors = checks.iter().filter(|c| c.status == Status::Error).count();
    let warnings = checks.iter().filter(|c| c.status == Status::Warn).count();

    if json {
        let report = Report {
            ok: errors == 0,
            checks: &checks,
            settings: &settings_rows,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        eprintln!("Checks");
        for check in &checks {
            check.print();
        }
        eprintln!();
        eprintln!("Resolved settings");
        print_settings(&settings_rows);
        eprintln!();

        let wp = if warnings == 1 { "" } else { "s" };
        if errors > 0 {
            let ep = if errors == 1 { "" } else { "s" };
            if warnings > 0 {
                eprintln!(
                    "doctor found {errors} problem{ep} and {warnings} warning{wp} — \
                     see the ✗ and ⚠ checks above"
                );
            } else {
                eprintln!("doctor found {errors} problem{ep} — see the ✗ check{ep} above");
            }
        } else if warnings > 0 {
            eprintln!("doctor passed with {warnings} warning{wp}");
        } else {
            eprintln!("All checks passed");
        }
    }

    // Exit non-zero on failure without re-rendering an error report: the checks
    // above already say exactly what's wrong, so a generic "Error: doctor found
    // problems" footer would be redundant noise.
    if errors > 0 {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::auth::{API_KEY_VAR_NAME, PASSWORD_VAR_NAME, USERNAME_VAR_NAME};
    use crate::cli::UpdateChannel;

    use super::*;

    // ---- auth_checks (env-only auth) -----------------------------------

    #[test]
    fn auth_api_key_set_is_a_single_bare_ok_check() {
        let checks = authn_checks(&[AttributedValue::EnvironmentVariable {
            value: AuthenticationInfo::ApiKey {
                api_key: "api_key".to_owned(),
            },
            environment_variable_names: vec![API_KEY_VAR_NAME],
        }]);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Ok);
        assert!(checks[0].message.contains("API key provided"));
    }

    /// A credential read from a file names the profile it came from and the
    /// path, as a sentence. The path goes through `Display`, so it carries no
    /// quotes of its own.
    #[test]
    fn auth_from_a_file_names_the_profile_and_the_path() {
        let note = |profile: Option<&str>| {
            let checks = authn_checks(&[AttributedValue::SettingsFile {
                value: AuthenticationInfo::ApiKey {
                    api_key: "api_key".to_owned(),
                },
                settings_file_path: std::path::PathBuf::from("/tmp/credentials.toml"),
                profile: profile.map(str::to_owned),
            }]);
            checks[0]
                .notes
                .iter()
                .map(|n| n.text.clone())
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert!(
            note(None).contains("read from the [default] profile in /tmp/credentials.toml"),
            "got: {}",
            note(None)
        );
        assert!(
            note(Some("prod")).contains("read from the [prod] profile in /tmp/credentials.toml"),
            "got: {}",
            note(Some("prod"))
        );
        assert!(!note(None).contains('"'), "the path must not be quoted");
    }

    #[test]
    fn auth_password_warns_on_credentials_and_notes_deprecation() {
        let checks = authn_checks(&[AttributedValue::EnvironmentVariable {
            value: AuthenticationInfo::Password {
                username: "user".to_owned(),
                password: "pass".to_owned(),
            },
            environment_variable_names: vec![USERNAME_VAR_NAME, PASSWORD_VAR_NAME],
        }]);
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].status, Status::Warn);
        assert_eq!(checks[0].name, CREDENTIALS_CHECK_NAME);
        assert!(checks[0].message.contains("No credentials"));
        // The remedy must name `snouty login` too, not an API key alone.
        assert!(
            checks[0]
                .notes
                .iter()
                .any(|n| n.text.contains("snouty login"))
        );
        assert!(checks[0].notes.iter().any(|n| n.level == Level::Warning));
        assert!(
            checks[0]
                .notes
                .iter()
                .any(|n| n.text.contains("ask Antithesis support"))
        );
        assert_eq!(checks[1].status, Status::Ok);
        assert_eq!(checks[1].notes.len(), 3);
        assert!(checks[1].notes.iter().any(|n| n.level == Level::Warning
            && n.text.contains("deprecated")
            && n.text.contains("snouty login")));
        // The deprecated creds steer the user to the only commands they unlock.
        assert!(checks[1].notes.iter().any(|n| n.level == Level::Note
            && n.text.contains("snouty launch")
            && n.text.contains("snouty debug")));
    }

    #[test]
    fn auth_nothing_set_errors_and_only_mentions_api_key() {
        let checks = [missing_credentials_check("PANIC PANIC PANIC")];
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Error);
        assert!(checks[0].message.contains("PANIC PANIC PANIC"));
        assert!(checks[0].notes.iter().any(|n| n.level == Level::Error));
        assert!(
            checks[0]
                .notes
                .iter()
                .any(|n| n.text.contains("ask Antithesis support"))
        );
        assert!(
            checks[0]
                .notes
                .iter()
                .any(|n| n.text.contains("snouty login")),
            "an unconfigured machine must be told the command that configures it"
        );
        let all = format!(
            "{} {}",
            checks[0].message,
            checks[0]
                .notes
                .iter()
                .map(|n| n.text.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        );
        assert!(!all.contains("USERNAME"));
        assert!(!all.contains("PASSWORD"));
    }

    /// A user who meets both messages reads one wording, not two.
    #[test]
    fn auth_no_source_reports_the_shared_no_credentials_message() {
        let checks = authn_checks(&[]);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Error);
        assert_eq!(checks[0].message, crate::auth::NO_CREDENTIALS_MESSAGE);
    }

    fn env_password_source() -> AttributedValue<AuthenticationInfo> {
        AttributedValue::EnvironmentVariable {
            value: AuthenticationInfo::Password {
                username: "user".to_owned(),
                password: "pass".to_owned(),
            },
            environment_variable_names: vec![USERNAME_VAR_NAME, PASSWORD_VAR_NAME],
        }
    }

    fn file_api_key_source() -> AttributedValue<AuthenticationInfo> {
        AttributedValue::SettingsFile {
            value: AuthenticationInfo::ApiKey {
                api_key: "api_key".to_owned(),
            },
            settings_file_path: std::path::PathBuf::from("/tmp/credentials.toml"),
            profile: None,
        }
    }

    fn note_text(check: &Check) -> String {
        check
            .notes
            .iter()
            .map(|n| n.text.as_str())
            .collect::<Vec<_>>()
            .join(" | ")
    }

    #[test]
    fn one_credential_source_reports_no_conflict() {
        let checks = authn_checks(&[file_api_key_source()]);
        assert!(!checks.iter().any(|c| c.name == "credential_sources"));
    }

    /// The case issue #292 reported: `snouty login` stored an API key while a
    /// legacy username/password was still exported.
    #[test]
    fn a_shadowed_credential_is_reported_with_the_action_that_frees_it() {
        let checks = authn_checks(&[env_password_source(), file_api_key_source()]);
        let check = checks
            .iter()
            .find(|c| c.name == "credential_sources")
            .expect("the conflict is reported");
        assert_eq!(check.status, Status::Warn);
        let notes = note_text(check);
        assert!(
            notes.contains(
                "snouty uses the username and password from the \
                 [ANTITHESIS_USERNAME, ANTITHESIS_PASSWORD] environment variables"
            ),
            "got: {notes}"
        );
        assert!(
            notes.contains(
                "snouty ignores the API key from the [default] profile in /tmp/credentials.toml"
            ),
            "got: {notes}"
        );
        assert!(
            notes.contains("unset [ANTITHESIS_USERNAME, ANTITHESIS_PASSWORD]"),
            "got: {notes}"
        );
    }

    #[test]
    fn the_action_that_frees_a_credential_matches_its_origin() {
        let keychain = AttributedValue::Keychain {
            value: AuthenticationInfo::ApiKey {
                api_key: "api_key".to_owned(),
            },
            entry_name: "_default_".to_owned(),
        };
        let checks = authn_checks(&[keychain, env_password_source()]);
        let check = checks
            .iter()
            .find(|c| c.name == "credential_sources")
            .expect("the conflict is reported");
        let notes = note_text(check);
        assert!(
            notes.contains("remove the [_default_] entry from the system keychain"),
            "got: {notes}"
        );
        assert!(
            notes.contains("snouty ignores the username and password"),
            "got: {notes}"
        );
    }

    // ---- required-settings checks --------------------------------------

    #[test]
    fn tenant_check_is_ok_when_resolved() {
        let check = tenant_check(Some("acme"));
        assert_eq!(check.status, Status::Ok);
    }

    #[test]
    fn tenant_check_fails_when_missing() {
        let check = tenant_check(None);
        assert_eq!(check.status, Status::Error);
        assert!(
            check
                .notes
                .iter()
                .any(|n| n.text.contains("ANTITHESIS_TENANT"))
        );
        assert!(
            check.notes.iter().any(|n| n.text.contains("snouty login")),
            "an unset tenant must point at the command that sets it"
        );
    }

    #[test]
    fn repository_check_is_ok_when_resolved() {
        let check = repository_check(Some("acme/repo"));
        assert_eq!(check.status, Status::Ok);
    }

    #[test]
    fn repository_check_only_warns_when_missing() {
        // Following main's #147 decision: repository is launch-only, so a missing
        // one is a warning, not a failure.
        let check = repository_check(None);
        assert_eq!(check.status, Status::Warn);
        assert!(check.notes.iter().any(|n| n.text.contains("snouty launch")));
        assert!(check.notes.iter().any(|n| n.text.contains("snouty login")));
    }

    // ---- resolved-settings table (informational, no status) ------------

    fn row<'a>(rows: &'a [Setting], name: &str) -> &'a Setting {
        rows.iter().find(|s| s.name == name).expect("row present")
    }

    #[test]
    fn tenant_row_shows_value() {
        let settings = Settings::builder().tenant("acme").build();
        let rows = resolve_settings(&settings, &[]);
        assert_eq!(row(&rows, "tenant").render_value(), "acme");
    }

    #[test]
    fn missing_settings_render_as_not_set() {
        let rows = resolve_settings(&Settings::default(), &[]);
        assert_eq!(row(&rows, "tenant").render_value(), "not set");
    }

    #[test]
    fn https_proxy_row_shows_value() {
        let settings = Settings::builder()
            .https_proxy("http://proxy.corp:8080")
            .build();
        let rows = resolve_settings(&settings, &[]);
        assert_eq!(
            row(&rows, "https_proxy").render_value(),
            "http://proxy.corp:8080"
        );
    }

    #[test]
    fn https_proxy_row_defaults_to_not_set() {
        let rows = resolve_settings(&Settings::default(), &[]);
        assert_eq!(row(&rows, "https_proxy").render_value(), "not set");
    }

    #[test]
    fn container_engine_row_auto_detects_when_unset() {
        let settings = Settings::builder().tenant("acme").build();
        let rows = resolve_settings(&settings, &[]);
        assert_eq!(row(&rows, "container_engine").render_value(), "auto-detect");
    }

    #[test]
    fn features_row_appears_only_when_a_feature_is_on() {
        let rows = resolve_settings(&Settings::default(), &[]);
        assert!(!rows.iter().any(|r| r.name == "features"));

        let rows = resolve_settings(
            &Settings::default(),
            &[Feature::RunsExec, Feature::Unknown("other".to_string())],
        );
        let row = rows
            .iter()
            .find(|r| r.name == "features")
            .expect("the row appears when a feature is on");
        // An id this build doesn't know is echoed, not dropped.
        assert_eq!(row.render_value(), "runs-exec, other");
    }

    #[test]
    fn private_registries_row_lists_every_prefix() {
        let rows = resolve_settings(&Settings::default(), &[]);
        assert_eq!(row(&rows, "private_registries").render_value(), "not set");

        let settings = Settings::builder()
            .private_registries(vec![
                "ghcr.io/acme".parse().unwrap(),
                "quay.io".parse().unwrap(),
            ])
            .build();
        let rows = resolve_settings(&settings, &[]);
        assert_eq!(
            row(&rows, "private_registries").render_value(),
            "ghcr.io/acme, quay.io"
        );
    }

    #[test]
    fn update_channel_row_defaults_to_stable() {
        let rows = resolve_settings(&Settings::default(), &[]);
        assert_eq!(row(&rows, "update_channel").render_value(), "stable");
    }

    #[test]
    fn update_channel_row_shows_value() {
        let settings = Settings::builder()
            .update_channel(UpdateChannel::Unstable)
            .build();
        let rows = resolve_settings(&settings, &[]);
        assert_eq!(row(&rows, "update_channel").render_value(), "unstable");
    }

    #[test]
    fn profile_row_reflects_no_active_profile() {
        let rows = resolve_settings(&Settings::default(), &[]);
        assert_eq!(row(&rows, "profile").render_value(), "default");
    }

    // ---- version_check (network probe) ---------------------------------

    #[test]
    fn runs_search_release_check_fires_only_on_a_known_gap() {
        let version = |release: &str| ApiVersion::new("v1".into(), release.into());
        // Feature off: nothing, whatever the release.
        assert!(runs_search_release_check(&version("56.0"), false).is_none());
        // Recent enough (58.11 ships the endpoint): nothing.
        assert!(runs_search_release_check(&version("58.11"), true).is_none());
        assert!(runs_search_release_check(&version("60.1"), true).is_none());
        // Too old: the check fails doctor, names the gap, and suggests
        // turning the feature off.
        let check = runs_search_release_check(&version("58.6"), true).unwrap();
        assert_eq!(check.status, Status::Error);
        assert!(
            check.notes[0].text.contains("58.6"),
            "{}",
            check.notes[0].text
        );
        assert!(
            check.notes[1].text.contains("remove `runs-search`"),
            "{}",
            check.notes[1].text
        );
        // An unparsable release says nothing rather than guessing.
        assert!(runs_search_release_check(&version("unknown"), true).is_none());
    }

    #[test]
    fn version_ok_reports_both_versions() {
        let check = version_check(
            "tenant.antithesis.com",
            Ok(ApiVersion::new("v1".into(), "56.0".into())),
        );
        assert_eq!(check.status, Status::Ok);
        let notes = check
            .notes
            .iter()
            .map(|n| n.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(notes.contains("v1"));
        assert!(notes.contains("56.0"));
    }

    #[test]
    fn version_404_is_reachable_but_warns() {
        let check = version_check("tenant.antithesis.com", Err(VersionError::Http(404)));
        assert_eq!(check.status, Status::Ok);
        assert!(check.message.contains("reachable"));
        // The warning explains the 404 without definitively blaming an old
        // tenant — it names both the old-tenant and proxy possibilities.
        let warning = check
            .notes
            .iter()
            .find(|n| n.level == Level::Warning)
            .expect("a 404 should attach a warning note");
        assert!(warning.text.contains("404"));
        assert!(warning.text.contains("proxy"));
    }

    #[test]
    fn version_auth_rejection_reports_the_status_code() {
        for status in [401, 403] {
            let check = version_check("tenant.antithesis.com", Err(VersionError::Http(status)));
            assert_eq!(check.status, Status::Error);
            assert!(
                check
                    .notes
                    .iter()
                    .any(|n| n.text.contains(&status.to_string()))
            );
            assert!(
                check
                    .notes
                    .iter()
                    .any(|n| n.text.contains("ANTITHESIS_API_KEY"))
            );
        }
    }

    #[test]
    fn version_bad_response_is_not_reported_as_unreachable() {
        let check = version_check(
            "tenant.antithesis.com",
            Err(VersionError::BadResponse(
                "invalid type: null, expected struct ApiVersion".into(),
            )),
        );
        assert_eq!(check.status, Status::Error);
        assert!(check.message.contains("unexpected response"));
        assert!(!check.message.contains("unreachable"));
        assert!(check.notes.iter().any(|n| {
            n.text.contains("tenant.antithesis.com")
                && n.text
                    .contains("invalid type: null, expected struct ApiVersion")
        }));
    }

    #[test]
    fn version_unreachable_names_the_host_and_includes_the_error() {
        let check = version_check(
            "tenant.antithesis.com",
            Err(VersionError::Unreachable("connection refused".into())),
        );
        assert_eq!(check.status, Status::Error);
        assert!(check.message.contains("unreachable"));
        assert!(check.notes.iter().any(|n| {
            n.text
                .contains("could not connect to tenant.antithesis.com")
        }));
        assert!(
            check
                .notes
                .iter()
                .any(|n| n.text.contains("connection refused"))
        );
    }

    #[test]
    fn version_5xx_is_unavailable_with_status() {
        let check = version_check("tenant.antithesis.com", Err(VersionError::Http(503)));
        assert_eq!(check.status, Status::Error);
        assert!(check.message.contains("unavailable"));
        assert!(check.notes.iter().any(|n| n.text.contains("503")));
    }

    #[test]
    fn version_unexpected_status_is_an_error_with_status() {
        let check = version_check("tenant.antithesis.com", Err(VersionError::Http(429)));
        assert_eq!(check.status, Status::Error);
        assert!(check.notes.iter().any(|n| n.text.contains("429")));
    }

    // ---- JSON report ----------------------------------------------------

    #[test]
    fn json_report_carries_checks_and_informational_settings() {
        let checks = vec![missing_credentials_check("PANIC PANIC PANIC")];
        let settings = vec![
            Setting::new("tenant", "acme"),
            Setting::maybe("https_proxy", None::<String>),
        ];
        let report = Report {
            ok: false,
            checks: &checks,
            settings: &settings,
        };
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["checks"][0]["name"], "credentials");
        assert_eq!(value["checks"][0]["status"], "error");
        assert_eq!(value["checks"][0]["notes"][0]["level"], "error");
        assert_eq!(value["settings"]["tenant"], "acme");
        // An unset setting is JSON `null`, never the human sentinel — a script
        // reading `.settings.https_proxy` must not see a truthy string.
        assert!(value["settings"]["https_proxy"].is_null());
        assert_eq!(value["settings"].as_object().unwrap().len(), 2);
    }
}
