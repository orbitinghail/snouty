use std::{
    fs,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

use color_eyre::eyre::{Context, OptionExt, Result, eyre};
use tempfile::NamedTempFile;
use toml::{Table, Value};

use crate::cli::UpdateChannel;
use crate::container::RegistryPrefix;
use crate::env;
use crate::error::user_error;

pub const ANTITHESIS_PROFILE_ENV_VAR_NAME: &str = "ANTITHESIS_PROFILE";
pub const SNOUTY_SETTINGS_PATH_VAR_NAME: &str = "SNOUTY_SETTINGS_PATH";
pub const ANTITHESIS_TENANT_VAR_NAME: &str = "ANTITHESIS_TENANT";
pub const ANTITHESIS_REPOSITORY_VAR_NAME: &str = "ANTITHESIS_REPOSITORY";
pub const ANTITHESIS_BASE_URL_VAR_NAME: &str = "ANTITHESIS_BASE_URL";
pub const ANTITHESIS_HTTPS_PROXY_VAR_NAME: &str = "ANTITHESIS_HTTPS_PROXY";
pub const CONTAINER_ENGINE_VAR_NAME: &str = "SNOUTY_CONTAINER_ENGINE";
pub const UPDATE_CHANNEL_VAR_NAME: &str = "SNOUTY_UPDATE_CHANNEL";
pub const API_CACHE_MAX_FILE_SIZE_VAR_NAME: &str = "SNOUTY_API_CACHE_MAX_FILE_SIZE";
pub const API_CACHE_RESPECT_HEADERS_VAR_NAME: &str = "SNOUTY_API_CACHE_RESPECT_HEADERS";
pub const PRIVATE_REGISTRIES_VAR_NAME: &str = "SNOUTY_PRIVATE_REGISTRIES";
const PROJECT_SETTINGS_FILENAME: &str = ".snouty.toml";
const GLOBAL_SETTINGS_FILENAME: &str = "settings.toml";
const PROFILE_KEY: &str = "profile";

/// snouty's subdirectory under an XDG base dir: `$<xdg_var>/snouty`, falling
/// back to `$HOME/<home_subdir>/snouty`. `None` when neither is set (e.g.
/// Windows). Deliberately hand-rolled rather than via the `dirs` crate, which on
/// macOS resolves to `~/Library/...` instead of the XDG layout snouty wants.
///
/// Reads through [`env::var`], so an exported-but-empty `XDG_*`/`HOME` is treated
/// as unset (per the XDG spec) rather than yielding a bogus relative path; a
/// non-Unicode value is likewise treated as unset here rather than aborting the
/// command.
fn xdg_snouty_dir(xdg_var: &str, home_subdir: &str) -> Option<PathBuf> {
    xdg_base(
        env::var(xdg_var).ok().flatten(),
        env::var("HOME").ok().flatten(),
        home_subdir,
    )
}

/// The `$base/snouty` directory given already-resolved (empty-collapsed) env
/// values: the XDG dir if set, else `$HOME/<home_subdir>`; `None` when neither
/// is set. Pure, so the XDG-vs-`HOME` fallback is unit-testable without touching
/// the process environment.
fn xdg_base(xdg_dir: Option<String>, home: Option<String>, home_subdir: &str) -> Option<PathBuf> {
    let base = match xdg_dir {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(home?).join(home_subdir),
    };
    Some(base.join("snouty"))
}

/// Directory holding the global settings file: `$XDG_CONFIG_HOME/snouty`,
/// falling back to `$HOME/.config/snouty`. `None` only when neither
/// `XDG_CONFIG_HOME` nor `HOME` is set (e.g. Windows).
pub fn global_settings_dir() -> Option<PathBuf> {
    xdg_snouty_dir("XDG_CONFIG_HOME", ".config")
}

pub fn cache_dir() -> Option<PathBuf> {
    xdg_snouty_dir("XDG_CACHE_HOME", ".cache")
}

/// snouty's resolved settings.
///
/// Everything is resolved eagerly in [`Settings::resolve`]: the environment,
/// the project settings file (`.snouty.toml`), and the global `settings.toml`
/// are read once, each setting is resolved through the precedence chain, and
/// the result is plain owned data. A settings file that exists but can't be read
/// or parsed is a hard error at construction — so by the time a `Settings`
/// exists, every value is either resolved or simply absent. No value is
/// recomputed and nothing fails later, which is what lets the accessors hand out
/// `&str`/`Option<&str>` instead of cached `Result`s.
///
/// Every command shares the same resolved instance (threaded by reference), so a
/// value resolves identically no matter which code path reads it.
///
/// `Default` is every setting unset (with the `stable` update channel and the
/// default cache size cap) — handy when a caller needs a `Settings` without
/// resolving anything.
pub struct Settings {
    profile: Option<String>,
    tenant: Option<String>,
    repository: Option<String>,
    base_url: Option<String>,
    https_proxy: Option<String>,
    container_engine: Option<String>,
    update_channel: UpdateChannel,
    api_cache_max_file_size: u64,
    api_cache_respect_headers: bool,
    private_registries: Vec<RegistryPrefix>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings::builder().build()
    }
}

