pub mod api;
pub mod error;
pub mod params;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "snouty")]
#[command(about = "A CLI tool", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the application
    Run,
    /// Debug the application
    Debug,
    /// Print version information
    Version,
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run => {
            println!("Running...");
            // TODO: implement run logic
        }
        Commands::Debug => {
            println!("Debugging...");
            // TODO: implement debug logic
        }
        Commands::Version => {
            println!("snouty {}", env!("CARGO_PKG_VERSION"));
        }
    }
}
