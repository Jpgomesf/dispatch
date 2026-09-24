use std::path::{Path, PathBuf};

use crate::config::{Config, DEFAULT_CONFIG_DIR, expand_user};
use crate::intake::notifications;
use crate::intake::secrets::{SECRETS_ENV, secrets_path};
use crate::store::{DB_ENV, DEFAULT_DB};

pub const CONFIG_ENV: &str = "HARNESS_CONFIG";

/// Environment variables that move files: `HARNESS_CONFIG`, `HARNESS_DB`, `HARNESS_SECRETS`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvPaths {
    pub config: Option<String>,
    pub db: Option<String>,
    pub secrets: Option<String>,
}

impl EnvPaths {
    #[must_use]
    pub fn from_process() -> EnvPaths {
        EnvPaths {
            config: std::env::var(CONFIG_ENV).ok(),
            db: std::env::var(DB_ENV).ok(),
            secrets: std::env::var(SECRETS_ENV).ok(),
        }
    }
}

/// `$HARNESS_DB`, else `~/.local/state/claude-harness/harness.db` (machine-wide, shared by
/// every runner whatever its `state_dir`).
#[must_use]
pub fn db_path(env_value: Option<&str>) -> PathBuf {
    match env_value {
        Some(path) if !path.is_empty() => expand_user(Path::new(path)),
        _ => expand_user(Path::new(DEFAULT_DB)),
    }
}
/// The repo's own plugin; `cargo install --path .` bakes in the checkout it was built from.
pub const REPO_PLUGIN_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/plugin");

/// `--config`, else `$HARNESS_CONFIG`, else `~/.config/claude-harness/config.toml`.
pub fn resolve_config_path(cli_value: Option<&Path>, env_value: Option<&str>) -> PathBuf {
    let raw = match (cli_value, env_value) {
        (Some(cli), _) if !cli.as_os_str().is_empty() => cli.to_path_buf(),
        (_, Some(env)) if !env.is_empty() => PathBuf::from(env),
        _ => Path::new(DEFAULT_CONFIG_DIR).join("config.toml"),
    };
    expand_user(&raw)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config: PathBuf,
    /// Runner name (config `name`): names the instance lock.
    pub runner: String,
    pub state_dir: PathBuf,
    pub outreach_file: PathBuf,
    pub plugin_dir: PathBuf,
    pub db: PathBuf,
    pub secrets: PathBuf,
    pub notifications_db: PathBuf,
}

impl Paths {
    pub fn resolve(config_path: &Path, config: &Config, env: &EnvPaths) -> Paths {
        Paths {
            config: config_path.to_path_buf(),
            runner: config.name.clone(),
            db: db_path(env.db.as_deref()),
            secrets: secrets_path(env.secrets.as_deref()),
            notifications_db: notifications::default_db(),
            state_dir: config.state_dir.clone(),
            outreach_file: config.outreach_file.clone(),
            plugin_dir: config
                .plugin_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from(REPO_PLUGIN_DIR)),
        }
    }

    /// Phase 1 state, imported into `harness.db` once and renamed `state.json.migrated`.
    pub fn legacy_state_file(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }

    pub fn kill_switch(&self) -> PathBuf {
        self.state_dir.join("STOP")
    }

    /// One live session-running process per runner name.
    pub fn instance_lock_file(&self) -> PathBuf {
        self.state_dir.join(format!("{}.lock", self.runner))
    }

    /// Per-card git worktrees live under the state dir, never inside the workspace.
    pub fn worktrees_dir(&self) -> PathBuf {
        self.state_dir.join("worktrees")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_path_precedence() {
        let env = "/tmp/example/env.toml";
        assert_eq!(
            resolve_config_path(Some(Path::new("cli.toml")), Some(env)),
            PathBuf::from("cli.toml")
        );
        assert_eq!(resolve_config_path(None, Some(env)), PathBuf::from(env));
        let default = expand_user(Path::new("~/.config/claude-harness/config.toml"));
        assert_eq!(resolve_config_path(None, None), default);
        assert_eq!(resolve_config_path(None, Some("")), default);
    }

    #[test]
    fn plugin_dir_defaults_to_repo_plugin() {
        let resolved = Paths::resolve(
            Path::new("c.toml"),
            &Config::default(),
            &EnvPaths::default(),
        );
        assert_eq!(resolved.plugin_dir, PathBuf::from(REPO_PLUGIN_DIR));
        assert!(
            resolved
                .plugin_dir
                .join(".claude-plugin/plugin.json")
                .is_file()
        );
    }

    #[test]
    fn plugin_dir_override() {
        let config =
            Config::from_toml("name = \"ex\"\nplugin_dir = \"/opt/example/plugin\"").unwrap();
        let resolved = Paths::resolve(Path::new("c.toml"), &config, &EnvPaths::default());
        assert_eq!(resolved.plugin_dir, PathBuf::from("/opt/example/plugin"));
    }

    #[test]
    fn derived_paths() {
        let config = Config::from_toml("name = \"ex\"\nstate_dir = \"/var/example\"").unwrap();
        let paths = Paths::resolve(Path::new("c.toml"), &config, &EnvPaths::default());
        assert_eq!(
            paths.legacy_state_file(),
            PathBuf::from("/var/example/state.json")
        );
        assert_eq!(paths.kill_switch(), PathBuf::from("/var/example/STOP"));
        assert_eq!(
            paths.instance_lock_file(),
            PathBuf::from("/var/example/ex.lock")
        );
        assert_eq!(paths.db, expand_user(Path::new(DEFAULT_DB)));
        assert_eq!(paths.secrets, secrets_path(None));
        let env = EnvPaths {
            db: Some("/tmp/example/h.db".into()),
            secrets: Some("/tmp/example/s.env".into()),
            ..EnvPaths::default()
        };
        let moved = Paths::resolve(Path::new("c.toml"), &config, &env);
        assert_eq!(moved.db, PathBuf::from("/tmp/example/h.db"));
        assert_eq!(moved.secrets, PathBuf::from("/tmp/example/s.env"));
        assert_eq!(
            paths.worktrees_dir(),
            PathBuf::from("/var/example/worktrees")
        );
    }
}