impl Settings {
    /// Resolve settings from the environment, the project settings file, and the
    /// global `settings.toml`.
    ///
    /// The project settings file is located by, in descending precedence:
    /// `SNOUTY_SETTINGS_PATH`, the `--settings` flag (`project_settings_path`),
    /// then `./.snouty.toml` in the current working directory. The active profile
    /// is the `--profile` flag if set, otherwise `ANTITHESIS_PROFILE`.
    ///
    /// A settings file that exists but cannot be read or parsed is a hard error,
    /// as is an explicitly-requested file (via flag or env var) that is missing:
    /// silently ignoring it would resurface later as a confusing "tenant not
    /// set". A missing *default* `./.snouty.toml` is fine.
    pub fn resolve(
        project_settings_path: Option<PathBuf>,
        profile: Option<String>,
    ) -> Result<Self> {
        // An empty value means "unset", not "a profile named the empty string".
        // `env::var` already collapses an empty env var to `None`; the trailing
        // filter also covers an explicitly-empty `--profile ""` flag.
        let profile = match profile {
            Some(flag) => Some(flag),
            None => env::var(ANTITHESIS_PROFILE_ENV_VAR_NAME)?,
        }
        .filter(|profile| !profile.is_empty());

        // Likewise an empty `SNOUTY_SETTINGS_PATH` is "unset" (it would otherwise
        // become an explicitly-requested empty path that must — and can't — exist).
        let project_settings_path = match project_settings_path {
            Some(path) => Some(path),
            None => env::var(SNOUTY_SETTINGS_PATH_VAR_NAME)?.map(PathBuf::from),
        };

        // An explicitly-requested project file must exist; the default
        // `./.snouty.toml` is optional. (Climbing the directory tree, if ever
        // wanted, would go here.)
        let project = match &project_settings_path {
            Some(path) => load_settings_file(path, true)?,
            None => load_settings_file(Path::new(PROJECT_SETTINGS_FILENAME), false)?,
        };
        let global = match global_settings_dir() {
            Some(dir) => load_settings_file(&dir.join(GLOBAL_SETTINGS_FILENAME), false)?,
            None => None,
        };

        let resolve = |key: &str, env_var: &str| {
            resolve_value(
                key,
                env_var,
                profile.as_deref(),
                project.as_ref(),
                global.as_ref(),
            )
        };

        let tenant = resolve("tenant", ANTITHESIS_TENANT_VAR_NAME)?;
        let repository = resolve("repository", ANTITHESIS_REPOSITORY_VAR_NAME)?;
        let base_url = resolve("base_url", ANTITHESIS_BASE_URL_VAR_NAME)?;
        let https_proxy = resolve("https_proxy", ANTITHESIS_HTTPS_PROXY_VAR_NAME)?;
        let container_engine = resolve("container_engine", CONTAINER_ENGINE_VAR_NAME)?;

        // The channel resolves to a typed value here, so an invalid setting
        // fails at startup like any other malformed setting.
        let update_channel = match resolve("update_channel", UPDATE_CHANNEL_VAR_NAME)? {
            Some(value) => value
                .parse::<UpdateChannel>()
                .map_err(|err| user_error(format!("invalid update_channel setting: {err}")))?,
            None => UpdateChannel::default(),
        };

        // Likewise a typed value: the largest API response body the response
        // cache will store, as a size such as "10 MB" or a bare byte count.
        // A bare count is naturally written as a TOML integer, so the file
        // layers accept one (see `resolve_integer_value`).
        let api_cache_max_file_size = resolve_integer_value(
            "api_cache_max_file_size",
            API_CACHE_MAX_FILE_SIZE_VAR_NAME,
            profile.as_deref(),
            project.as_ref(),
            global.as_ref(),
        )?
        .map(|value| parse_byte_size("api_cache_max_file_size", &value))
        .transpose()?;

        // A typed boolean: whether the response cache requires the server's
        // cache headers to allow caching. Set false to fall back to the
        // handlers' logical admission checks alone (see `crate::api_cache`).
        let api_cache_respect_headers = resolve_boolean_value(
            "api_cache_respect_headers",
            API_CACHE_RESPECT_HEADERS_VAR_NAME,
            profile.as_deref(),
            project.as_ref(),
            global.as_ref(),
        )?
        .map(|value| parse_boolean("api_cache_respect_headers", &value))
        .transpose()?;

        // A typed list: the registries a test run cannot pull from. The file
        // layers accept a TOML array; the environment variable and a quoted
        // string separate the entries with commas.
        let private_registries = resolve_list_value(
            "private_registries",
            PRIVATE_REGISTRIES_VAR_NAME,
            profile.as_deref(),
            project.as_ref(),
            global.as_ref(),
        )?
        .map(|value| parse_registry_prefixes("private_registries", &value))
        .transpose()?;

        // A derived base URL interpolates the tenant into the request host
        // (`https://{tenant}.antithesis.com`) and we attach the API key to that
        // host, so a malformed tenant would silently send credentials to an
        // unintended endpoint. Validate it as a hostname before deriving. An
        // explicit base_url bypasses the tenant, so only check when we'd derive.
        if base_url.is_none()
            && let Some(tenant) = &tenant
        {
            validate_tenant_host(tenant)?;
        }

        // A named-field literal (not the setters) so the compiler flags this
        // site whenever a new setting is added to the builder.
        Ok(SettingsBuilder {
            profile,
            tenant,
            repository,
            base_url,
            https_proxy,
            container_engine,
            update_channel,
            api_cache_max_file_size,
            api_cache_respect_headers,
            private_registries,
        }
        .build())
    }

    /// The resolved tenant, or `None` if unset. Call sites that require it turn
    /// the `None` into an error (see [`require`]); doctor reports it as-is.
    pub fn tenant(&self) -> Option<&str> {
        self.tenant.as_deref()
    }

    /// The resolved container registry, or `None` if unset.
    pub fn repository(&self) -> Option<&str> {
        self.repository.as_deref()
    }

    /// The base URL to talk to: an explicit `base_url` if set, otherwise one
    /// derived from the tenant; `None` when neither exists (a derived `base_url`
    /// is present exactly when the tenant is).
    pub fn base_url(&self) -> Option<&str> {
        self.base_url.as_deref()
    }

    pub fn https_proxy(&self) -> Option<&str> {
        self.https_proxy.as_deref()
    }

    pub fn container_engine(&self) -> Option<&str> {
        self.container_engine.as_deref()
    }

    /// The resolved update channel; `stable` when unset. Already validated —
    /// an invalid setting value fails in [`Settings::resolve`].
    pub fn update_channel(&self) -> UpdateChannel {
        self.update_channel
    }

