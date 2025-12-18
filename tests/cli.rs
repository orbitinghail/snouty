use assert_cmd::Command;
use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
use std::io::Write;
use std::net::TcpListener;
use std::thread;

fn snouty() -> Command {
    let mut cmd = cargo_bin_cmd!("snouty");
    cmd.env("RUST_LOG", "debug");
    cmd
}

/// Start a simple mock HTTP server that returns a fixed response.
/// Returns the server URL and a handle to stop it.
fn start_mock_server(response_body: &'static str, status: u16) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{}", addr);

    thread::spawn(move || {
        for stream in listener.incoming() {
            if let Ok(mut stream) = stream {
                // Read request (we don't care about the content for these tests)
                let mut buf = [0u8; 4096];
                let _ = std::io::Read::read(&mut stream, &mut buf);

                // Send response
                let response = format!(
                    "HTTP/1.1 {} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    status,
                    response_body.len(),
                    response_body
                );
                let _ = stream.write_all(response.as_bytes());
                break; // Only handle one request
            }
        }
    });

    url
}

fn snouty_with_mock(mock_url: &str) -> Command {
    let mut cmd = snouty();
    cmd.env("ANTITHESIS_USERNAME", "testuser")
        .env("ANTITHESIS_PASSWORD", "testpass")
        .env("ANTITHESIS_TENANT", "testtenant")
        .env("ANTITHESIS_BASE_URL", mock_url);
    cmd
}

// === Tests that don't need API (version, help) ===

#[test]
fn version_prints_version() {
    snouty()
        .arg("version")
        .assert()
        .success()
        .stdout(predicate::str::is_match(r"^snouty \d+\.\d+\.\d+").unwrap());
}

#[test]
fn help_shows_subcommands() {
    snouty()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("run"))
        .stdout(predicate::str::contains("debug"))
        .stdout(predicate::str::contains("version"));
}

// === Tests for run command ===

