use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};

use crate::durations::parse_duration;

pub const DEFAULT_CONFIG_DIR: &str = "~/.config/claude-harness";
pub const DEFAULT_STATE_DIR: &str = "~/.local/state/claude-harness";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HeartbeatConfig {
    #[serde(deserialize_with = "duration_from_str")]
    pub interval: Duration,
    pub model: String,
    pub effort: Effort,
    pub max_budget_usd: f64,
    pub max_cards_per_tick: u32,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(600),
            model: "sonnet".into(),
            effort: Effort::Medium,
            max_budget_usd: 1.0,
            max_cards_per_tick: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CardConfig {
    pub model: String,
    pub effort: Effort,
    pub max_budget_usd: f64,
    /// Card sessions running at the same time under `heartbeat`.
    pub max_parallel: u32,
}

impl Default for CardConfig {
    fn default() -> Self {
        Self {
            model: "claude-opus-5-5".into(),
            effort: Effort::High,
            max_budget_usd: 20.0,
            max_parallel: 2,
        }
    }
}

/// Free-form: the workflow skill interprets these values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourcesConfig {
    pub slack_channels: Vec<String>,
    pub tracker: String,
    pub tracker_query: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Default for SourcesConfig {
    fn default() -> Self {
        Self {
            slack_channels: Vec::new(),
            tracker: "linear".into(),
            tracker_query: String::new(),
            extra: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    pub name: String,
    pub path: PathBuf,
    #[serde(default, rename = "match")]
    pub match_: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub outreach_file: PathBuf,
    pub state_dir: PathBuf,
    pub plugin_dir: Option<PathBuf>,
    pub heartbeat: HeartbeatConfig,
    pub card: CardConfig,
    pub sources: SourcesConfig,
    pub workspaces: Vec<Workspace>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            outreach_file: expand_user(&Path::new(DEFAULT_CONFIG_DIR).join("outreach.md")),
            state_dir: expand_user(Path::new(DEFAULT_STATE_DIR)),
            plugin_dir: None,
            heartbeat: HeartbeatConfig::default(),
            card: CardConfig::default(),
            sources: SourcesConfig::default(),
            workspaces: Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
#[error("no workspace named {0:?} in config")]
pub struct UnknownWorkspace(pub String);

impl Config {
    pub fn from_toml(text: &str) -> Result<Config, String> {
        let config: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        config.validated()
    }

    fn validated(mut self) -> Result<Config, String> {
        if self.heartbeat.max_budget_usd <= 0.0 {
            return Err("heartbeat.max_budget_usd must be > 0".into());
        }
        if self.card.max_budget_usd <= 0.0 {
            return Err("card.max_budget_usd must be > 0".into());
        }
        if self.card.max_parallel == 0 {
            return Err("card.max_parallel must be >= 1".into());
        }
        self.outreach_file = expand_user(&self.outreach_file);
        self.state_dir = expand_user(&self.state_dir);
        self.plugin_dir = self.plugin_dir.as_deref().map(expand_user);
        for workspace in &mut self.workspaces {
            workspace.path = expand_user(&workspace.path);
        }
        Ok(self)
    }

    pub fn workspace_named(&self, name: &str) -> Result<&Workspace, UnknownWorkspace> {
        self.workspaces
            .iter()
            .find(|w| w.name == name)
            .ok_or_else(|| UnknownWorkspace(name.to_string()))
    }

    pub fn workspace_for(&self, card_ref: &str) -> Option<&Workspace> {
        self.workspaces.iter().find(|w| {
            w.match_
                .iter()
                .any(|pattern| card_ref.contains(pattern.as_str()))
        })
    }
}

pub fn load_config(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    Config::from_toml(&text)
}

/// Expand a leading `~` to `$HOME`.
pub fn expand_user(path: &Path) -> PathBuf {
    let Ok(rest) = path.strip_prefix("~") else {
        return path.to_path_buf();
    };
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(rest),
        None => path.to_path_buf(),
    }
}

fn duration_from_str<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
    let text = String::deserialize(deserializer)?;
    parse_duration(&text).map_err(serde::de::Error::custom)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn config_toml(root: &Path) -> String {
        let root = root.display();
        format!(
            r#"
outreach_file = "{root}/outreach.md"
state_dir = "{root}/state"
plugin_dir = "{root}/plugin"

[heartbeat]
interval = "10m"
max_cards_per_tick = 2

[sources]
slack_channels = ["C0000000001"]
tracker = "linear"
tracker_query = "assignee:me label:agent"

[[workspaces]]
name = "example-app"
path = "{root}/code/example-app"
match = ["EX-"]
"#
        )
    }

    #[test]
    fn loads_full_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::from_toml(&config_toml(dir.path())).unwrap();
        assert_eq!(config.heartbeat.interval, Duration::from_secs(600));
        assert_eq!(config.heartbeat.max_cards_per_tick, 2);
        assert_eq!(config.card.model, "claude-opus-5-5");
        assert_eq!(config.card.effort, Effort::High);
        assert_eq!(config.card.max_parallel, 2);
        assert_eq!(
            config.workspaces[0].path,
            dir.path().join("code/example-app")
        );
        assert_eq!(config.workspaces[0].match_, vec!["EX-"]);
    }

    #[test]
    fn defaults_match_spec() {
        let config = Config::default();
        assert_eq!(config.heartbeat.model, "sonnet");
        assert_eq!(config.heartbeat.effort, Effort::Medium);
        assert_eq!(config.heartbeat.max_budget_usd, 1.0);
        assert_eq!(config.heartbeat.max_cards_per_tick, 1);
        assert_eq!(config.card.model, "claude-opus-5-5");
        assert_eq!(config.card.effort, Effort::High);
        assert_eq!(config.card.max_budget_usd, 20.0);
        assert_eq!(config.card.max_parallel, 2);
        assert_eq!(
            config.state_dir,
            expand_user(Path::new("~/.local/state/claude-harness"))
        );
        assert_eq!(
            config.outreach_file,
            expand_user(Path::new("~/.config/claude-harness/outreach.md"))
        );
        assert!(!config.state_dir.starts_with("~"));
        assert_eq!(Config::from_toml("").unwrap(), config);
    }

    #[test]
    fn sources_accept_free_form_keys() {
        let config = Config::from_toml("[sources]\njira_board = \"EX\"\n").unwrap();
        assert_eq!(config.sources.extra["jira_board"], "EX");
        let dumped = serde_json::to_value(&config.sources).unwrap();
        assert_eq!(dumped["jira_board"], "EX");
        assert_eq!(dumped["tracker"], "linear");
    }

    #[test]
    fn rejects_invalid_config() {
        for raw in [
            "unknown_key = 1",
            "[heartbeat]\ninterval = \"soon\"",
            "[heartbeat]\neffort = \"extreme\"",
            "[card]\nmax_budget_usd = 0",
            "[card]\nmax_parallel = 0",
            "[heartbeat]\nmax_cards_per_tick = -1",
            "[send]\nmode = \"all\"",
            "[[workspaces]]\nname = \"x\"",
        ] {
            assert!(Config::from_toml(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn example_config_is_valid() {
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/config.example.toml");
        let config = load_config(&example).unwrap();
        assert_eq!(config.card.max_parallel, 2);
        assert_eq!(config.workspaces[0].name, "example-app");
    }

    #[test]
    fn workspace_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::from_toml(&config_toml(dir.path())).unwrap();
        assert!(config.workspace_for("EX-12").is_some());
        assert!(config.workspace_for("OTHER-1").is_none());
        assert_eq!(
            config.workspace_named("example-app").unwrap().name,
            "example-app"
        );
        let error = config.workspace_named("missing").unwrap_err();
        assert!(error.to_string().contains("no workspace"));
    }

    #[test]
    fn expands_home() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(expand_user(Path::new("~/code")), home.join("code"));
        assert_eq!(expand_user(Path::new("/abs/~x")), PathBuf::from("/abs/~x"));
    }
}
