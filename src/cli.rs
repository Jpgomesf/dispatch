use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use clap::{Parser, Subcommand};
use serde_json::json;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;

use crate::config::{Config, load_config};
use crate::durations::parse_duration;
use crate::intake::secrets::{FileStatus, Secrets, process_env};
use crate::intake::{EventKind, IncomingEvent, SourceKind, notifications, required_keys};
use crate::paths::{EnvPaths, Paths, resolve_config_path};
use crate::results::CardOutcome;
use crate::runner::Runner;
use crate::session::{Session, Shutdown};
use crate::state::InstanceLock;
use crate::store::{Enqueued, Store};

pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_BAD_CONFIG: i32 = 2;

#[derive(Debug, Parser)]
#[command(name = "dispatch", about = "dispatch runner", version)]
struct Cli {
    /// config.toml path (default: $DISPATCH_CONFIG or ~/.config/dispatch/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Intake, triage (on events and as a fallback sweep) and the cards and discussions it
    /// queues
    Heartbeat {
        /// Override the fallback sweep interval, e.g. 30m
        #[arg(long, value_parser = parse_duration)]
        interval: Option<Duration>,
        /// Run a single triage (over pending events, else a sweep), its sessions, and exit
        #[arg(long)]
        once: bool,
    },
    /// Work one card end to end
    Card {
        #[arg(value_name = "REF")]
        card_ref: String,
        /// Workspace name from config
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Queue a manual event for this runner's next triage
    Enqueue {
        /// Source label, e.g. manual
        source: String,
        /// Event text
        text: String,
    },
    /// Create the kill switch
    Stop,
    /// Remove the kill switch
    Resume,
    /// Validate config, print resolved paths and which sources have their keys
    Check,
}

/// Parse `argv`, run the command and return the process exit code.
pub async fn run<S: Session>(
    argv: impl IntoIterator<Item = OsString>,
    env: EnvPaths,
    session: impl FnOnce() -> S,
) -> i32 {
    let cli = match Cli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            return error.exit_code();
        }
    };
    let config_path = resolve_config_path(cli.config.as_deref(), env.config.as_deref());
    let allow_missing = matches!(cli.command, Command::Stop | Command::Resume);
    let config = match load(&config_path, allow_missing) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("invalid config {}: {error}", config_path.display());
            return EXIT_BAD_CONFIG;
        }
    };
    let paths = Paths::resolve(&config_path, &config, &env);
    match cli.command {
        Command::Check => cmd_check(&paths, &config),
        Command::Stop => cmd_stop(&paths),
        Command::Resume => cmd_resume(&paths),
        Command::Enqueue { source, text } => cmd_enqueue(&paths, &config, &source, &text).await,
        Command::Heartbeat { interval, once } => {
            let Ok((_instance, store)) = open_runner(&paths) else {
                return EXIT_FAILED;
            };
            let interval = interval.unwrap_or(config.triage.interval);
            let runner = Runner::new(config, paths, store, session());
            install_signal_handlers(runner.shutdown_handle());
            runner.heartbeat(interval, once).await;
            EXIT_OK
        }
        Command::Card {
            card_ref,
            workspace,
        } => {
            let Ok((_instance, store)) = open_runner(&paths) else {
                return EXIT_FAILED;
            };
            let runner = Runner::new(config, paths, store, session());
            cmd_card(runner, &card_ref, workspace.as_deref()).await
        }
    }
}

/// Only one session-running process per runner name (held until the caller drops it), then
/// the shared store.
fn open_runner(paths: &Paths) -> Result<(InstanceLock, Store), ()> {
    let lock_file = paths.instance_lock_file();
    let instance = match InstanceLock::try_acquire(&lock_file) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            eprintln!(
                "another dispatch heartbeat or card run holds {}; not starting",
                lock_file.display()
            );
            return Err(());
        }
        Err(error) => {
            eprintln!("cannot lock {}: {error:#}", lock_file.display());
            return Err(());
        }
    };
    let store = open_store(paths)?;
    Ok((instance, store))
}

fn open_store(paths: &Paths) -> Result<Store, ()> {
    Store::open(&paths.db).map_err(|error| eprintln!("cannot open store: {error}"))
}

fn load(path: &Path, allow_missing: bool) -> Result<Config, String> {
    if allow_missing && !path.exists() {
        return Ok(Config::default());
    }
    load_config(path)
}

