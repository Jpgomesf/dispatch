//! `[intake]`: event sources that start triage. Personal scope (assignee = me) is not
//! configurable: these values only narrow the queries further.

use std::time::Duration;

use serde::Deserialize;

use super::duration_from_str;

pub const LINEAR_API_URL: &str = "https://api.linear.app/graphql";
pub const SLACK_APP_ID: &str = "com.tinyspeck.slackmacgap";

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct IntakeConfig {
    /// Collect events this long before one triage session.
    #[serde(deserialize_with = "duration_from_str")]
    pub batch_window: Duration,
    /// Sender allowlist (display names / emails, case-insensitive); empty = no filter.
    /// Never applies to mentions of me or to work events.
    pub allow_senders: Vec<String>,
    /// How I appear in notification text (e.g. "@Example User"); a notification containing
    /// one counts as a mention of me.
    pub mention_names: Vec<String>,
    pub notifications: NotificationsConfig,
    pub linear: LinearConfig,
    pub jira: JiraConfig,
}

impl Default for IntakeConfig {
    fn default() -> Self {
        Self {
            batch_window: Duration::from_secs(60),
            allow_senders: Vec::new(),
            mention_names: Vec::new(),
            notifications: NotificationsConfig::default(),
            linear: LinearConfig::default(),
            jira: JiraConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NotificationsConfig {
    pub enabled: bool,
    #[serde(deserialize_with = "duration_from_str")]
    pub poll: Duration,
    pub apps: Vec<String>,
    /// Substrings of title/subtitle routing a notification to this runner; empty = all.
    #[serde(rename = "match")]
    pub match_: Vec<String>,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            poll: Duration::from_secs(5),
            apps: vec![SLACK_APP_ID.into()],
            match_: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LinearConfig {
    pub enabled: bool,
    /// Name of the variable holding the personal API key, never the key.
    pub api_key_env: String,
    #[serde(deserialize_with = "duration_from_str")]
    pub poll: Duration,
    pub projects: Vec<String>,
    /// Team keys (`EX`) or names.
    pub teams: Vec<String>,
    pub labels: Vec<String>,
    /// GraphQL endpoint; not configurable (tests point it at a local server).
    #[serde(skip)]
    pub api_url: String,
}

impl Default for LinearConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key_env: "LINEAR_API_KEY".into(),
            poll: Duration::from_secs(60),
            projects: Vec::new(),
            teams: Vec::new(),
            labels: Vec::new(),
            api_url: LINEAR_API_URL.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct JiraConfig {
    pub enabled: bool,
    pub base_url: String,
    pub email_env: String,
    pub token_env: String,
    #[serde(deserialize_with = "duration_from_str")]
    pub poll: Duration,
    /// Narrowing only: the runner always runs `assignee = currentUser() AND (<jql>)`.
    pub jql: String,
}

impl Default for JiraConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: String::new(),
            email_env: "JIRA_EMAIL".into(),
            token_env: "JIRA_API_TOKEN".into(),
            poll: Duration::from_secs(60),
            jql: String::new(),
        }
    }
}

impl IntakeConfig {
    pub(super) fn validate(&self) -> Result<(), String> {
        if self.jira.enabled && !self.jira.base_url.starts_with("https://") {
            return Err("intake.jira.base_url must be an https:// URL when enabled".into());
        }
        crate::intake::jira::validate_narrowing(&self.jira.jql)
            .map_err(|e| format!("intake.jira.jql: {e}"))
    }
}
