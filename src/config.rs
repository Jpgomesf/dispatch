use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};

use crate::durations::{parse_duration, parse_duration_or_zero};

mod intake;

pub use intake::{
    IntakeConfig, JiraConfig, LINEAR_API_URL, LinearConfig, NotificationsConfig, SLACK_APP_ID,
};

pub const DEFAULT_CONFIG_DIR: &str = "~/.config/dispatch";
pub const DEFAULT_STATE_DIR: &str = "~/.local/state/dispatch";

/// What each session is asked to do; the JSON context block follows it. `{ref}` is replaced by
/// the card or discussion ref.
pub const TRIAGE_OBJECTIVE: &str = "Check the new activity below (or sweep your sources if \
`events` is empty) and decide what deserves attention: respond, draft, ignore, pick up \
assigned work as cards, or investigate mentions.";
pub const CARD_OBJECTIVE: &str = "Work card {ref} to completion in this workspace.";
pub const DISCUSSION_OBJECTIVE: &str = "You were mentioned in a discussion about {ref}. \
Investigate the question and respond in the thread.";

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

/// Triage sessions: started by a batch of intake events, or by the fallback sweep timer.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TriageConfig {
    /// Fallback sweep: a triage with no events this long after the previous sweep.
    #[serde(deserialize_with = "duration_from_str")]
    pub interval: Duration,
    pub model: String,
    pub effort: Effort,
    pub max_budget_usd: f64,
    pub max_cards_per_tick: u32,
    /// Wall-clock limit of one triage session.
    #[serde(deserialize_with = "duration_from_str")]
    pub timeout: Duration,
    pub objective: String,
}

impl Default for TriageConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(30 * 60),
            model: "sonnet".into(),
            effort: Effort::Medium,
            max_budget_usd: 1.0,
            max_cards_per_tick: 1,
            timeout: Duration::from_secs(20 * 60),
            objective: TRIAGE_OBJECTIVE.into(),
        }
    }
}

/// Card sessions; discussion sessions use the same model, effort and budget.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CardConfig {
    pub model: String,
    pub effort: Effort,
    pub max_budget_usd: f64,
    /// Card and discussion sessions running at the same time under `heartbeat`.
    pub max_parallel: u32,
    /// Wall-clock limit of one card attempt.
    #[serde(deserialize_with = "duration_from_str")]
    pub timeout: Duration,
    /// Counted attempts per card (and per discussion) before it needs a person.
    pub max_attempts: u32,
    /// `{ref}` is replaced by the card ref.
    pub objective: String,
}

impl Default for CardConfig {
    fn default() -> Self {
        Self {
            model: "claude-opus-5-5".into(),
            effort: Effort::High,
            max_budget_usd: 20.0,
            max_parallel: 2,
            timeout: Duration::from_secs(3 * 60 * 60),
            max_attempts: 3,
            objective: CARD_OBJECTIVE.into(),
        }
    }
}

/// Discussion sessions (model, effort and budget come from `[card]`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DiscussionConfig {
    /// Wall-clock limit of one discussion session.
    #[serde(deserialize_with = "duration_from_str")]
    pub timeout: Duration,
    /// `{ref}` is replaced by the discussion ref.
    pub objective: String,
}

impl Default for DiscussionConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60 * 60),
            objective: DISCUSSION_OBJECTIVE.into(),
        }
    }
}

/// Limits every session shares.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SessionsConfig {
    /// No stream event for this long means the session is stuck.
    #[serde(deserialize_with = "duration_from_str")]
    pub idle_timeout: Duration,
    /// Between two session starts, plus up to 50% random jitter; `0s` switches it off.
    #[serde(deserialize_with = "duration_or_zero_from_str")]
    pub start_stagger: Duration,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(15 * 60),
            start_stagger: Duration::from_secs(30),
        }
    }
}

/// Free-form: the workflow skill interprets these values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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
    /// Required; unique per machine (`[a-z0-9-]+`): claims, branches and labels carry it.
    pub name: String,
    pub outreach_file: PathBuf,
    pub state_dir: PathBuf,
    pub plugin_dir: Option<PathBuf>,
    pub triage: TriageConfig,
    pub card: CardConfig,
    pub discussion: DiscussionConfig,
    pub sessions: SessionsConfig,
    pub sources: SourcesConfig,
    pub intake: IntakeConfig,
    pub workspaces: Vec<Workspace>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: String::new(),
            outreach_file: expand_user(&Path::new(DEFAULT_CONFIG_DIR).join("outreach.md")),
            state_dir: expand_user(Path::new(DEFAULT_STATE_DIR)),
            plugin_dir: None,
            triage: TriageConfig::default(),
            card: CardConfig::default(),
            discussion: DiscussionConfig::default(),
            sessions: SessionsConfig::default(),
            sources: SourcesConfig::default(),
            intake: IntakeConfig::default(),
            workspaces: Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("no workspace named {0:?} in config")]