    /// The largest API response body, in bytes, the response cache stores;
    /// [`crate::api_cache::DEFAULT_MAX_FILE_SIZE`] when unset. Already
    /// validated — an invalid setting value fails in [`Settings::resolve`].
    pub fn api_cache_max_file_size(&self) -> u64 {
        self.api_cache_max_file_size
    }

    /// Whether the response cache requires the server's cache headers to
    /// allow caching; true when unset. Already validated — an invalid setting
    /// value fails in [`Settings::resolve`].
    pub fn api_cache_respect_headers(&self) -> bool {
        self.api_cache_respect_headers
    }

    /// The registries a test run cannot pull from; empty when unset. Already
    /// validated — an invalid setting value fails in [`Settings::resolve`].
    pub fn private_registries(&self) -> &[RegistryPrefix] {
        &self.private_registries
    }

    pub(crate) fn profile(&self) -> Option<&str> {
        self.profile.as_deref()
    }

    /// Start building a `Settings` from explicit values, with no environment
    /// or filesystem IO — for callers (and tests) that already hold the
    /// values. Set only what's needed and finish with
    /// [`SettingsBuilder::build`].
    pub fn builder() -> SettingsBuilder {
        SettingsBuilder::default()
    }

    /// Test-only: a `Settings` with an explicit base URL and everything else
    /// unset, for driving [`crate::api::AntithesisApi`] against a mock server
    /// without touching the environment.
    #[cfg(test)]
    pub(crate) fn for_test_base_url(base_url: String) -> Self {
        Self::builder().base_url(&base_url).build()
    }
}

/// Builder behind [`Settings::builder`], so call sites name just the values
/// they set instead of growing a positional argument for every new setting.
/// It bypasses the resolution precedence chain entirely; the derived values
/// still apply in [`SettingsBuilder::build`].
#[derive(Default)]
pub struct SettingsBuilder {
    profile: Option<String>,
    tenant: Option<String>,
    repository: Option<String>,
    base_url: Option<String>,
    https_proxy: Option<String>,
    container_engine: Option<String>,
    update_channel: UpdateChannel,
    api_cache_max_file_size: Option<u64>,
    api_cache_respect_headers: Option<bool>,
    private_registries: Option<Vec<RegistryPrefix>>,
}

impl SettingsBuilder {
    pub fn profile(mut self, value: &str) -> Self {
        self.profile = Some(value.to_string());
        self
    }

    pub fn tenant(mut self, value: &str) -> Self {
        self.tenant = Some(value.to_string());
        self
    }

    pub fn repository(mut self, value: &str) -> Self {
        self.repository = Some(value.to_string());
        self
    }

    pub fn base_url(mut self, value: &str) -> Self {
        self.base_url = Some(value.to_string());
        self
    }

    pub fn https_proxy(mut self, value: &str) -> Self {
        self.https_proxy = Some(value.to_string());
        self
    }

    pub fn container_engine(mut self, value: &str) -> Self {
        self.container_engine = Some(value.to_string());
        self
    }

    pub fn update_channel(mut self, value: UpdateChannel) -> Self {
        self.update_channel = value;
        self
    }

    pub fn api_cache_max_file_size(mut self, value: u64) -> Self {
        self.api_cache_max_file_size = Some(value);
        self
    }

    pub fn api_cache_respect_headers(mut self, value: bool) -> Self {
        self.api_cache_respect_headers = Some(value);
        self
    }

    pub fn private_registries(mut self, value: Vec<RegistryPrefix>) -> Self {
        self.private_registries = Some(value);
        self
    }

    /// Finish the build, applying the derived values: `base_url` falls back to
    /// a tenant-derived host, and the cache size cap falls back to its default.
    /// [`Settings::resolve`] and the test constructors both finish here, so the
    /// derivation is exercised the same way everywhere.
    pub fn build(self) -> Settings {
        let base_url = self.base_url.or_else(|| {
            self.tenant
                .as_ref()
                .map(|tenant| format!("https://{tenant}.antithesis.com"))
        });

        Settings {
            profile: self.profile,
            tenant: self.tenant,
            repository: self.repository,
            base_url,
            https_proxy: self.https_proxy,
            container_engine: self.container_engine,
            update_channel: self.update_channel,
            api_cache_max_file_size: self
                .api_cache_max_file_size
                .unwrap_or(crate::api_cache::DEFAULT_MAX_FILE_SIZE),
            api_cache_respect_headers: self.api_cache_respect_headers.unwrap_or(true),
            private_registries: self.private_registries.unwrap_or_default(),
        }
    }
}

/// Writes the given fields into the global `settings.toml`, returning the path it
/// wrote so callers (e.g. `snouty login`) can tell the user where it landed.
pub(crate) fn update_settings_in_global_file(
    tenant: Option<String>,
    repository: Option<String>,
    base_url: Option<String>,
    container_engine: Option<String>,
    profile_to_update: Option<&str>,
) -> Result<PathBuf> {
    let settings_dir = global_settings_dir().ok_or_eyre("Could not determine global settings directory. Ensure either $XDG_CONFIG_HOME or $HOME is set.")?;
    let path = settings_dir.join(GLOBAL_SETTINGS_FILENAME);
    let mut contents = match read_to_string_if_file_exists(&path)? {
        Some(contents) => match parse_settings(&contents, &path) {
            Ok(table) => table,
            Err(_) => {
                let backup = back_up_unparsable_file(&path)?;
                // Paths go on their own lines: they are the two things the
                // user may need to copy, and they are what made the one-line
                // form overflow any terminal.
                eprintln!(
                    "note: the existing settings file could not be parsed; a new one will be written.\n  kept as a backup: {}\n  will be rewritten: {}",
                    backup.display(),
                    path.display(),
                );
                Table::new()
            }
        },
        None => Table::new(),
    };

    if let Some(profile) = profile_to_update {
        let profiles = contents
            .entry(PROFILE_KEY)
            .or_insert_with(|| Value::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| eyre!(
                "The settings file at {:?} is malformed: `profile` should be a table of named profiles",
                &path
            ))?;
        update_table(
            entry_as_table_mut(profiles, profile),
            tenant,
            repository,
            base_url,
            container_engine,
        );
    } else {
        update_table(
            &mut contents,
            tenant,
            repository,
            base_url,
            container_engine,
        );
    }

    mkdir(&settings_dir, true, 0o700)?;
    let mut temp = NamedTempFile::new_in(&settings_dir)?;
    temp.write_all(toml::to_string_pretty(&contents)?.as_bytes())?;

    temp.persist(&path)?;

    Ok(path)
}

