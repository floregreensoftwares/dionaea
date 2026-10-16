mod collect;
mod config;
mod erasure;
mod meta_cli;
mod net;
mod ops;
mod schema_cli;
mod server;
mod settings;

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use config::{Config, LogFormat};
use dionaea_meta::{ErasureState, MetaStore, SqliteMeta};
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
    /// Create and list sites
    Site {
        #[command(subcommand)]
        command: SiteCommand,
    },
    /// Issue, list and revoke write keys
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
    /// Read and change the limits and intervals a running server follows
    Settings {
        #[command(subcommand)]
        command: SettingsCommand,
    },
    /// Erase every trace of a user: ask for it, follow it, retry a failure
    Erase {
        #[command(subcommand)]
        command: EraseCommand,
    },
}

#[derive(Subcommand)]
enum SchemaCommand {
    /// Validate a schema file; problems are printed as file:line:column
    Check { file: PathBuf },
    /// List the changes from an old schema file to a new one; exits 1 when any is breaking
    Diff { old: PathBuf, new: PathBuf },
    /// Print the JSON Schema of the schema file format, for editor completion
    JsonSchema,
    /// Publish a schema file as the next version of a site; exits 1 when it is refused
    Push {
        /// Id of the site (see `dionaea site list`)
        #[arg(long)]
        site: u32,
        /// Only list what would change; store nothing
        #[arg(long)]
        dry_run: bool,
        file: PathBuf,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// List the published versions of a site
    Versions {
        #[arg(long)]
        site: u32,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Stop accepting events under a published version (never the last active one)
    Retire {
        #[arg(long)]
        site: u32,
        version: u32,
        #[command(flatten)]
        store: StoreArgs,
    },
}

#[derive(clap::Args)]
struct StoreArgs {
    /// TOML configuration file; `DIONAEA_*` variables override it
    #[arg(long, short)]
    config: Option<PathBuf>,
}

#[derive(Subcommand)]
enum SiteCommand {
    /// Create a site; prints its id and name
    Create {
        name: String,
        /// Organization to create the site in (made if missing)
        #[arg(long)]
        organization: Option<String>,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// List sites as id, name and organization
    List {
        #[command(flatten)]
        store: StoreArgs,
    },
}

#[derive(Subcommand)]
enum EraseCommand {
    /// Ask for a user of a site to be erased; prints the request. From this
    /// moment the server drops anything of that user.
    Request {
        #[arg(long)]
        site: u32,
        /// The user id the app sent to identify
        #[arg(long)]
        user: String,
        /// Who is asking, for the audit trail
        #[arg(long, default_value = "cli")]
        by: String,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// List requests: id, site, state, requested by, requested at, hidden at,
    /// completed at, devices, hash of the user id
    List {
        /// Only requests in this state: requested, hidden, complete or failed
        #[arg(long)]
        state: Option<String>,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Show one request and what the job reported about it
    Show {
        id: u64,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Put a failed request back to be tried again
    Retry {
        id: u64,
        #[command(flatten)]
        store: StoreArgs,
    },
}

/// The operational settings: limits and intervals that follow a change within
/// the server's refresh interval, with no restart. Addresses, paths and
/// credentials stay in the file and the environment.
#[derive(Subcommand)]
enum SettingsCommand {
    /// List every setting: name, value in force, where it comes from, its range, what it is
    List {
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Print the value in force of one setting
    Get {
        name: String,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Store a value; the file, the environment and the default give way to it
    Set {
        name: String,
        value: String,
        #[command(flatten)]
        when: When,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Remove a stored value, so the file, the environment or the default applies
    Unset {
        name: String,
        #[command(flatten)]
        when: When,
        #[command(flatten)]
        store: StoreArgs,
    },
}

/// When a change takes effect. Every server that reads the store switches at
/// the same moment, so the default waits one refresh interval plus a second:
/// by then all of them have the change. Servers' clocks are assumed to agree
/// to within about a second.
#[derive(clap::Args)]
struct When {
    /// Take effect this many seconds from now instead
    #[arg(long, conflicts_with = "now")]
    after: Option<u64>,
    /// Take effect on each server's next refresh, not all at once
    #[arg(long)]
    now: bool,
}

impl When {
    /// The delay in milliseconds.
    fn delay_ms(&self, config: &Config) -> i64 {
        let seconds = if self.now {
            0
        } else {
            self.after
                .unwrap_or(config.tuning.refresh_every.as_secs() + 1)
        };
        i64::try_from(seconds.saturating_mul(1000)).unwrap_or(i64::MAX / 2)
    }
}

#[derive(Subcommand)]
enum KeyCommand {
    /// Issue a write key for a site; prints the key
    Issue {
        #[arg(long)]
        site: u32,
        #[arg(long, default_value = "default")]
        label: String,
        /// A web origin allowed to use the key, such as https://example.com; repeatable
        #[arg(long = "origin")]
        origins: Vec<String>,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// List the keys of a site as key, state, label and origins
    List {
        #[arg(long)]
        site: u32,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Revoke a write key
    Revoke {
        key: String,
        #[command(flatten)]
        store: StoreArgs,
    },
}

fn main() -> ExitCode {
    let Cli { command } = Cli::parse();
    match command {
        Command::Serve { config } => serve(config),
        Command::Schema { command } => schema(command),
        Command::Site { command } => site(command),
        Command::Key { command } => key(command),
        Command::Settings { command } => settings_command(command),
        Command::Erase { command } => erase(command),
    }
}

fn erase(command: EraseCommand) -> ExitCode {
    match command {
        EraseCommand::Request {
            site,
            user,
            by,
            store,
        } => with_store(&store, |meta, out, err| {
            meta_cli::erase_request(meta, site, &user, &by, out, err)
        }),
        EraseCommand::List { state, store } => with_store(&store, |meta, out, err| {
            let state = match state.as_deref() {
                None => None,
                Some(text) => match ErasureState::parse(text) {
                    Some(state) => Some(state),
                    None => {
                        let _ = writeln!(
                            err,
                            "error: `{text}` is not a state; use requested, hidden, complete or failed"
                        );
                        return meta_cli::REFUSED;
                    }
                },
            };
            meta_cli::erase_list(meta, state, out, err)
        }),
        EraseCommand::Show { id, store } => with_store(&store, |meta, out, err| {
            meta_cli::erase_show(meta, id, out, err)
        }),
        EraseCommand::Retry { id, store } => with_store(&store, |meta, out, err| {
            meta_cli::erase_retry(meta, id, out, err)
        }),
    }
}

fn schema(command: SchemaCommand) -> ExitCode {
    if matches!(
        command,
        SchemaCommand::Push { .. } | SchemaCommand::Versions { .. } | SchemaCommand::Retire { .. }
    ) {
        return schema_store(command);
    }
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
        SchemaCommand::JsonSchema => schema_cli::json_schema(&mut out),
        SchemaCommand::Push { .. }
        | SchemaCommand::Versions { .. }
        | SchemaCommand::Retire { .. } => {
            unreachable!("handled above")
        }
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
    let served = runtime.block_on(async {
        let stop = server::shutdown_signal();
        server::run(&config, stop).await
    });
    match served {
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

/// Opens the metadata store named by the configuration and runs `work` on it.
fn with_store(
    args: &StoreArgs,
    work: impl FnOnce(&dyn MetaStore, &mut std::io::StdoutLock<'_>, &mut std::io::StderrLock<'_>) -> u8,
) -> ExitCode {
    with_config_store(args, |_, meta, out, err| work(meta, out, err))
}

/// The same, for work that also needs the configuration.
fn with_config_store(
    args: &StoreArgs,
    work: impl FnOnce(
        &Config,
        &dyn MetaStore,
        &mut std::io::StdoutLock<'_>,
        &mut std::io::StderrLock<'_>,
    ) -> u8,
) -> ExitCode {
    let (mut out, mut err) = (std::io::stdout().lock(), std::io::stderr().lock());
    let config = match load_config(args.config.clone()) {
        Ok(config) => config,
        Err(message) => {
            let _ = writeln!(err, "error: {message}");
            return ExitCode::from(meta_cli::UNUSABLE);
        }
    };
    let store = match SqliteMeta::open(&config.metadata_path) {
        Ok(store) => store,
        Err(error) => {
            let _ = writeln!(
                err,
                "error: cannot open {}: {error}",
                config.metadata_path.display()
            );
            return ExitCode::from(meta_cli::UNUSABLE);
        }
    };
    ExitCode::from(work(&config, &store, &mut out, &mut err))
}

fn settings_command(command: SettingsCommand) -> ExitCode {
    let now = || i64::try_from(collect::system_clock()).unwrap_or(i64::MAX / 2);
    match command {
        SettingsCommand::List { store } => with_config_store(&store, |config, meta, out, err| {
            meta_cli::settings_list(
                meta,
                &config.tuning,
                &config.tuning_origins,
                now(),
                out,
                err,
            )
        }),
        SettingsCommand::Get { name, store } => {
            with_config_store(&store, |config, meta, out, err| {
                meta_cli::settings_get(
                    meta,
                    &config.tuning,
                    &config.tuning_origins,
                    now(),
                    &name,
                    out,
                    err,
                )
            })
        }
        SettingsCommand::Set {
            name,
            value,
            when,
            store,
        } => with_config_store(&store, |config, meta, _, err| {
            let moment = (now(), when.delay_ms(config));
            meta_cli::settings_set(meta, &name, &value, moment, err)
        }),
        SettingsCommand::Unset { name, when, store } => {
            with_config_store(&store, |config, meta, _, err| {
                let moment = (now(), when.delay_ms(config));
                meta_cli::settings_unset(meta, &name, moment, err)
            })
        }
    }
}

fn site(command: SiteCommand) -> ExitCode {
    match command {
        SiteCommand::Create {
            name,
            organization,
            store,
        } => with_store(&store, |meta, out, err| {
            meta_cli::site_create(meta, &name, organization.as_deref(), out, err)
        }),
        SiteCommand::List { store } => {
            with_store(&store, |meta, out, err| meta_cli::site_list(meta, out, err))
        }
    }
}

fn key(command: KeyCommand) -> ExitCode {
    match command {
        KeyCommand::Issue {
            site,
            label,
            origins,
            store,
        } => with_store(&store, |meta, out, err| {
            meta_cli::key_issue(meta, site, &label, &origins, out, err)
        }),
        KeyCommand::List { site, store } => with_store(&store, |meta, out, err| {
            meta_cli::key_list(meta, site, out, err)
        }),
        KeyCommand::Revoke { key, store } => {
            with_store(&store, |meta, _, err| meta_cli::key_revoke(meta, &key, err))
        }
    }
}

/// `schema push`, `versions` and `retire`: the schema commands that need the store.
fn schema_store(command: SchemaCommand) -> ExitCode {
    match command {
        SchemaCommand::Push {
            site,
            dry_run,
            file,
            store,
        } => {
            let source = match schema_cli::read_schema_file(&file) {
                Ok(source) => source,
                Err(message) => {
                    let _ = writeln!(std::io::stderr(), "error: {message}");
                    return ExitCode::from(meta_cli::UNUSABLE);
                }
            };
            let label = file.display().to_string();
            with_store(&store, |meta, out, err| {
                meta_cli::schema_push(meta, site, (&label, &source), dry_run, out, err)
            })
        }
        SchemaCommand::Versions { site, store } => with_store(&store, |meta, out, err| {
            meta_cli::schema_versions(meta, site, out, err)
        }),
        SchemaCommand::Retire {
            site,
            version,
            store,
        } => with_store(&store, |meta, _, err| {
            meta_cli::schema_retire(meta, site, version, err)
        }),
        SchemaCommand::Check { .. } | SchemaCommand::Diff { .. } | SchemaCommand::JsonSchema => {
            unreachable!("handled by `schema`")
        }
    }
}