pub struct UnknownWorkspace(pub String);

impl Config {
    pub fn from_toml(text: &str) -> Result<Config, String> {
        let config: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        config.validated()
    }

    fn validated(mut self) -> Result<Config, String> {
        if !is_valid_name(&self.name) {
            return Err(format!(
                "name is required and must match [a-z0-9-]+ (got {:?})",
                self.name
            ));
        }
        if self.triage.max_budget_usd <= 0.0 {
            return Err("triage.max_budget_usd must be > 0".into());
        }
        if self.card.max_budget_usd <= 0.0 {
            return Err("card.max_budget_usd must be > 0".into());
        }
        if self.card.max_parallel == 0 {
            return Err("card.max_parallel must be >= 1".into());
        }
        if self.card.max_attempts == 0 {
            return Err("card.max_attempts must be >= 1".into());
        }
        for (key, objective) in [
            ("triage.objective", &self.triage.objective),
            ("card.objective", &self.card.objective),
            ("discussion.objective", &self.discussion.objective),
        ] {
            if objective.trim().is_empty() {
                return Err(format!("{key} must not be empty"));
            }
        }
        self.intake.validate()?;
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

/// Runner names go into claim keys, branch names and labels.
#[must_use]
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
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

fn duration_or_zero_from_str<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Duration, D::Error> {
    let text = String::deserialize(deserializer)?;
    parse_duration_or_zero(&text).map_err(serde::de::Error::custom)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn config_toml(root: &Path) -> String {
        let root = root.display();
        format!(
            r#"
name = "example-app"
outreach_file = "{root}/outreach.md"
state_dir = "{root}/state"
plugin_dir = "{root}/plugin"

[triage]
interval = "10m"
max_cards_per_tick = 2

[sessions]
start_stagger = "0s"

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
        assert_eq!(config.triage.interval, Duration::from_secs(600));
        assert_eq!(config.triage.max_cards_per_tick, 2);
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
        assert_eq!(config.triage.model, "sonnet");
        assert_eq!(config.triage.effort, Effort::Medium);
        assert_eq!(config.triage.max_budget_usd, 1.0);
        assert_eq!(config.triage.max_cards_per_tick, 1);
        assert_eq!(config.card.model, "claude-opus-5-5");
        assert_eq!(config.card.effort, Effort::High);
        assert_eq!(config.card.max_budget_usd, 20.0);
        assert_eq!(config.card.max_parallel, 2);
        assert_eq!(config.triage.objective, TRIAGE_OBJECTIVE);
        assert_eq!(
            config.card.objective,
            "Work card {ref} to completion in this workspace."
        );
        assert!(
            config
                .discussion
                .objective
                .contains("discussion about {ref}")
        );
        let minutes = |m: u64| Duration::from_secs(m * 60);
        assert_eq!(config.triage.timeout, minutes(20));
        assert_eq!(config.card.timeout, minutes(180));
        assert_eq!(config.discussion.timeout, minutes(60));
        assert_eq!(config.sessions.idle_timeout, minutes(15));
        assert_eq!(config.sessions.start_stagger, Duration::from_secs(30));
        assert_eq!(config.card.max_attempts, 3);
        assert_eq!(
            config.state_dir,
            expand_user(Path::new("~/.local/state/dispatch"))
        );
        assert_eq!(
            config.outreach_file,
            expand_user(Path::new("~/.config/dispatch/outreach.md"))
        );
        assert!(!config.state_dir.starts_with("~"));
        assert_eq!(config.triage.interval, Duration::from_secs(30 * 60));
        assert_eq!(config.intake.batch_window, Duration::from_secs(60));
        assert!(!config.intake.notifications.enabled);
        assert_eq!(config.intake.notifications.apps, [SLACK_APP_ID]);
        assert_eq!(config.intake.linear.api_key_env, "LINEAR_API_KEY");
        assert_eq!(config.intake.jira.token_env, "JIRA_API_TOKEN");
        let minimal = Config::from_toml("name = \"example-app\"").unwrap();
        assert_eq!(
            minimal,
            Config {
                name: "example-app".into(),
                ..config
            }
        );
    }

    #[test]
    fn sources_accept_free_form_keys() {
        let config = Config::from_toml("name = \"ex\"\n[sources]\njira_board = \"EX\"\n").unwrap();
        assert_eq!(config.sources.extra["jira_board"], "EX");
        let dumped = serde_json::to_value(&config.sources).unwrap();
        assert_eq!(dumped["jira_board"], "EX");
        assert_eq!(dumped["tracker"], "linear");
    }

    #[test]
    fn rejects_invalid_config() {
        for body in [
            "unknown_key = 1",
            "[triage]\ninterval = \"soon\"",
            "[triage]\neffort = \"extreme\"",
            "[card]\nmax_budget_usd = 0",
            "[card]\nmax_parallel = 0",
            "[triage]\nmax_cards_per_tick = -1",
            "[send]\nmode = \"all\"",
            "[[workspaces]]\nname = \"x\"",
            "[heartbeat]\ninterval = \"10m\"",
            "[intake]\nunknown = 1",
            "[intake.linear]\napi_key = \"lin_api_example\"",
            "[intake.linear]\napi_url = \"https://example.com\"",
            "[intake.jira]\nenabled = true\nbase_url = \"http://example.atlassian.net\"",
            "[intake.jira]\njql = \"project = EX) OR (project = OTHER\"",
            "[intake.notifications]\npoll = \"often\"",
            "[card]\nobjective = \"  \"",
            "[discussion]\nobjective = \"\"",
            "[discussion]\nmodel = \"sonnet\"",
            "[discussion]\ntimeout = \"soon\"",
            "[card]\ntimeout = \"0s\"",
            "[sessions]\nidle_timeout = \"-1m\"",
            "[sessions]\nunknown = 1",
            "[card]\nmax_attempts = 0",
        ] {
            let raw = format!("name = \"example-app\"\n{body}");
            assert!(Config::from_toml(&raw).is_err(), "{raw}");
        }
        for name in ["", "Example", "ex_app", "ex app", "ex/app"] {
            let raw = format!("name = {name:?}");
            assert!(Config::from_toml(&raw).is_err(), "{raw}");
        }
        assert!(Config::from_toml("").is_err(), "name is required");
    }

    #[test]
    fn example_config_is_valid() {
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/config.example.toml");
        let config = load_config(&example).unwrap();
        assert!(is_valid_name(&config.name));
        assert_eq!(config.card.max_parallel, 2);
        assert_eq!(config.workspaces[0].name, "example-app");
    }

    #[test]
    fn loads_intake_sections() {
        let raw = r##"
name = "example-app"

[intake]
batch_window = "30s"
allow_senders = ["Example Person"]

[intake.notifications]
enabled = true
poll = "5s"
match = ["#example-channel"]

[intake.linear]
enabled = true
projects = ["Example App"]

[intake.jira]
enabled = true
base_url = "https://example.atlassian.net"
jql = "project = EX"
"##;
        let config = Config::from_toml(raw).unwrap();
        let intake = &config.intake;
        assert_eq!(intake.batch_window, Duration::from_secs(30));
        assert_eq!(intake.notifications.match_, ["#example-channel"]);
        assert_eq!(intake.notifications.apps, [SLACK_APP_ID]);
        assert_eq!(intake.linear.projects, ["Example App"]);
        assert_eq!(intake.linear.api_url, LINEAR_API_URL);
        assert_eq!(intake.jira.jql, "project = EX");
    }

    #[test]
    fn session_limits_are_configurable() {
        let raw = "name = \"ex\"\n[triage]\ntimeout = \"5m\"\n[card]\ntimeout = \"2h\"\n\
                   [discussion]\ntimeout = \"30m\"\n[sessions]\nidle_timeout = \"10m\"\nstart_stagger = \"45s\"\n";
        let config = Config::from_toml(raw).unwrap();
        assert_eq!(config.triage.timeout, Duration::from_secs(300));
        assert_eq!(config.card.timeout, Duration::from_secs(7200));
        assert_eq!(config.discussion.timeout, Duration::from_secs(1800));
        assert_eq!(config.sessions.idle_timeout, Duration::from_secs(600));
        assert_eq!(config.sessions.start_stagger, Duration::from_secs(45));
    }

    #[test]
    fn objectives_are_configurable() {
        let raw = "name = \"ex\"\n[triage]\nobjective = \"Look around.\"\n\
                   [card]\nobjective = \"Finish {ref}.\"\n\
                   [discussion]\nobjective = \"Answer about {ref}.\"\n";
        let config = Config::from_toml(raw).unwrap();
        assert_eq!(config.triage.objective, "Look around.");
        assert_eq!(config.card.objective, "Finish {ref}.");
        assert_eq!(config.discussion.objective, "Answer about {ref}.");
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
