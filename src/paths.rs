use std::path::{Path, PathBuf};

use crate::config::{Config, DEFAULT_CONFIG_DIR, expand_user};

pub const CONFIG_ENV: &str = "HARNESS_CONFIG";
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

#[derive(Debug, Clone, PartialEq)]
pub struct Paths {
    pub config: PathBuf,
    pub state_dir: PathBuf,
    pub outreach_file: PathBuf,
    pub plugin_dir: PathBuf,
}

impl Paths {
    pub fn resolve(config_path: &Path, config: &Config) -> Paths {
        Paths {
            config: config_path.to_path_buf(),
            state_dir: config.state_dir.clone(),
            outreach_file: config.outreach_file.clone(),
            plugin_dir: config
                .plugin_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from(REPO_PLUGIN_DIR)),
        }
    }

    pub fn state_file(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }

    pub fn kill_switch(&self) -> PathBuf {
        self.state_dir.join("STOP")
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
        let resolved = Paths::resolve(Path::new("c.toml"), &Config::default());
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
        let config = Config::from_toml("plugin_dir = \"/opt/example/plugin\"").unwrap();
        let resolved = Paths::resolve(Path::new("c.toml"), &config);
        assert_eq!(resolved.plugin_dir, PathBuf::from("/opt/example/plugin"));
    }

    #[test]
    fn derived_paths() {
        let config = Config::from_toml("state_dir = \"/var/example\"").unwrap();
        let paths = Paths::resolve(Path::new("c.toml"), &config);
        assert_eq!(paths.state_file(), PathBuf::from("/var/example/state.json"));
        assert_eq!(paths.kill_switch(), PathBuf::from("/var/example/STOP"));
        assert_eq!(
            paths.worktrees_dir(),
            PathBuf::from("/var/example/worktrees")
        );
    }
}
