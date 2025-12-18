# snouty

A unofficial CLI for the [Antithesis](https://antithesis.com) API. See the [webhook documentation](https://antithesis.com/docs/webhook/) for details on available endpoints and parameters.

> [!NOTE]
> This CLI is unofficial and unsupported by Antithesis. It's released open-source for the benefit of Antithesis users. If you encounter problems with the CLI please file issues here. If you encounter issues with the Antithesis API then please reach out to support.

## Install snouty

### Install prebuilt binaries via shell script

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/orbitinghail/snouty/releases/latest/download/snouty-installer.sh | sh
```

### Install prebuilt binaries via powershell script

```sh
powershell -ExecutionPolicy Bypass -c "irm https://github.com/orbitinghail/snouty/releases/latest/download/snouty-installer.ps1 | iex"
```

### Install prebuilt binaries via cargo binstall

```sh
cargo binstall snouty
```

### Install snouty from source

```sh
cargo install snouty
```

### Download prebuilt binaries

| File                                                                                                                                               | Platform            | Checksum                                                                                                                   |
| -------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------- | -------------------------------------------------------------------------------------------------------------------------- |
| [snouty-aarch64-apple-darwin.tar.xz](https://github.com/orbitinghail/snouty/releases/latest/download/snouty-aarch64-apple-darwin.tar.xz)           | Apple Silicon macOS | [checksum](https://github.com/orbitinghail/snouty/releases/latest/download/snouty-aarch64-apple-darwin.tar.xz.sha256)      |
| [snouty-x86_64-pc-windows-msvc.zip](https://github.com/orbitinghail/snouty/releases/latest/download/snouty-x86_64-pc-windows-msvc.zip)             | x64 Windows         | [checksum](https://github.com/orbitinghail/snouty/releases/latest/download/snouty-x86_64-pc-windows-msvc.zip.sha256)       |
| [snouty-aarch64-unknown-linux-gnu.tar.xz](https://github.com/orbitinghail/snouty/releases/latest/download/snouty-aarch64-unknown-linux-gnu.tar.xz) | ARM64 Linux         | [checksum](https://github.com/orbitinghail/snouty/releases/latest/download/snouty-aarch64-unknown-linux-gnu.tar.xz.sha256) |
| [snouty-x86_64-unknown-linux-gnu.tar.xz](https://github.com/orbitinghail/snouty/releases/latest/download/snouty-x86_64-unknown-linux-gnu.tar.xz)   | x64 Linux           | [checksum](https://github.com/orbitinghail/snouty/releases/latest/download/snouty-x86_64-unknown-linux-gnu.tar.xz.sha256)  |

## Configuration

Set the following environment variables:

```sh
export ANTITHESIS_USERNAME="your-username"
export ANTITHESIS_PASSWORD="your-password"
export ANTITHESIS_TENANT="your-tenant"
```

## Usage

The `-w`/`--webhook` flag specifies which webhook to call. Common values are `basic_test` (Docker environment) or `basic_k8s_test` (Kubernetes environment), unless you have a custom webhook registered with Antithesis.

### Launch a test run

```
snouty run -w basic_test \
  --antithesis.test_name "my-test" \
  --antithesis.description "nightly test run" \
  --antithesis.config_image config:latest \
  --antithesis.images app:latest \
  --antithesis.duration 30 \
  --antithesis.report.recipients "team@example.com"
```

Parameters can also be passed via stdin as JSON:

```sh
echo '{"antithesis.description": "test", ...}' | snouty run -w basic_test --stdin
```

### Launch a debugging session

Using CLI arguments:

```sh
snouty debug \
  --antithesis.debugging.session_id f89d5c11f5e3bf5e4bb3641809800cee-44-22 \
  --antithesis.debugging.input_hash 6057726200491963783 \
  --antithesis.debugging.vtime 329.8037810830865
```

Using `Moment.from` (copy directly from a triage report):

```sh
echo 'Moment.from({ session_id: "...", input_hash: "...", vtime: ... })' | snouty debug --stdin
```
