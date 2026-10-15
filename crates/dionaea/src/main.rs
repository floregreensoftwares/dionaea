mod clickhouse;
mod config;
mod schema_cli;
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
    /// Work with schema files
    Schema {
        #[command(subcommand)]
        command: SchemaCommand,
    },
}

#[derive(Subcommand)]
enum SchemaCommand {
    /// Validate a schema file; problems are printed as file:line:column
    Check { file: PathBuf },
    /// List the changes from an old schema file to a new one; exits 1 when any is breaking
    Diff { old: PathBuf, new: PathBuf },
}

fn main() -> ExitCode {
    let Cli { command } = Cli::parse();
    match command {
        Command::Serve { config } => serve(config),
        Command::Schema { command } => schema(command),
    }
}

fn schema(command: SchemaCommand) -> ExitCode {
    let (mut out, mut err) = (std::io::stdout().lock(), std::io::stderr().lock());
    let read = |path: &PathBuf, err: &mut dyn Write| {
        schema_cli::read_schema_file(path).inspect_err(|message| {
            let _ = writeln!(err, "error: {message}");
        })
    };
    let code = match command {
        SchemaCommand::Check { file } => match read(&file, &mut err) {
            Ok(source) => {
                schema_cli::check(&file.display().to_string(), &source, &mut out, &mut err)
            }
            Err(_) => schema_cli::UNUSABLE,
        },
        SchemaCommand::Diff { old, new } => match (read(&old, &mut err), read(&new, &mut err)) {
            (Ok(old_source), Ok(new_source)) => schema_cli::compare(
                (&old.display().to_string(), &old_source),
                (&new.display().to_string(), &new_source),
                &mut out,
                &mut err,
            ),
            _ => schema_cli::UNUSABLE,
        },
    };
    ExitCode::from(code)
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
