//! `secrets.env`: `KEY=value` lines outside the repo, re-read on every poll so keys can be
//! added or rotated while the runner runs. A real environment variable wins over the file.
//! Values are never printed, logged or put in a session context.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::expand_user;

pub const SECRETS_ENV: &str = "DISPATCH_SECRETS";
pub const DEFAULT_SECRETS: &str = "~/.config/dispatch/secrets.env";

/// Environment lookup, injectable so tests never touch the process environment.
pub type EnvLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

#[must_use]
pub fn process_env() -> EnvLookup {
    Arc::new(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
}

/// `$DISPATCH_SECRETS`, else `~/.config/dispatch/secrets.env`.
#[must_use]
pub fn secrets_path(env_value: Option<&str>) -> PathBuf {
    match env_value {
        Some(path) if !path.is_empty() => expand_user(Path::new(path)),
        _ => expand_user(Path::new(DEFAULT_SECRETS)),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileStatus {
    Missing,
    Loaded,
    /// Unusable (group/world accessible, unreadable or malformed); the reason never
    /// contains a value.
    Refused(String),
}

#[derive(Clone)]
pub struct Secrets {
    file: BTreeMap<String, String>,
    pub status: FileStatus,
    env: EnvLookup,
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secrets")
            .field("keys", &self.file.keys().collect::<Vec<_>>())
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl Secrets {
    /// Read the file now. A refused file contributes no keys; the environment still does.
    pub fn load(path: &Path, env: EnvLookup) -> Secrets {
        let (file, status) = match read_file(path) {
            Ok(Some(file)) => (file, FileStatus::Loaded),
            Ok(None) => (BTreeMap::new(), FileStatus::Missing),
            Err(reason) => (BTreeMap::new(), FileStatus::Refused(reason)),
        };
        Secrets { file, status, env }
    }

    /// The environment variable, else the file's value; empty values count as missing.
    pub fn get(&self, name: &str) -> Option<String> {
        (self.env)(name)
            .or_else(|| self.file.get(name).cloned())
            .filter(|value| !value.is_empty())
    }

    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// Why `name` is unavailable, for a one-line status (no values).
    pub fn missing_reason(&self, name: &str) -> String {
        match &self.status {
            FileStatus::Refused(reason) => {
                format!("{name} missing (secrets file refused: {reason})")
            }
            _ => format!("{name} missing"),
        }
    }
}

fn read_file(path: &Path) -> Result<Option<BTreeMap<String, String>>, String> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {}", path.display(), e.kind())),
    };
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "{} has mode {mode:04o}; run chmod 600 on it",
            path.display()
        ));
    }
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e.kind()))?;
    parse(&text).map(Some)
}

/// `KEY=value` lines; `#` comments and blank lines are skipped, `export ` and matching
/// surrounding quotes are stripped. Errors name the line, never its content.
pub fn parse(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut values = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("line {}: expected KEY=value", index + 1));
        };
        let key = key.trim();
        let valid_key =
            !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !valid_key {
            return Err(format!("line {}: invalid key", index + 1));
        }
        values.insert(key.to_string(), unquote(value.trim()).to_string());
    }
    Ok(values)
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|v| v.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn no_env() -> EnvLookup {
        Arc::new(|_| None)
    }

    pub(crate) fn write_secrets(path: &Path, text: &str, mode: u32) {
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn parses_lines_comments_and_quotes() {
        let text = "# comment\n\nLINEAR_API_KEY=lin_api_example\nexport JIRA_EMAIL = \"dev@example.com\"\nJIRA_API_TOKEN='tok=en'\n";
        let values = parse(text).unwrap();
        assert_eq!(values["LINEAR_API_KEY"], "lin_api_example");
        assert_eq!(values["JIRA_EMAIL"], "dev@example.com");
        assert_eq!(values["JIRA_API_TOKEN"], "tok=en");
    }

    #[test]
    fn parse_errors_never_echo_the_line() {
        for text in [
            "just-a-secret-value",
            "BAD KEY=secret-value",
            "=secret-value",
        ] {
            let error = parse(text).unwrap_err();
            assert!(error.starts_with("line 1:"), "{error}");
            assert!(!error.contains("secret-value"), "{error}");
        }
    }

    #[test]
    fn file_must_not_be_group_or_world_accessible() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.env");
        write_secrets(&path, "LINEAR_API_KEY=lin_api_example\n", 0o644);
        let refused = Secrets::load(&path, no_env());
        assert!(matches!(&refused.status, FileStatus::Refused(r) if r.contains("0644")));
        assert!(!refused.has("LINEAR_API_KEY"));
        assert!(refused.missing_reason("LINEAR_API_KEY").contains("refused"));

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let loaded = Secrets::load(&path, no_env());
        assert_eq!(loaded.status, FileStatus::Loaded);
        assert_eq!(
            loaded.get("LINEAR_API_KEY").as_deref(),
            Some("lin_api_example")
        );
        assert!(
            !format!("{loaded:?}").contains("lin_api_example"),
            "Debug hides values"
        );
    }

    #[test]
    fn environment_wins_and_missing_file_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.env");
        let env: EnvLookup =
            Arc::new(|name| (name == "LINEAR_API_KEY").then(|| "from-env".to_string()));
        let missing = Secrets::load(&path, env.clone());
        assert_eq!(missing.status, FileStatus::Missing);
        assert_eq!(missing.get("LINEAR_API_KEY").as_deref(), Some("from-env"));

        write_secrets(&path, "LINEAR_API_KEY=from-file\nJIRA_EMAIL=\n", 0o600);
        let both = Secrets::load(&path, env);
        assert_eq!(both.get("LINEAR_API_KEY").as_deref(), Some("from-env"));
        assert!(!both.has("JIRA_EMAIL"), "empty counts as missing");
    }

    #[test]
    fn reread_picks_up_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.env");
        assert!(!Secrets::load(&path, no_env()).has("LINEAR_API_KEY"));
        write_secrets(&path, "LINEAR_API_KEY=first\n", 0o600);
        assert_eq!(
            Secrets::load(&path, no_env())
                .get("LINEAR_API_KEY")
                .as_deref(),
            Some("first")
        );
        write_secrets(&path, "LINEAR_API_KEY=second\n", 0o600);
        assert_eq!(
            Secrets::load(&path, no_env())
                .get("LINEAR_API_KEY")
                .as_deref(),
            Some("second")
        );
    }

    #[test]
    fn secrets_path_override() {
        assert_eq!(
            secrets_path(Some("/tmp/example/s.env")),
            PathBuf::from("/tmp/example/s.env")
        );
        assert_eq!(secrets_path(None), expand_user(Path::new(DEFAULT_SECRETS)));
        assert_eq!(
            secrets_path(Some("")),
            expand_user(Path::new(DEFAULT_SECRETS))
        );
    }
}