fn entry_as_table_mut<'a>(table: &'a mut Table, key: &str) -> &'a mut Table {
    let slot = table
        .entry(key.to_owned())
        .or_insert_with(|| Value::Table(Table::new()));
    if slot.as_table_mut().is_none() {
        *slot = Value::Table(Table::new());
    }
    slot.as_table_mut()
        .expect("slot was just ensured to hold a table")
}

pub(crate) fn mkdir(path: &Path, recursive: bool, permissions: u32) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(permissions);
    }
    builder.recursive(recursive).create(path)?;
    Ok(())
}

fn insert_key_if_non_empty(target: &mut Table, key: &str, value: Option<String>) {
    if let Some(value) = value
        && !value.is_empty()
    {
        target.insert(key.to_owned(), Value::String(value));
    }
}

fn update_table(
    target: &mut Table,
    tenant: Option<String>,
    repository: Option<String>,
    base_url: Option<String>,
    container_engine: Option<String>,
) {
    insert_key_if_non_empty(target, "tenant", tenant);
    insert_key_if_non_empty(target, "repository", repository);
    insert_key_if_non_empty(target, "base_url", base_url);
    insert_key_if_non_empty(target, "container_engine", container_engine);
}

/// Validate that `tenant` is safe to interpolate into the derived base URL
/// `https://{tenant}.antithesis.com`. The tenant becomes the request host, so
/// it must be a valid DNS hostname — one or more labels of ASCII letters,
/// digits, and hyphens (hyphens not leading/trailing). This rejects values
/// carrying URL-significant characters (`/`, `#`, `?`, `@`, `:`, whitespace,
/// …) that would otherwise redirect requests — with the API key attached — to
/// an unintended host. Dots are allowed so a multi-label tenant still works;
/// set `ANTITHESIS_BASE_URL` directly for any URL this rejects.
pub(crate) fn validate_tenant_host(tenant: &str) -> Result<()> {
    fn is_valid_label(label: &str) -> bool {
        !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    }

    let valid = !tenant.is_empty() && tenant.len() <= 253 && tenant.split('.').all(is_valid_label);
    if !valid {
        return Err(user_error(format!(
            "invalid tenant `{tenant}`: a tenant must be a valid hostname \
             (letters, digits, and hyphens) because it becomes the host in \
             `https://{tenant}.antithesis.com`. Set ANTITHESIS_BASE_URL directly \
             if you need a custom API URL."
        )));
    }
    Ok(())
}

/// Turn a missing required setting into snouty's standard "run doctor" error.
/// The `Option` accessors stay the single source of truth; the few call sites
/// that cannot proceed without a value funnel the `None` through here, so the
/// message lives in one place instead of being duplicated per accessor.
pub fn require<'a>(value: Option<&'a str>, setting: &str) -> Result<&'a str> {
    value
        .ok_or_else(|| eyre!("Could not resolve Antithesis {setting}. Run snouty doctor to debug."))
}

/// Parse settings-file contents into a TOML table, attributing parse errors to
/// the file's path.
fn parse_settings(contents: &str, path: &Path) -> Result<Table> {
    contents
        .parse::<Table>()
        .map_err(|err| eyre!("Settings file at {:?} was not valid TOML: {err:#}", path))
}

/// Load and parse a settings file. `Ok(None)` when the file simply does not
/// exist and was not explicitly requested; an error when it exists but cannot be
/// read or parsed, or when an explicitly-`required` file is missing.
fn load_settings_file(path: &Path, required: bool) -> Result<Option<Table>> {
    let contents = match read_to_string_if_file_exists(path)? {
        Some(contents) => contents,
        None if !required => return Ok(None),
        None => return Err(eyre!("Settings file at {:?} was not found", path)),
    };
    parse_settings(&contents, path).map(Some)
}

/// Reads a setting out of one TOML table: [`string_value`] for text settings,
/// [`integer_value`] for numeric ones, [`boolean_value`] for flags,
/// [`list_value`] for lists. Environment variables are always plain text, so
/// this only affects the file layers.
type ValueReader = fn(&Table, &str, &str) -> Result<Option<String>>;

/// Resolve a single text setting with the precedence: environment variable,
/// then the active profile (project file before global file), then top-level
/// defaults (project file before global file). The first layer that has the
/// key wins.
///
/// A layer that *has* the key but with a non-string value (or a malformed
/// `profile` section) is a hard error rather than a silent skip — a typo like
/// `tenant = 123` should be reported, not quietly ignored in favour of a
/// lower-precedence value.
fn resolve_value(
    key: &str,
    env_var: &str,
    profile: Option<&str>,
    project: Option<&Table>,
    global: Option<&Table>,
) -> Result<Option<String>> {
    resolve_value_with(string_value, key, env_var, profile, project, global)
}

/// [`resolve_value`] for a numeric setting: the file layers accept a bare TOML
/// integer (the natural form for a number) as well as a quoted string. The
/// value still comes back as text — the caller owns the parse and its error
/// message, exactly as for an environment variable.
fn resolve_integer_value(
    key: &str,
    env_var: &str,
    profile: Option<&str>,
    project: Option<&Table>,
    global: Option<&Table>,
) -> Result<Option<String>> {
    resolve_value_with(integer_value, key, env_var, profile, project, global)
}