#[test]
fn run_with_cli_args() {
    let mock_url = start_mock_server(r#"{"status": "ok"}"#, 200);

    // Test all fields recommended in the README
    snouty_with_mock(&mock_url)
        .args([
            "run",
            "-w",
            "basic_test",
            "--antithesis.test_name",
            "my-test",
            "--antithesis.description",
            "nightly test run",
            "--antithesis.config_image",
            "config:latest",
            "--antithesis.images",
            "app:latest",
            "--antithesis.duration",
            "30",
            "--antithesis.report.recipients",
            "team@example.com",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            r#""antithesis.test_name": "my-test""#,
        ))
        .stderr(predicate::str::contains(
            r#""antithesis.description": "nightly test run""#,
        ))
        .stderr(predicate::str::contains(
            r#""antithesis.config_image": "config:latest""#,
        ))
        .stderr(predicate::str::contains(
            r#""antithesis.images": "app:latest""#,
        ))
        .stderr(predicate::str::contains(r#""antithesis.duration": "30""#))
        .stderr(predicate::str::contains(
            r#""antithesis.report.recipients": "[REDACTED]""#,
        ))
        .stderr(predicate::str::contains(r#""status": "ok""#));
}

#[test]
fn run_with_stdin_json() {
    let mock_url = start_mock_server(r#"{"launched": true}"#, 200);

    snouty_with_mock(&mock_url)
        .args(["run", "-w", "basic_test", "--stdin"])
        .write_stdin(r#"{"antithesis.duration": "60", "antithesis.is_ephemeral": "true"}"#)
        .assert()
        .success()
        .stderr(predicate::str::contains(r#""antithesis.duration": "60""#))
        .stderr(predicate::str::contains(
            r#""antithesis.is_ephemeral": "true""#,
        ));
}

#[test]
fn run_with_custom_properties() {
    let mock_url = start_mock_server(r#"{"ok": true}"#, 200);

    snouty_with_mock(&mock_url)
        .args([
            "run",
            "-w",
            "basic_test",
            "--antithesis.duration",
            "30",
            "--my.custom.prop",
            "value",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains(r#""my.custom.prop": "value""#));
}

#[test]
fn run_stdin_flag_required_for_stdin_input() {
    let mock_url = start_mock_server(r#"{"ok": true}"#, 200);

    // Without --stdin flag, stdin is not read even if provided
    snouty_with_mock(&mock_url)
        .args(["run", "-w", "basic_test", "--antithesis.duration", "30"])
        .write_stdin(r#"{"antithesis.duration": "SHOULD_BE_IGNORED"}"#)
        .assert()
        .success()
        .stderr(predicate::str::contains(r#""antithesis.duration": "30""#))
        .stderr(predicate::str::contains("SHOULD_BE_IGNORED").not());
}

#[test]
fn run_with_k8s_webhook() {
    let mock_url = start_mock_server(r#"{"status": "ok"}"#, 200);

    snouty_with_mock(&mock_url)
        .args([
            "run",
            "--webhook",
            "basic_k8s_test",
            "--antithesis.duration",
            "30",
        ])
        .assert()
        .success();
}

#[test]
fn run_with_custom_webhook() {
    let mock_url = start_mock_server(r#"{"status": "ok"}"#, 200);

    snouty_with_mock(&mock_url)
        .args([
            "run",
            "-w",
            "my_custom_webhook",
            "--antithesis.duration",
            "30",
        ])
        .assert()
        .success();
}

// === Tests for debug command ===

#[test]
fn debug_with_cli_args() {
    let mock_url = start_mock_server(r#"{"session": "started"}"#, 200);

    snouty_with_mock(&mock_url)
        .args([
            "debug",
            "--antithesis.debugging.input_hash",
            "abc123",
            "--antithesis.debugging.session_id",
            "sess-456",
            "--antithesis.debugging.vtime",
            "1234567890",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            r#""antithesis.debugging.input_hash": "abc123""#,
        ));
}

#[test]
fn debug_with_moment_from_format() {
    let mock_url = start_mock_server(r#"{"debugging": true}"#, 200);
    let moment_input = r#"Moment.from({ session_id: "f89d5c11f5e3bf5e4bb3641809800cee-44-22", input_hash: "6057726200491963783", vtime: 329.8037810830865 })"#;

    snouty_with_mock(&mock_url)
        .args(["debug", "--stdin"])
        .write_stdin(moment_input)
        .assert()
        .success()
        .stderr(predicate::str::contains(
            r#""antithesis.debugging.session_id": "f89d5c11f5e3bf5e4bb3641809800cee-44-22""#,
        ))
        .stderr(predicate::str::contains(
            r#""antithesis.debugging.input_hash": "6057726200491963783""#,
        ))
        .stderr(predicate::str::contains(
            r#""antithesis.debugging.vtime": "329.8037810830865""#,
        ));
}

#[test]
fn debug_with_stdin_json() {
    let mock_url = start_mock_server(r#"{"ok": true}"#, 200);
    let json = r#"{
        "antithesis.debugging.input_hash": "abc",
        "antithesis.debugging.session_id": "sess",
        "antithesis.debugging.vtime": "123"
    }"#;

    snouty_with_mock(&mock_url)
        .args(["debug", "--stdin"])
        .write_stdin(json)
        .assert()
        .success()
        .stderr(predicate::str::contains(
            r#""antithesis.debugging.input_hash": "abc""#,
        ));
}

// === Validation error tests ===

#[test]
fn debug_fails_missing_required_fields() {
    let mock_url = start_mock_server(r#"{}"#, 200);

    snouty_with_mock(&mock_url)
        .args(["debug", "--antithesis.debugging.input_hash", "abc"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("validation failed"));
}

#[test]
fn debug_rejects_custom_properties() {
    let mock_url = start_mock_server(r#"{}"#, 200);

    snouty_with_mock(&mock_url)
        .args([
            "debug",
            "--antithesis.debugging.input_hash",
            "abc",
            "--antithesis.debugging.session_id",
            "sess",
            "--antithesis.debugging.vtime",
            "123",
            "--my.custom.prop",
            "value",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("validation failed"));
}

// === Input error tests ===

#[test]
fn run_fails_on_invalid_json_stdin() {
    let mock_url = start_mock_server(r#"{}"#, 200);

    snouty_with_mock(&mock_url)
        .args(["run", "-w", "basic_test", "--stdin"])
        .write_stdin("not valid json")
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid JSON"));
}

#[test]
fn run_fails_on_missing_value() {
    let mock_url = start_mock_server(r#"{}"#, 200);

    snouty_with_mock(&mock_url)
        .args(["run", "-w", "basic_test", "--antithesis.duration"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("missing value"));
}

#[test]
fn run_fails_on_unexpected_arg() {
    let mock_url = start_mock_server(r#"{}"#, 200);

    snouty_with_mock(&mock_url)
        .args(["run", "-w", "basic_test", "notaflag"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn run_fails_without_webhook() {
    let mock_url = start_mock_server(r#"{}"#, 200);

    snouty_with_mock(&mock_url)
        .args(["run", "--antithesis.duration", "30"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--webhook"));
}

// === API error tests ===

#[test]
fn run_reports_api_errors() {
    let mock_url = start_mock_server(r#"{"error": "bad request"}"#, 400);

    snouty_with_mock(&mock_url)
        .args(["run", "-w", "basic_test", "--antithesis.duration", "30"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("API error: 400"));
}

#[test]
fn run_fails_without_credentials() {
    snouty()
        .env_remove("ANTITHESIS_USERNAME")
        .env_remove("ANTITHESIS_PASSWORD")
        .env_remove("ANTITHESIS_TENANT")
        .env_remove("ANTITHESIS_BASE_URL")
        .args(["run", "-w", "basic_test", "--antithesis.duration", "30"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("missing environment variable"));
}

#[test]
fn run_fails_without_parameters() {
    let mock_url = start_mock_server(r#"{}"#, 200);

    snouty_with_mock(&mock_url)
        .args(["run", "-w", "basic_test"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no parameters provided"));
}

#[test]
fn debug_reports_api_errors() {
    let mock_url = start_mock_server(r#"{"error": "unauthorized"}"#, 401);

    snouty_with_mock(&mock_url)
        .args([
            "debug",
            "--antithesis.debugging.input_hash",
            "abc123",
            "--antithesis.debugging.session_id",
            "sess-456",
            "--antithesis.debugging.vtime",
            "1234567890",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("API error: 401"));
}

// === Tests for merging stdin and CLI args ===

#[test]
fn run_merges_stdin_json_with_cli_args() {
    let mock_url = start_mock_server(r#"{"ok": true}"#, 200);

    snouty_with_mock(&mock_url)
        .args([
            "run",
            "-w",
            "basic_test",
            "--stdin",
            "--antithesis.report.recipients",
            "team@example.com",
        ])
        .write_stdin(r#"{"antithesis.duration": "60", "antithesis.description": "from stdin"}"#)
        .assert()
        .success()
        // Values from stdin should be present
        .stderr(predicate::str::contains(r#""antithesis.duration": "60""#))
        .stderr(predicate::str::contains(
            r#""antithesis.description": "from stdin""#,
        ))
        // CLI arg should be merged in
        .stderr(predicate::str::contains(
            r#""antithesis.report.recipients": "[REDACTED]""#,
        ));
}

#[test]
fn run_cli_args_override_stdin_json() {
    let mock_url = start_mock_server(r#"{"ok": true}"#, 200);

    snouty_with_mock(&mock_url)
        .args([
            "run",
            "-w",
            "basic_test",
            "--stdin",
            "--antithesis.duration",
            "120",
        ])
        .write_stdin(r#"{"antithesis.duration": "60", "antithesis.description": "from stdin"}"#)
        .assert()
        .success()
        // CLI arg should override stdin value
        .stderr(predicate::str::contains(r#""antithesis.duration": "120""#))
        // Stdin-only value should still be present
        .stderr(predicate::str::contains(
            r#""antithesis.description": "from stdin""#,
        ));
}

#[test]
fn debug_merges_moment_with_cli_args() {
    let mock_url = start_mock_server(r#"{"debugging": true}"#, 200);
    let moment_input = r#"Moment.from({ session_id: "f89d5c11f5e3bf5e4bb3641809800cee-44-22", input_hash: "6057726200491963783", vtime: 329.8037810830865 })"#;

    snouty_with_mock(&mock_url)
        .args([
            "debug",
            "--stdin",
            "--antithesis.report.recipients",
            "team@example.com",
        ])
        .write_stdin(moment_input)
        .assert()
        .success()
        // Moment params should be present
        .stderr(predicate::str::contains(
            r#""antithesis.debugging.session_id": "f89d5c11f5e3bf5e4bb3641809800cee-44-22""#,
        ))
        .stderr(predicate::str::contains(
            r#""antithesis.debugging.input_hash": "6057726200491963783""#,
        ))
        .stderr(predicate::str::contains(
            r#""antithesis.debugging.vtime": "329.8037810830865""#,
        ))
        // CLI arg should be merged in
        .stderr(predicate::str::contains(
            r#""antithesis.report.recipients": "[REDACTED]""#,
        ));
}
