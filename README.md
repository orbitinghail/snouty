# snouty

A CLI for the [Antithesis](https://antithesis.com) API. See the [webhook documentation](https://antithesis.com/docs/webhook/) for details on available endpoints and parameters.

## Usage

Snouty provides the following subcommands. Invoke `snouty <command> --help` to find out more.

- `snouty login`: sign in and store your tenant, repository, and credentials.
- `snouty launch`: push images and kick off an Antithesis run.
- `snouty runs`: list and inspect Antithesis test runs and their results.
  - `snouty runs list`: list runs, with status/launcher/date filters.
  - `snouty runs show <run_id>`: show details for a single run.
  - `snouty runs wait <run_id>`: poll a run until it reaches a terminal state.
  - `snouty runs properties <run_id>`: list property (assertion) results.
  - `snouty runs build-logs <run_id>`: stream a run's build logs.
  - `snouty runs logs <run_id> <hash> [vtime]`: stream a run's logs along one branch.
  - `snouty runs events <run_id> -m <needle>`: search events in a run.
  - `snouty runs search <run_id> <query>`: run an event-set DSL query against a run's events.
  - `snouty runs exec <run_id> <hash> <vtime> [script]`: run a bash script in a run's live session at a given moment.
- `snouty debug`: start a debug session.
- `snouty validate`: locally run and validate your docker-compose.yaml setup.
- `snouty doctor`: check your environment is configured correctly.
- `snouty docs`: search the Antithesis documentation locally (auto-refreshes the local copy over the network; pass `--offline` to skip).
- `snouty completions <shell>`: generate shell completion scripts.
- `snouty version`: print version and build information.
- `snouty update`: install the latest version. Set `update_channel = "unstable"` (or `SNOUTY_UPDATE_CHANNEL=unstable`) to also consider pre-releases; override the setting for one run with `--channel stable|unstable`.

Add `--json` for machine-readable output. See [COOKBOOK.md](COOKBOOK.md) for worked recipes.

## Requirements

Commands that work with `docker-compose.yaml` files (e.g. `launch`, `validate`) require:

- **Docker Compose v2** — snouty drives either the standalone `docker-compose` binary or the `docker compose` CLI plugin (bundled with Docker Desktop/Engine), whichever it finds on your `PATH`. podman-compose is no longer supported.
  - Linux: [Install Docker Compose](https://docs.docker.com/compose/install/) or check your package manager.
  - macOS: [Install Docker Desktop](https://docs.docker.com/desktop/setup/install/mac-install/) (includes Compose v2) or `brew install docker-compose`.
- **A container engine** — Docker or Podman, used to build and push images.

If both engines are installed, Podman is preferred. Override the engine with `SNOUTY_CONTAINER_ENGINE=docker` or `container_engine` in a settings file (see below); an explicit `DOCKER_HOST` in your environment is always respected.

### VM-backed and remote container engines

`snouty validate` bind-mounts a temp directory from this machine into each container. It watches this directory for the setup-complete event. Some container engines run inside a VM or on another machine. If the engine does not share this machine's temp directory, the engine creates the bind source on its own side and reports no error. The directory on this machine stays empty. Validate then fails with `timed out waiting for setup-complete event`, even though the system under test emits the event.

There are two fixes:

- Share this machine's temp directory with the engine. See the mount documentation for your engine.
- Set `SNOUTY_TEMP_DIR` to a directory under a path the engine already shares with write access:

  ```sh
  SNOUTY_TEMP_DIR=/path/shared/with/the/vm/snouty snouty validate ./config
  ```

  `SNOUTY_TEMP_DIR` must point to an empty or non-existent directory. This prevents validate from reading events that a previous run left behind. Snouty does not remove the directory after the run, so remove it before the next run.

## Install snouty

### Install prebuilt binaries via shell script

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/antithesishq/snouty/releases/latest/download/snouty-installer.sh | sh
```

This also allows you to update via `snouty update` later on.

### Install prebuilt binaries via cargo binstall

```sh
cargo binstall snouty
```

### Install snouty from source

```sh
cargo install snouty
```

### Download prebuilt binaries

| File                                                                                                                                                 | Platform            | Checksum                                                                                                                    |
| ---------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------- | --------------------------------------------------------------------------------------------------------------------------- |
| [snouty-aarch64-apple-darwin.tar.xz](https://github.com/antithesishq/snouty/releases/latest/download/snouty-aarch64-apple-darwin.tar.xz)             | Apple Silicon macOS | [checksum](https://github.com/antithesishq/snouty/releases/latest/download/snouty-aarch64-apple-darwin.tar.xz.sha256)       |
| [snouty-aarch64-unknown-linux-musl.tar.xz](https://github.com/antithesishq/snouty/releases/latest/download/snouty-aarch64-unknown-linux-musl.tar.xz) | ARM64 Linux         | [checksum](https://github.com/antithesishq/snouty/releases/latest/download/snouty-aarch64-unknown-linux-musl.tar.xz.sha256) |
| [snouty-x86_64-unknown-linux-musl.tar.xz](https://github.com/antithesishq/snouty/releases/latest/download/snouty-x86_64-unknown-linux-musl.tar.xz)   | x64 Linux           | [checksum](https://github.com/antithesishq/snouty/releases/latest/download/snouty-x86_64-unknown-linux-musl.tar.xz.sha256)  |

### Uninstalling

```
cargo uninstall snouty || rm -f "$(which snouty)" "$(which snouty-update)"
```

## Configuration

Using the API requires at least a **tenant** and a credential. `snouty launch` also needs a **repository** — the container registry it pushes the config image to. Docs commands need no configuration at all.

The quickest way is `snouty login`, which asks for each value and stores the result:

```sh
snouty login
```

Settings can also come from environment variables or a TOML settings file. An environment variable always takes precedence.

```sh
export ANTITHESIS_TENANT="your-tenant"
export ANTITHESIS_REPOSITORY="us-central1-docker.pkg.dev/your-project/your-repo"
export ANTITHESIS_API_KEY="your-api-key"
```

Run `snouty doctor` at any point to see what snouty resolves, and where each value comes from.

### Settings files

Settings can instead live in a TOML file. Snouty reads two, the first taking precedence:

1. A **project** settings file — `./.snouty.toml` by default. Point elsewhere with the global `--settings <path>` flag or the `SNOUTY_SETTINGS_PATH` environment variable.
2. A **global** settings file — `settings.toml` under `$XDG_CONFIG_HOME/snouty/` (falling back to `$HOME/.config/snouty/`).

```toml
# .snouty.toml
tenant = "your-tenant"
repository = "us-central1-docker.pkg.dev/your-project/your-repo"
```

A matching environment variable always overrides the file. The supported keys and their environment-variable equivalents are:

| Settings key                | Environment variable              | Purpose                                                                          |
| --------------------------- | --------------------------------- | -------------------------------------------------------------------------------- |
| `tenant`                    | `ANTITHESIS_TENANT`               | Your Antithesis tenant. Becomes the API host, `https://<tenant>.antithesis.com`.  |
| `repository`                | `ANTITHESIS_REPOSITORY`           | Container registry that `snouty launch` pushes the config image to.              |
| `base_url`                  | `ANTITHESIS_BASE_URL`             | API URL, replacing the one derived from the tenant. See the proxy section below. |
| `container_engine`          | `SNOUTY_CONTAINER_ENGINE`         | Force `docker` or `podman` instead of auto-detecting.                            |
| `update_channel`            | `SNOUTY_UPDATE_CHANNEL`           | `stable` (default) or `unstable`, for `snouty update`.                           |
| `api_cache_max_file_size`   | `SNOUTY_API_CACHE_MAX_FILE_SIZE`  | Largest response the API cache stores, as `10 MB` or a byte count.               |
| `api_cache_respect_headers` | `SNOUTY_API_CACHE_RESPECT_HEADERS`| Set `false` to ignore server caching directives.                                 |
| `private_registries`        | `SNOUTY_PRIVATE_REGISTRIES`       | Registries a test run cannot pull from. See below.                               |

`private_registries` lists registries, or repository paths below a registry, that need this machine's credentials. `snouty launch --config` pulls a compose service image below a listed prefix when the local image store lacks it, and always copies the image into your repository, so the test run never pulls from the private registry. An image at a private registry that is not listed keeps its own address in the pin, and the test run fails to pull it. `snouty validate` does not pull. In a settings file the value is a TOML array; in the environment variable, separate the entries with commas.

```toml
private_registries = ["ghcr.io/your-org", "123456789012.dkr.ecr.us-east-1.amazonaws.com"]
```

A credential is never read from a settings file. `snouty login` keeps credentials apart from settings — see [Authentication](#authentication).

### Profiles

A settings file can define named profiles for switching between environments. Select one with the global `--profile <name>` flag or the `ANTITHESIS_PROFILE` environment variable (the flag wins):

```toml
# .snouty.toml
tenant = "default-tenant"
repository = "registry.example.com/default"

[profile.staging]
tenant = "staging-tenant"
repository = "registry.example.com/staging"
```

```sh
snouty --profile staging runs list
```

For any one setting, snouty uses the first value it finds, highest precedence first:

1. environment variable
2. the selected profile in the project settings file
3. the selected profile in the global settings file
4. the top-level default in the project settings file
5. the top-level default in the global settings file

### Unstable features

A few subcommands depend on an Antithesis API that is still changing shape, so snouty keeps them behind a gate. Name the features you want in `SNOUTY_UNSTABLE_FEATURES`, as a comma-separated list. A gated command is hidden from `--help` until its feature is on, and fails as an unrecognized subcommand when it is off.

```sh
export SNOUTY_UNSTABLE_FEATURES="runs-search,runs-exec"
```

| Feature       | Enables                                                                            |
| ------------- | ---------------------------------------------------------------------------------- |
| `runs-search` | `snouty runs search`, and routes `snouty runs events` through the events-search API. Needs tenant release 58.11 or newer. |
| `runs-exec`   | `snouty runs exec`. The execute-command API is unavailable on most tenants.        |

Anything behind this gate can change its behavior, its flags, or its id, or go away, in any release. `snouty doctor` lists the features that are on, and reports when your tenant is too old to serve one.

## Authentication

Snouty accepts four kinds of credentials: OAuth tokens from a browser sign-in, an API key, a GitHub Actions OIDC token, and a deprecated username and password. On a workstation, run `snouty login` and let snouty store a credential for you. In CI and in scripts, use environment variables or GitHub Actions OIDC tokens.

Snouty looks for a credential in this order, and uses the first one it finds:

1. `ANTITHESIS_API_KEY` in the environment.
2. `ANTITHESIS_USERNAME` and `ANTITHESIS_PASSWORD` in the environment (deprecated).
3. The credential stored for the selected `--profile`: the system keychain first, the credentials file second.
4. The credential stored for the default profile: the system keychain first, the credentials file second.
5. A GitHub Actions OIDC token, when the workflow exposes one.

An environment variable always wins over a stored credential. Unset `ANTITHESIS_API_KEY` when you want snouty to use what `snouty login` stored.

Run `snouty doctor` to see which credential snouty resolves, and where it comes from. doctor also warns when more than one source holds a credential, and names the action that hands the run to the next source.

### Browser sign-in with `snouty login`

`snouty login` asks for your tenant and your repository, then asks how you want to authenticate. It writes the tenant and the repository to the global settings file, and it stores the credential separately.

```sh
snouty login
snouty login --tenant "your-tenant" --repository "us-central1-docker.pkg.dev/your-project/your-repo"
snouty login --profile staging
```

Pick **Single sign-on (OAuth)** to sign in through your identity provider. Snouty binds a loopback server, opens a browser, and waits up to five minutes for the redirect to come back. It prints the sign-in URL as well, so you can open the URL by hand when it cannot open a browser for you. The exchange uses PKCE, so no shared secret is needed.

Two limits are worth knowing:

- The menu offers single sign-on only when your tenant enables CLI OAuth. Use an API key when the option does not appear.
- The browser must run on the same machine as snouty, because the redirect goes to `http://localhost:<port>/callback`. On a headless machine or a remote VM, either forward that port to your workstation, or use another authentication method.

### Where stored credentials live

On macOS, snouty stores the credential in the keychain, under the service name `snouty`. The entry is named `_default_`, or `profile_<name>` for a named profile. Set `SNOUTY_DISABLE_KEYCHAIN_CREDENTIAL_STORAGE=1` to use the credentials file instead.

On every other platform, snouty writes `credentials.toml` next to the global settings file, in `$XDG_CONFIG_HOME/snouty/` (falling back to `$HOME/.config/snouty/`).

### API key

An API key works with every command, and needs no browser. Ask Antithesis support for one if you do not have one.

```sh
export ANTITHESIS_API_KEY="your-api-key"
```

`snouty login` also stores an API key for you, which keeps the key out of your shell history and out of your environment.

### GitHub Actions OIDC

In a GitHub Actions workflow, snouty authenticates with the workflow's own OIDC token, so you store no secret. Give the job permission to mint the token:

```yaml
permissions:
  id-token: write
  contents: read
```

> [!IMPORTANT]
> Your Antithesis tenant must trust GitHub Actions OIDC tokens from your repository before this works. Ask Antithesis support to configure it.

### Credential injection by an HTTPS proxy

Some VM and development platforms hold your credentials for you, and inject them into outbound requests. The platform gives you a hostname that stands in for `https://<tenant>.antithesis.com`. That hostname terminates TLS, replaces the `Authorization` header with the real credential, and forwards the request. Your machine never holds the secret. Replit and exe.dev work this way; check your platform's documentation for the hostname it gives you.

Point snouty at that hostname, and give it a placeholder API key:

```sh
export ANTITHESIS_BASE_URL="https://antithesis.int.example.com"
export ANTITHESIS_API_KEY="replaced-by-proxy"
```

Three things to know:

- `ANTITHESIS_BASE_URL` replaces the URL that snouty derives from the tenant, so every API request goes to the proxy instead. Snouty takes the value as given, and only trims a trailing `/` or `/api/v1`.
- Snouty still needs *a* credential, or it stops before it sends a request. Snouty never inspects the value, so any non-empty string works. Use an obvious placeholder such as `replaced-by-proxy`, so a reader sees that the real secret is elsewhere. An empty value counts as unset, and does not work.
- Keep `ANTITHESIS_TENANT` set. Only the derived base URL uses it, and `ANTITHESIS_BASE_URL` replaces that, but `snouty doctor` reports a missing tenant as a failure.

`snouty doctor` confirms the whole setup: it reports the API key as provided, and it contacts the API through the proxy to report the API and tenant versions.

`ANTITHESIS_EXTRA_HEADERS` adds headers to every API request, as one `Name: value` pair per line. Use it when the platform expects a header of its own.

```sh
export ANTITHESIS_EXTRA_HEADERS="X-Proxy-Token: abc123"
```

### Username and password (deprecated)

Username and password authentication is deprecated. It works with `snouty launch` and `snouty debug` only, and both print a warning that points to `snouty login`. Every other command that talks to the API refuses it. Use `snouty login` or an API key instead.

## Shell Completions

Snouty supports tab completions for bash, zsh, fish, and elvish.

### Bash

```sh
# Add to ~/.bashrc
eval "$(snouty completions bash)"
```

### Zsh

```sh
# Add to ~/.zshrc
eval "$(snouty completions zsh)"
```

### Fish

```sh
snouty completions fish > ~/.config/fish/completions/snouty.fish
```

### Elvish

```sh
snouty completions elvish > ~/.config/elvish/lib/snouty.elv
```

# Credits

This project was originally developed by [orbitinghail](https://orbitinghail.dev) for use by [Graft](https://github.com/orbitinghail/graft). It was donated to Antithesis for the benefit of everyone on Feb 27, 2026.
