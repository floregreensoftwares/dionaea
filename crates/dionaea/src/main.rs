mod clickhouse;
mod config;
mod server;

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use config::{Config, LogFormat};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP server until interrupted
    Serve {
        /// TOML configuration file; `DIONAEA_*` variables override it
        #[arg(long, short)]
        config: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let Cli { command } = Cli::parse();
    match command {
        Command::Serve { config } => serve(config),
    }
}

fn serve(path: Option<PathBuf>) -> ExitCode {
    let config = match load_config(path) {
        Ok(config) => config,
        Err(message) => {
            let _ = writeln!(std::io::stderr(), "error: {message}");
            return ExitCode::from(2);
        }
    };
    init_logging(&config);

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "cannot start the async runtime");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(server::run(&config, server::shutdown_signal())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "server stopped with an error");
            ExitCode::FAILURE
        }
    }
}

fn load_config(path: Option<PathBuf>) -> Result<Config, String> {
    let text = match path {
        Some(path) => Some(
            std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?,
        ),
        None => None,
    };
    Config::resolve(text.as_deref(), |name| std::env::var(name).ok()).map_err(|e| e.to_string())
}

fn init_logging(config: &Config) {
    let filter = EnvFilter::try_new(&config.log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match config.log_format {
        LogFormat::Json => builder.json().init(),
        LogFormat::Text => builder.init(),
    }
}
