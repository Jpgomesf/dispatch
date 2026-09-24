use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;

use crate::config::{Config, load_config};
use crate::durations::parse_duration;
use crate::paths::{Paths, resolve_config_path};
use crate::results::CardOutcome;
use crate::runner::Runner;
use crate::session::{Session, Shutdown};
use crate::state::InstanceLock;

pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_BAD_CONFIG: i32 = 2;

#[derive(Debug, Parser)]
#[command(name = "harness", about = "claude-harness runner", version)]
struct Cli {
    /// config.toml path (default: $HARNESS_CONFIG or ~/.config/claude-harness/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Triage loop over Slack and the tracker; runs the cards it queues
    Heartbeat {
        /// Override the configured interval, e.g. 10m
        #[arg(long, value_parser = parse_duration)]
        interval: Option<Duration>,
        /// Run a single triage (and its cards) and exit
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
    /// Create the kill switch
    Stop,
    /// Remove the kill switch
    Resume,
    /// Validate config and print resolved paths
    Check,
}

/// Parse `argv`, run the command and return the process exit code.
pub async fn run<S: Session>(
    argv: impl IntoIterator<Item = OsString>,
    env_config: Option<String>,
    session: impl FnOnce() -> S,
) -> i32 {
    let cli = match Cli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            return error.exit_code();
        }
    };
    let config_path = resolve_config_path(cli.config.as_deref(), env_config.as_deref());
    let allow_missing = matches!(cli.command, Command::Stop | Command::Resume);
    let config = match load(&config_path, allow_missing) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("invalid config {}: {error}", config_path.display());
            return EXIT_BAD_CONFIG;
        }
    };
    let paths = Paths::resolve(&config_path, &config);
    // Only one session-running process per state dir, held until this function returns.
    let _instance = match cli.command {
        Command::Heartbeat { .. } | Command::Card { .. } => match single_instance(&paths) {
            Ok(lock) => Some(lock),
            Err(code) => return code,
        },
        _ => None,
    };
    match cli.command {
        Command::Check => cmd_check(&paths),
        Command::Stop => cmd_stop(&paths),
        Command::Resume => cmd_resume(&paths),
        Command::Heartbeat { interval, once } => {
            let interval = interval.unwrap_or(config.heartbeat.interval);
            let runner = Runner::new(config, paths, session());
            install_signal_handlers(runner.shutdown_handle());
            runner.heartbeat(interval, once).await;
            EXIT_OK
        }
        Command::Card {
            card_ref,
            workspace,
        } => {
            let runner = Runner::new(config, paths, session());
            cmd_card(runner, &card_ref, workspace.as_deref()).await
        }
    }
}

fn single_instance(paths: &Paths) -> Result<InstanceLock, i32> {
    let lock_file = paths.state_dir.join("harness.lock");
    match InstanceLock::try_acquire(&paths.state_dir) {
        Ok(Some(lock)) => Ok(lock),
        Ok(None) => {
            eprintln!(
                "another harness heartbeat or card run holds {}; not starting",
                lock_file.display()
            );
            Err(EXIT_FAILED)
        }
        Err(error) => {
            eprintln!("cannot lock {}: {error:#}", lock_file.display());
            Err(EXIT_FAILED)
        }
    }
}

fn load(path: &Path, allow_missing: bool) -> Result<Config, String> {
    if allow_missing && !path.exists() {
        return Ok(Config::default());
    }
    load_config(path)
}