/// [`resolve_value`] for a boolean setting: the file layers accept a bare
/// TOML boolean as well as a quoted string. The value still comes back as
/// text — the caller owns the parse (see [`parse_boolean`]), exactly as for
/// an environment variable.
fn resolve_boolean_value(
    key: &str,
    env_var: &str,
    profile: Option<&str>,
    project: Option<&Table>,
    global: Option<&Table>,
) -> Result<Option<String>> {
    resolve_value_with(boolean_value, key, env_var, profile, project, global)
}

/// [`resolve_value`] for a list setting: the file layers accept a TOML array
/// of strings as well as a quoted string. The value comes back as one text
/// with the entries separated by commas — the form an environment variable
/// takes — and the caller owns the split and the parse.
fn resolve_list_value(
    key: &str,
    env_var: &str,
    profile: Option<&str>,
    project: Option<&Table>,
    global: Option<&Table>,
) -> Result<Option<String>> {
    resolve_value_with(list_value, key, env_var, profile, project, global)
}

fn resolve_value_with(
    read: ValueReader,
    key: &str,
    env_var: &str,
    profile: Option<&str>,
    project: Option<&Table>,
    global: Option<&Table>,
) -> Result<Option<String>> {
    // The environment variable always has highest precedence.
    if let Some(value) = env::var(env_var)? {
        return Ok(Some(value));
    }

    // A named profile is consulted before defaults, project before global.
    if let Some(profile) = profile {
        for table in [project, global].into_iter().flatten() {
            if let Some(value) = profile_value(table, profile, key, read)? {
                return Ok(Some(value));
            }
        }
    }

    // Finally fall back to top-level defaults, project before global.
    for table in [project, global].into_iter().flatten() {
        if let Some(value) = read(table, key, key)? {
            return Ok(Some(value));
        }
    }

    Ok(None)
}

/// A `[profile.<name>]` value: `profile.<profile>.<key>`. `Ok(None)` when the
/// `profile` section, the named profile, or the key is absent; an error when
/// `profile`/`profile.<name>` is present but not a table, or the value fails
/// `read`'s type check.
fn profile_value(
    table: &Table,
    profile: &str,
    key: &str,
    read: ValueReader,
) -> Result<Option<String>> {
    let Some(profiles) = table.get(PROFILE_KEY) else {
        return Ok(None);
    };
    let profiles = profiles
        .as_table()
        .ok_or_else(|| eyre!("`{PROFILE_KEY}` must be a table of profiles"))?;
    let Some(selected) = profiles.get(profile) else {
        return Ok(None);
    };
    let selected = selected
        .as_table()
        .ok_or_else(|| eyre!("profile `{profile}` must be a table"))?;
    read(selected, key, &format!("{PROFILE_KEY}.{profile}.{key}"))
}

/// Read `key` from `table` as a string, naming the offending value `display` in
/// the error. `Ok(None)` when the key is absent; an error when it is present but
/// holds a non-string TOML value.
fn string_value(table: &Table, key: &str, display: &str) -> Result<Option<String>> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => match value.as_str() {
            Some(value) => Ok(Some(value.to_string())),
            None => Err(eyre!(
                "setting `{display}` must be a string, but found {}",
                value.type_str()
            )),
        },
    }
}

/// Read `key` from `table` as a number-like setting (a bare TOML integer or a
/// quoted string), naming the offending value `display` in the error. The
/// value comes back in text form; the caller parses it (see
/// [`resolve_integer_value`]).
fn integer_value(table: &Table, key: &str, display: &str) -> Result<Option<String>> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::Integer(number)) => Ok(Some(number.to_string())),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(value) => Err(eyre!(
            "setting `{display}` must be an integer or a string, but found {}",
            value.type_str()
        )),
    }
}

/// Read `key` from `table` as a boolean-like setting (a bare TOML boolean or
/// a quoted string), naming the offending value `display` in the error. The
/// value comes back in text form; the caller parses it (see
/// [`resolve_boolean_value`]).
fn boolean_value(table: &Table, key: &str, display: &str) -> Result<Option<String>> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::Boolean(flag)) => Ok(Some(flag.to_string())),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(value) => Err(eyre!(
            "setting `{display}` must be a boolean or a string, but found {}",
            value.type_str()
        )),
    }
}

/// Read `key` from `table` as a list-like setting (a TOML array of strings or
/// a quoted string), naming the offending value `display` in the error. The
/// value comes back as comma-separated text; the caller splits and parses it
/// (see [`resolve_list_value`]).
fn list_value(table: &Table, key: &str, display: &str) -> Result<Option<String>> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str().ok_or_else(|| {
                    eyre!(
                        "setting `{display}` must be an array of strings, but found {}",
                        item.type_str()
                    )
                })
            })
            .collect::<Result<Vec<&str>>>()
            .map(|entries| Some(entries.join(","))),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(value) => Err(eyre!(
            "setting `{display}` must be an array of strings or a string, but found {}",
            value.type_str()
        )),
    }
}

/// Parse a comma-separated list of registry prefixes, naming `setting` in the
/// error. An empty entry between two commas is skipped.
fn parse_registry_prefixes(setting: &str, value: &str) -> Result<Vec<RegistryPrefix>> {
    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            entry
                .parse::<RegistryPrefix>()
                .map_err(|err| user_error(format!("invalid {setting} setting: {err}")))
        })
        .collect()
}

/// Parse a boolean setting — exactly `true` or `false` — naming `setting` in
/// the error.
fn parse_boolean(setting: &str, value: &str) -> Result<bool> {
    value.parse::<bool>().map_err(|_| {
        user_error(format!(
            "invalid {setting} setting (expected \"true\" or \"false\"): got `{value}`"
        ))
    })
}

/// Parse a byte-size setting — a size such as "10 MB" or "1.5GiB", or a bare
/// byte count — into bytes, naming `setting` in the error. SI units are
/// decimal (MB = 10^6 bytes); IEC units are binary (MiB = 2^20 bytes).
fn parse_byte_size(setting: &str, value: &str) -> Result<u64> {
    value
        .parse::<bytesize::ByteSize>()
        .map(|size| size.as_u64())
        .map_err(|err| {
            user_error(format!(
                "invalid {setting} setting (expected a size such as \"10 MB\"): {err}"
            ))
        })
}

