pub mod api;
pub mod error;
pub mod moment;
pub mod params;

use std::io::{self, Read};

use chrono::{Duration, Local};
use clap::{Parser, Subcommand};
use log::{debug, info};

use crate::api::AntithesisApi;
use crate::error::{Error, Result};
use crate::params::Params;

#[derive(Parser)]
#[command(name = "snouty")]
#[command(about = "CLI for the Antithesis API", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Launch a test run
    #[command(long_about = r#"Launch a test run

Example:
  snouty run -w basic_test \
    --antithesis.description "nightly test run" \
    --antithesis.config_image config:latest \
    --antithesis.images app:latest \
    --antithesis.duration 30 \
    --antithesis.report.recipients "team@example.com""#)]
    Run {
        /// Webhook endpoint name (e.g., basic_test, basic_k8s_test)
        #[arg(short, long)]
        webhook: String,

        /// Read parameters from stdin (JSON or Moment.from format)
        #[arg(long)]
        stdin: bool,

        /// Parameters as `--key value` pairs
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Launch a debugging session
    #[command(long_about = r#"Launch a debugging session

Using CLI arguments:
  snouty debug \
    --antithesis.debugging.session_id f89d5c11f5e3bf5e4bb3641809800cee-44-22 \
    --antithesis.debugging.input_hash 6057726200491963783 \
    --antithesis.debugging.vtime 329.8037810830865

Using Moment.from (copy from triage report):
  echo 'Moment.from({ session_id: "...", input_hash: "...", vtime: ... })' | snouty debug --stdin"#)]
    Debug {
        /// Read parameters from stdin (JSON or Moment.from format)
        #[arg(long)]
        stdin: bool,

        /// Parameters as `--key value` pairs
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Print version information
    Version,
}

fn read_stdin() -> Result<String> {
    let mut buf = String::new();
    io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| error::Error::InvalidArgs(format!("failed to read stdin: {}", e)))?;
    let buf = buf.trim().to_string();
    Ok(buf)
}

fn get_params(args: Vec<String>, use_stdin: bool, support_moment: bool) -> Result<Params> {
    if use_stdin {
        let input = read_stdin()?;
        if support_moment && moment::is_moment_format(&input) {
            debug!("detected Moment.from on stdin");
            return moment::parse(&input);
        }
        debug!("parsing input as JSON");
        let value: serde_json::Value = json5::from_str(&input)
            .map_err(|e| error::Error::InvalidArgs(format!("invalid JSON: {}", e)))?;
        Params::from_json(&value)
    } else if args.is_empty() {
        Err(Error::InvalidArgs("no parameters provided".to_string()))
    } else {
        Params::from_args(&args)
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    env_logger::init();
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Run {
            webhook,
            stdin,
            args,
        } => {
            info!("running test with webhook: {}", webhook);
            cmd_run(webhook, args, stdin).await
        }
        Commands::Debug { stdin, args } => {
            info!("starting debug session");
            cmd_debug(args, stdin).await
        }
        Commands::Version => {
            println!("snouty {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    };

    if let Err(e) = result {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
}

async fn cmd_run(webhook: String, args: Vec<String>, use_stdin: bool) -> Result<()> {
    let params = get_params(args, use_stdin, false)?;
    params.validate_test_params()?;

    // Print params to stderr for user visibility (with sensitive values redacted)
    eprintln!(
        "\nRequesting Antithesis test run with params:\n{}",
        serde_json::to_string_pretty(&params.to_redacted_map()).unwrap()
    );

    let api = AntithesisApi::from_env()?;
    let response = api
        .post(&format!("/launch/{}", webhook))
        .json(&serde_json::json!({ "params": params.to_value() }))
        .send()
        .await?;

    let status = response.status();
    let body = response.text().await?;
    debug!("response status: {}, body:\n{}", status, body);

    if status.is_success() {
        // Estimate when the report email will arrive
        let duration_mins: i64 = params
            .as_map()
            .get("antithesis.duration")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let eta = Local::now() + Duration::minutes(duration_mins + 10);
        eprintln!(
            "\nExpect a report email from Antithesis around {}",
            eta.format("%b %-d at %-I:%M %p")
        );

        Ok(())
    } else {
        Err(error::Error::Api {
            status: status.as_u16(),
            message: body,
        })
    }
}

async fn cmd_debug(args: Vec<String>, use_stdin: bool) -> Result<()> {
    let params = get_params(args, use_stdin, true)?;
    params.validate_debugging_params()?;

    // Print params to stderr for user visibility (with sensitive values redacted)
    eprintln!(
        "\nRequesting the Antithesis multiverse debugger with params:\n{}",
        serde_json::to_string_pretty(&params.to_redacted_map()).unwrap()
    );

    let api = AntithesisApi::from_env()?;
    let response = api
        .post("/launch/debugging")
        .json(&serde_json::json!({ "params": params.to_value() }))
        .send()
        .await?;

    let status = response.status();
    let body = response.text().await?;
    debug!("response status: {}, body length: {}", status, body.len());

    if status.is_success() {
        println!("{}", body);

        // Estimate when the debugging session email will arrive
        let eta = Local::now() + Duration::minutes(10);
        eprintln!(
            "\nExpect a debugging session email from Antithesis around {}",
            eta.format("%b %-d at %-I:%M %p")
        );

        Ok(())
    } else {
        Err(error::Error::Api {
            status: status.as_u16(),
            message: body,
        })
    }
}