fn cmd_check(paths: &Paths) -> i32 {
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
    println!("config:       {} (ok)", paths.config.display());
    println!("state dir:    {}", paths.state_dir.display());
    println!("state file:   {}", paths.state_file().display());
    println!("kill switch:  {} ({kill})", paths.kill_switch().display());
    println!("outreach:     {}", paths.outreach_file.display());
    println!("worktrees:    {}", paths.worktrees_dir().display());
    println!("plugin dir:   {} ({plugin})", paths.plugin_dir.display());
    if plugin_ok { EXIT_OK } else { EXIT_FAILED }
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
mod tests {
    use super::*;
    use crate::testing::*;

    fn argv(env: &TestEnv, args: &[&str]) -> Vec<OsString> {
        let mut all: Vec<OsString> = vec!["harness".into(), "--config".into()];
        all.push(env.paths.config.clone().into());
        all.extend(args.iter().map(OsString::from));
        all
    }

    async fn run_with(
        env: &TestEnv,
        args: &[&str],
        session: FakeSession,
    ) -> (i32, Arc<FakeSession>) {
        let shared = Arc::new(session);
        let handle = shared.clone();
        let code = run(argv(env, args), None, move || SharedSession(handle)).await;
        (code, shared)
    }

    /// Lets a test keep a handle on the fake after the runner takes ownership.
    struct SharedSession(Arc<FakeSession>);

    impl Session for SharedSession {
        async fn run(
            &self,
            request: crate::session::SessionRequest,
            shutdown: watch::Receiver<Shutdown>,
        ) -> Result<crate::session::SessionOutcome, crate::session::SessionError> {
            self.0.run(request, shutdown).await
        }
    }

    fn no_session() -> FakeSession {
        FakeSession::sequence(vec![])
    }

    #[tokio::test]
    async fn check_reports_paths() {
        let env = test_env();
        write_plugin(env.dir.path());
        assert_eq!(run_with(&env, &["check"], no_session()).await.0, EXIT_OK);
    }

    #[tokio::test]
    async fn check_fails_without_plugin() {
        let env = test_env();
        assert_eq!(
            run_with(&env, &["check"], no_session()).await.0,
            EXIT_FAILED
        );
    }

    #[tokio::test]
    async fn invalid_or_missing_config_exits_2() {
        let env = test_env();
        std::fs::write(&env.paths.config, "[heartbeat]\neffort = \"extreme\"\n").unwrap();
        assert_eq!(
            run_with(&env, &["check"], no_session()).await.0,
            EXIT_BAD_CONFIG
        );
        std::fs::remove_file(&env.paths.config).unwrap();
        assert_eq!(
            run_with(&env, &["check"], no_session()).await.0,
            EXIT_BAD_CONFIG
        );
    }

    #[tokio::test]
    async fn stop_and_resume() {
        let env = test_env();
        let kill_switch = env.dir.path().join("state/STOP");
        assert_eq!(run_with(&env, &["stop"], no_session()).await.0, EXIT_OK);
        assert!(kill_switch.exists());
        assert_eq!(run_with(&env, &["resume"], no_session()).await.0, EXIT_OK);
        assert!(!kill_switch.exists());
        assert_eq!(run_with(&env, &["resume"], no_session()).await.0, EXIT_OK);
    }

    #[tokio::test]
    async fn config_from_env() {
        let env = test_env();
        let config = env.paths.config.display().to_string();
        let args = ["harness", "stop"].map(OsString::from);
        assert_eq!(run(args, Some(config), no_session).await, EXIT_OK);
        assert!(env.dir.path().join("state/STOP").exists());
    }

    #[tokio::test]
    async fn card_command() {
        let env = test_env();
        let session = FakeSession::sequence(vec![ok(card_output("EX-1", "done"))]);
        let (code, session) = run_with(&env, &["card", "EX-1"], session).await;
        assert_eq!(code, EXIT_OK);
        assert_eq!(
            session.first_lines(),
            ["/claude-harness:workflow card EX-1"]
        );
    }

    #[tokio::test]
    async fn card_command_failure_exit_code() {
        let env = test_env();
        let (code, _) = run_with(
            &env,
            &["card", "EX-1"],
            FakeSession::sequence(vec![fail("boom")]),
        )
        .await;
        assert_eq!(code, EXIT_FAILED);
        let failed = FakeSession::sequence(vec![ok(card_output("EX-1", "failed"))]);
        assert_eq!(
            run_with(&env, &["card", "EX-1"], failed).await.0,
            EXIT_FAILED
        );
    }

    #[tokio::test]
    async fn card_unknown_workspace() {
        let env = test_env();
        let args = ["card", "EX-1", "--workspace", "missing"];
        assert_eq!(run_with(&env, &args, no_session()).await.0, EXIT_BAD_CONFIG);
    }

    #[tokio::test]
    async fn card_refuses_when_stopped() {
        let env = test_env();
        run_with(&env, &["stop"], no_session()).await;
        let (code, session) = run_with(&env, &["card", "EX-1"], no_session()).await;
        assert_eq!(code, EXIT_FAILED);
        assert!(session.calls().is_empty());
    }

    #[tokio::test]
    async fn second_instance_refuses_to_run_sessions() {
        let env = test_env();
        let held = InstanceLock::try_acquire(&env.paths.state_dir)
            .unwrap()
            .unwrap();
        let (code, session) = run_with(&env, &["card", "EX-1"], no_session()).await;
        assert_eq!(code, EXIT_FAILED);
        let args = ["heartbeat", "--once"];
        let (hb_code, hb_session) = run_with(&env, &args, no_session()).await;
        assert_eq!(hb_code, EXIT_FAILED);
        assert!(session.calls().is_empty() && hb_session.calls().is_empty());
        // Commands that run no sessions are not blocked.
        assert_eq!(run_with(&env, &["stop"], no_session()).await.0, EXIT_OK);
        drop(held);
        let done = FakeSession::sequence(vec![ok(card_output("EX-1", "done"))]);
        run_with(&env, &["resume"], no_session()).await;
        assert_eq!(run_with(&env, &["card", "EX-1"], done).await.0, EXIT_OK);
    }

    #[tokio::test]
    async fn heartbeat_once() {
        let env = test_env();
        let session = FakeSession::sequence(vec![ok(heartbeat_output(&[]))]);
        let args = ["heartbeat", "--once", "--interval", "1m"];
        let (code, session) = run_with(&env, &args, session).await;
        assert_eq!(code, EXIT_OK);
        assert_eq!(session.calls().len(), 1);
    }

    #[tokio::test]
    async fn bad_interval_is_rejected() {
        let env = test_env();
        let code = run_with(&env, &["heartbeat", "--interval", "soon"], no_session())
            .await
            .0;
        assert_eq!(code, EXIT_BAD_CONFIG);
    }
}