pub(crate) fn read_to_string_if_file_exists(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(eyre!("File at {:?} could not be read: {err:#}", path)),
    }
}

pub(crate) fn back_up_unparsable_file(path: &Path) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| eyre!("Cannot back up file with no name: {:?}", path))?;
    let backup = path.with_file_name(format!("{file_name}.bak"));
    fs::rename(path, &backup)
        .wrap_err_with(|| format!("Failed to back up {:?} to {:?}", path, backup))?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An env var name guaranteed not to be set, so `resolve_value` exercises
    /// the file layers deterministically under parallel execution (the
    /// environment-wins case is covered end-to-end by `specs/settings.txt`).
    const UNSET_ENV: &str = "SNOUTY_DEFINITELY_NOT_SET_ENV_VAR_98f3a";

    fn settings_file(contents: &str) -> Table {
        contents.parse().expect("test TOML should parse")
    }

    /// Resolve `tenant` against an env var guaranteed not to be set, so the file
    /// layers decide the outcome. Each layer in the precedence tests uses a
    /// distinct value, so the resolved value alone proves which layer won.
    fn resolve_tenant(
        profile: Option<&str>,
        project: Option<&Table>,
        global: Option<&Table>,
    ) -> Option<String> {
        resolve_value("tenant", UNSET_ENV, profile, project, global).unwrap()
    }

    // ---- parse_settings ------------------------------------------------

    #[test]
    fn invalid_toml_is_reported_with_path() {
        let err =
            parse_settings("this is = = not toml", Path::new("/some/.snouty.toml")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not valid TOML"), "unexpected error: {msg}");
        assert!(msg.contains(".snouty.toml"), "unexpected error: {msg}");
    }

    // ---- load_settings_file (filesystem) -------------------------------

    #[test]
    fn missing_default_file_is_ok() {
        let result = load_settings_file(Path::new("/no/such/.snouty.toml"), false).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn missing_required_file_is_an_error() {
        let err = load_settings_file(Path::new("/no/such/.snouty.toml"), true).unwrap_err();
        assert!(err.to_string().contains("was not found"));
    }

    // ---- profile_value / string_value / integer_value --------------------

    #[test]
    fn profile_value_reads_nested_table() {
        let table: Table = "[profile.staging]\ntenant = \"staging-tenant\"\n"
            .parse()
            .unwrap();
        assert_eq!(
            profile_value(&table, "staging", "tenant", string_value)
                .unwrap()
                .as_deref(),
            Some("staging-tenant")
        );
        // missing profile and missing key both resolve to None
        assert_eq!(
            profile_value(&table, "prod", "tenant", string_value).unwrap(),
            None
        );
        assert_eq!(
            profile_value(&table, "staging", "repository", string_value).unwrap(),
            None
        );
    }

    #[test]
    fn string_value_reads_top_level_key() {
        let table: Table = "tenant = \"acme\"\n".parse().unwrap();
        assert_eq!(
            string_value(&table, "tenant", "tenant").unwrap().as_deref(),
            Some("acme")
        );
        assert_eq!(string_value(&table, "missing", "missing").unwrap(), None);
    }

    // ---- strict TOML typing --------------------------------------------

    #[test]
    fn non_string_default_value_is_an_error() {
        let table: Table = "tenant = 123\n".parse().unwrap();
        let err = string_value(&table, "tenant", "tenant").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("tenant"), "unexpected error: {msg}");
        assert!(msg.contains("must be a string"), "unexpected error: {msg}");
    }

    #[test]
    fn non_string_profile_value_is_an_error() {
        let table: Table = "[profile.p]\ntenant = true\n".parse().unwrap();
        let err = profile_value(&table, "p", "tenant", string_value).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("profile.p.tenant"), "unexpected error: {msg}");
        assert!(msg.contains("must be a string"), "unexpected error: {msg}");
    }

    #[test]
    fn integer_value_accepts_a_bare_toml_integer() {
        let table: Table = "api_cache_max_file_size = 1234\n".parse().unwrap();
        assert_eq!(
            integer_value(&table, "api_cache_max_file_size", "api_cache_max_file_size")
                .unwrap()
                .as_deref(),
            Some("1234")
        );
    }

    #[test]
    fn integer_value_accepts_a_quoted_string() {
        let table: Table = "api_cache_max_file_size = \"1234\"\n".parse().unwrap();
        assert_eq!(
            integer_value(&table, "api_cache_max_file_size", "api_cache_max_file_size")
                .unwrap()
                .as_deref(),
            Some("1234")
        );
    }

    #[test]
    fn non_numeric_integer_value_is_an_error() {
        let table: Table = "api_cache_max_file_size = true\n".parse().unwrap();
        let err = integer_value(&table, "api_cache_max_file_size", "api_cache_max_file_size")
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("must be an integer"),
            "unexpected error: {msg}"
        );
        assert!(msg.contains("boolean"), "unexpected error: {msg}");
    }

    #[test]
    fn boolean_value_accepts_a_bare_toml_boolean_or_a_quoted_string() {
        let table: Table = "api_cache_respect_headers = false\n".parse().unwrap();
        assert_eq!(
            boolean_value(&table, "api_cache_respect_headers", "x")
                .unwrap()
                .as_deref(),
            Some("false")
        );
        let table: Table = "api_cache_respect_headers = \"false\"\n".parse().unwrap();
        assert_eq!(
            boolean_value(&table, "api_cache_respect_headers", "x")
                .unwrap()
                .as_deref(),
            Some("false")
        );
    }

    #[test]
    fn non_boolean_value_is_an_error() {
        let table: Table = "api_cache_respect_headers = 1\n".parse().unwrap();
        let err = boolean_value(
            &table,
            "api_cache_respect_headers",
            "api_cache_respect_headers",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("must be a boolean"), "unexpected error: {msg}");
        assert!(msg.contains("integer"), "unexpected error: {msg}");
    }

    #[test]
    fn list_value_accepts_an_array_of_strings_or_a_quoted_string() {
        let table: Table = "private_registries = [\"ghcr.io/acme\", \"quay.io\"]\n"
            .parse()
            .unwrap();
        assert_eq!(
            list_value(&table, "private_registries", "x")
                .unwrap()
                .as_deref(),
            Some("ghcr.io/acme,quay.io")
        );
        let table: Table = "private_registries = \"ghcr.io/acme, quay.io\"\n"
            .parse()
            .unwrap();
        assert_eq!(
            list_value(&table, "private_registries", "x")
                .unwrap()
                .as_deref(),
            Some("ghcr.io/acme, quay.io")
        );
        assert_eq!(list_value(&table, "missing", "x").unwrap(), None);
    }

    #[test]
    fn non_string_list_value_is_an_error() {
        for contents in [
            "private_registries = [\"ghcr.io\", 1]\n",
            "private_registries = true\n",
        ] {
            let table: Table = contents.parse().unwrap();
            let msg = list_value(&table, "private_registries", "private_registries")
                .unwrap_err()
                .to_string();
            assert!(
                msg.contains("private_registries"),
                "unexpected error: {msg}"
            );
            assert!(msg.contains("array of strings"), "unexpected error: {msg}");
        }
    }

    #[test]
    fn registry_prefixes_split_on_commas_and_skip_empty_entries() {
        let prefixes =
            parse_registry_prefixes("private_registries", " ghcr.io/acme ,, quay.io,").unwrap();
        let spelled: Vec<String> = prefixes.iter().map(ToString::to_string).collect();
        assert_eq!(spelled, ["ghcr.io/acme", "quay.io"]);
        assert!(
            parse_registry_prefixes("private_registries", "")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn parse_boolean_accepts_only_true_and_false() {
        assert!(parse_boolean("api_cache_respect_headers", "true").unwrap());
        assert!(!parse_boolean("api_cache_respect_headers", "false").unwrap());
        for bad in ["1", "yes", "True", ""] {
            let err = parse_boolean("api_cache_respect_headers", bad).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("api_cache_respect_headers"),
                "unexpected error for {bad:?}: {msg}"
            );
        }
    }

    #[test]
    fn malformed_profile_section_is_an_error() {
        // `profile` present but not a table of profiles.
        let table: Table = "profile = \"oops\"\n".parse().unwrap();
        let err = profile_value(&table, "p", "tenant", string_value).unwrap_err();
        assert!(err.to_string().contains("table of profiles"));
    }

    #[test]
    fn non_table_profile_is_an_error() {
        // The named profile exists but isn't a table.
        let table: Table = "[profile]\np = \"oops\"\n".parse().unwrap();
        let err = profile_value(&table, "p", "tenant", string_value).unwrap_err();
        assert!(err.to_string().contains("profile `p` must be a table"));
    }

    // ---- xdg_base (path resolution) ------------------------------------

    #[test]
    fn xdg_base_prefers_the_xdg_dir() {
        let dir = xdg_base(
            Some("/xdg".to_string()),
            Some("/home/u".to_string()),
            ".config",
        );
        assert_eq!(dir, Some(PathBuf::from("/xdg/snouty")));
    }

    #[test]
    fn xdg_base_falls_back_to_home_subdir() {
        let dir = xdg_base(None, Some("/home/u".to_string()), ".config");
        assert_eq!(dir, Some(PathBuf::from("/home/u/.config/snouty")));
    }

    #[test]
    fn xdg_base_is_none_without_xdg_or_home() {
        assert_eq!(xdg_base(None, None, ".config"), None);
    }

    // ---- resolve_value precedence --------------------------------------

    #[test]
    fn project_profile_beats_global_profile_and_all_defaults() {
        let project =
            settings_file("tenant = \"proj-default\"\n[profile.p]\ntenant = \"proj-profile\"\n");
        let global = settings_file(
            "tenant = \"global-default\"\n[profile.p]\ntenant = \"global-profile\"\n",
        );
        assert_eq!(
            resolve_tenant(Some("p"), Some(&project), Some(&global)).as_deref(),
            Some("proj-profile")
        );
    }

    #[test]
    fn global_profile_beats_project_default() {
        let project = settings_file("tenant = \"proj-default\"\n");
        let global = settings_file("[profile.p]\ntenant = \"global-profile\"\n");
        assert_eq!(
            resolve_tenant(Some("p"), Some(&project), Some(&global)).as_deref(),
            Some("global-profile")
        );
    }

    #[test]
    fn project_default_beats_global_default() {
        let project = settings_file("tenant = \"proj-default\"\n");
        let global = settings_file("tenant = \"global-default\"\n");
        assert_eq!(
            resolve_tenant(None, Some(&project), Some(&global)).as_deref(),
            Some("proj-default")
        );
    }

    #[test]
    fn global_default_is_the_last_resort() {
        let global = settings_file("tenant = \"global-default\"\n");
        assert_eq!(
            resolve_tenant(None, None, Some(&global)).as_deref(),
            Some("global-default")
        );
    }

    #[test]
    fn nothing_set_resolves_to_none() {
        assert!(resolve_tenant(None, None, None).is_none());
    }

    #[test]
    fn an_unselected_profile_falls_back_to_defaults() {
        // No `--profile`, so a `[profile.p]` value is ignored in favor of the
        // top-level default.
        let project =
            settings_file("tenant = \"proj-default\"\n[profile.p]\ntenant = \"proj-profile\"\n");
        assert_eq!(
            resolve_tenant(None, Some(&project), None).as_deref(),
            Some("proj-default")
        );
    }

    // ---- accessors -----------------------------------------------------

    #[test]
    fn require_missing_setting_points_at_doctor() {
        let err = require(None, "tenant").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("tenant"), "unexpected error: {msg}");
        assert!(msg.contains("snouty doctor"), "unexpected error: {msg}");
    }

    #[test]
    fn require_passes_a_present_setting_through() {
        assert_eq!(require(Some("acme"), "tenant").unwrap(), "acme");
    }

    #[test]
    fn base_url_falls_back_to_tenant_host() {
        let settings = Settings::builder().tenant("acme").build();
        assert_eq!(settings.base_url(), Some("https://acme.antithesis.com"));
    }

    #[test]
    fn explicit_base_url_overrides_tenant_host() {
        let settings = Settings::builder()
            .tenant("acme")
            .base_url("https://example.test")
            .build();
        assert_eq!(settings.base_url(), Some("https://example.test"));
    }

    #[test]
    fn base_url_without_tenant_is_none() {
        let settings = Settings::default();
        assert_eq!(settings.base_url(), None);
    }

    #[test]
    fn container_engine_absent_resolves_to_none() {
        let settings = Settings::default();
        assert_eq!(settings.container_engine(), None);
    }

    #[test]
    fn container_engine_resolves_when_set() {
        let settings = Settings::builder().container_engine("podman").build();
        assert_eq!(settings.container_engine(), Some("podman"));
    }

    #[test]
    fn https_proxy_absent_resolves_to_none() {
        let settings = Settings::default();
        assert_eq!(settings.https_proxy(), None);
    }

    #[test]
    fn https_proxy_resolves_when_set() {
        let settings = Settings::builder()
            .https_proxy("http://proxy.corp:8080")
            .build();
        assert_eq!(settings.https_proxy(), Some("http://proxy.corp:8080"));
    }

    #[test]
    fn https_proxy_resolves_from_a_settings_file() {
        let project = settings_file("https_proxy = \"http://proxy.corp:8080\"\n");
        assert_eq!(
            resolve_value("https_proxy", UNSET_ENV, None, Some(&project), None).unwrap(),
            Some("http://proxy.corp:8080".to_string())
        );
    }

    #[test]
    fn update_channel_absent_resolves_to_stable() {
        let settings = Settings::default();
        assert_eq!(settings.update_channel(), UpdateChannel::Stable);
    }

    #[test]
    fn update_channel_resolves_when_set() {
        let settings = Settings::builder()
            .update_channel(UpdateChannel::Unstable)
            .build();
        assert_eq!(settings.update_channel(), UpdateChannel::Unstable);
    }

    #[test]
    fn update_channel_resolves_from_a_settings_file() {
        let project = settings_file("update_channel = \"unstable\"\n");
        assert_eq!(
            resolve_value("update_channel", UNSET_ENV, None, Some(&project), None).unwrap(),
            Some("unstable".to_string())
        );
    }

    #[test]
    fn api_cache_max_file_size_resolves_from_a_settings_file() {
        // The natural TOML form for a byte count is a bare integer.
        let project = settings_file("api_cache_max_file_size = 1234\n");
        assert_eq!(
            resolve_integer_value(
                "api_cache_max_file_size",
                UNSET_ENV,
                None,
                Some(&project),
                None
            )
            .unwrap(),
            Some("1234".to_string())
        );
    }

    // ---- parse_byte_size -------------------------------------------------

    #[test]
    fn byte_sizes_parse_with_si_and_iec_units() {
        assert_eq!(
            parse_byte_size("api_cache_max_file_size", "10 MB").unwrap(),
            10_000_000
        );
        assert_eq!(
            parse_byte_size("api_cache_max_file_size", "10 MiB").unwrap(),
            10 * 1024 * 1024
        );
        // Units are case-insensitive and the space is optional.
        assert_eq!(
            parse_byte_size("api_cache_max_file_size", "1gb").unwrap(),
            1_000_000_000
        );
        assert_eq!(
            parse_byte_size("api_cache_max_file_size", "1.5 KB").unwrap(),
            1_500
        );
    }

    #[test]
    fn a_bare_number_parses_as_bytes() {
        assert_eq!(
            parse_byte_size("api_cache_max_file_size", "10485760").unwrap(),
            10_485_760
        );
    }

    #[test]
    fn a_malformed_byte_size_names_the_setting() {
        for bad in ["ten megabytes", "-1", ""] {
            let err = parse_byte_size("api_cache_max_file_size", bad).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("api_cache_max_file_size") && msg.contains("10 MB"),
                "unexpected error for {bad:?}: {msg}"
            );
        }
    }

    // ---- validate_tenant_host -----------------------------------------

    #[test]
    fn valid_tenants_pass_host_validation() {
        for tenant in ["orbitinghail", "acme", "my-tenant", "t123", "foo.bar"] {
            assert!(
                validate_tenant_host(tenant).is_ok(),
                "expected `{tenant}` to be a valid tenant host"
            );
        }
    }

    #[test]
    fn url_significant_tenants_are_rejected() {
        // Each of these would otherwise redirect requests (with the API key) to
        // an unintended host or mangle the URL.
        for tenant in [
            "evil.com#",
            "evil.com/x",
            "a b",
            "acme#",
            "foo/../bar",
            "host:8080",
            "tenant?x=1",
            "user@host",
            "",
            "-leadinghyphen",
            "trailinghyphen-",
        ] {
            assert!(
                validate_tenant_host(tenant).is_err(),
                "expected `{tenant}` to be rejected as a tenant host"
            );
        }
    }

    #[test]
    fn explicit_base_url_bypasses_tenant_host_validation() {
        // An explicit base_url bypasses tenant-host validation (the tenant isn't
        // interpolated into the host), so an otherwise-invalid tenant still
        // constructs. The derive-path validation itself is covered by
        // validate_tenant_host's unit tests and specs/settings_tenant.txt.
        let s = Settings::builder()
            .tenant("evil.com#")
            .base_url("https://ok.example")
            .build();
        assert_eq!(s.base_url(), Some("https://ok.example"));
    }
}