fn cmd_check(paths: &Paths, config: &Config) -> i32 {
    let plugin_ok = paths
        .plugin_dir
        .join(".claude-plugin/plugin.json")
        .is_file();
    let kill = if paths.kill_switch().exists() {
        "SET"
    } else {
        "off"
    };
    let plugin = if plugin_ok {
        "ok"
    } else {
        "MISSING plugin.json"
    };
    // `check` changes nothing: a store that does not exist yet is created by the first run.
    let store = if !paths.db.exists() {
        "not created yet".to_string()
    } else {
        match Store::open(&paths.db) {
            Ok(_) => "ok".to_string(),
            Err(error) => format!("ERROR {error}"),
        }
    };
    let secrets = Secrets::load(&paths.secrets, process_env());
    let secrets_state = match &secrets.status {
        FileStatus::Missing => "missing".to_string(),
        FileStatus::Loaded => "ok".to_string(),
        FileStatus::Refused(reason) => format!("REFUSED {reason}"),
    };
    println!("runner:       {}", config.name);
    println!("config:       {} (ok)", paths.config.display());
    println!("state dir:    {}", paths.state_dir.display());
    println!("store:        {} ({store})", paths.db.display());
    println!(
        "secrets:      {} ({secrets_state})",
        paths.secrets.display()
    );
    println!("kill switch:  {} ({kill})", paths.kill_switch().display());
    println!("outreach:     {}", paths.outreach_file.display());
    println!("worktrees:    {}", paths.worktrees_dir().display());
    println!("plugin dir:   {} ({plugin})", paths.plugin_dir.display());
    for kind in SourceKind::enabled(&config.intake) {
        println!(
            "source:       {} ({})",
            kind.name(),
            source_readiness(kind, paths, config, &secrets)
        );
    }
    if plugin_ok { EXIT_OK } else { EXIT_FAILED }
}

/// Key presence per source, never values.
fn source_readiness(kind: SourceKind, paths: &Paths, config: &Config, secrets: &Secrets) -> String {
    if kind == SourceKind::Notifications {
        return match notifications::probe(&paths.notifications_db) {
            Ok(()) => "notification DB readable".into(),
            Err(error) => format!("notification DB unreadable (Full Disk Access?): {error}"),
        };
    }
    let keys: Vec<String> = required_keys(kind, &config.intake)
        .into_iter()
        .map(|name| {
            let present = if secrets.has(name) {
                "present"
            } else {
                "MISSING"
            };
            format!("{name} {present}")
        })
        .collect();
    keys.join(", ")
}

async fn cmd_enqueue(paths: &Paths, config: &Config, source: &str, text: &str) -> i32 {
    let Ok(store) = open_store(paths) else {
        return EXIT_FAILED;
    };
    let now = Utc::now();
    let event = IncomingEvent {
        source: source.to_string(),
        external_id: format!(
            "{}-{}",
            now.timestamp_nanos_opt().unwrap_or_default(),
            std::process::id()
        ),
        kind: EventKind::Message,
        mentions_me: false,
        sender: None,
        occurred_at: now,
        payload: json!({"body": text}),
    };
    let runner = config.name.clone();
    match store.call(move |s| s.enqueue(&runner, &event, now)).await {
        Ok(Enqueued::Inserted(id)) => {
            println!("enqueued event {id} for {}", config.name);
            EXIT_OK
        }
        Ok(other) => {
            eprintln!("not enqueued: {other:?}");
            EXIT_FAILED
        }
        Err(error) => {
            eprintln!("cannot enqueue: {error}");
            EXIT_FAILED
        }
    }
}

fn cmd_stop(paths: &Paths) -> i32 {
    let created = std::fs::create_dir_all(&paths.state_dir)
        .and_then(|()| std::fs::write(paths.kill_switch(), ""));
    match created {
        Ok(()) => {
            println!("kill switch set: {}", paths.kill_switch().display());
            EXIT_OK
        }
        Err(error) => {
            eprintln!(
                "cannot create kill switch {}: {error}",
                paths.kill_switch().display()
            );
            EXIT_FAILED
        }
    }
}

fn cmd_resume(paths: &Paths) -> i32 {
    match std::fs::remove_file(paths.kill_switch()) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            eprintln!(
                "cannot remove kill switch {}: {error}",
                paths.kill_switch().display()
            );
            return EXIT_FAILED;
        }
    }
    println!("kill switch removed: {}", paths.kill_switch().display());
    EXIT_OK
}

async fn cmd_card<S: Session>(runner: Runner<S>, card_ref: &str, workspace: Option<&str>) -> i32 {
    if runner.killed() {
        eprintln!(
            "kill switch present ({}); not starting",
            runner.paths.kill_switch().display()
        );
        return EXIT_FAILED;
    }
    install_signal_handlers(runner.shutdown_handle());
    runner.recover().await;
    match runner.run_card(card_ref, workspace).await {
        Err(error) => {
            eprintln!("{error}");
            EXIT_BAD_CONFIG
        }
        Ok(Some(result)) if result.status != CardOutcome::Failed => EXIT_OK,
        Ok(_) => EXIT_FAILED,
    }
}

/// First SIGINT/SIGTERM: start nothing new and SIGTERM running sessions. Second: SIGKILL
/// them. Third: exit immediately.
fn install_signal_handlers(shutdown: Arc<watch::Sender<Shutdown>>) {
    let (Ok(mut interrupt), Ok(mut terminate)) = (
        signal(SignalKind::interrupt()),
        signal(SignalKind::terminate()),
    ) else {
        eprintln!("cannot install signal handlers");
        return;
    };
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = interrupt.recv() => {}
                _ = terminate.recv() => {}
            }
            let next = match *shutdown.borrow() {
                Shutdown::Run => Shutdown::Graceful,
                Shutdown::Graceful => Shutdown::Force,
                Shutdown::Force => std::process::exit(130),
            };
            shutdown.send_replace(next);
            match next {
                Shutdown::Graceful => {
                    eprintln!("stopping: running sessions asked to end; signal again to kill them")
                }
                _ => eprintln!("killing running sessions"),
            }
        }
    });
}

#[cfg(test)]
mod tests;
