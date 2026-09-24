//! One git worktree per card, so parallel cards in the same workspace never share a checkout.

use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::config::Workspace;

/// Directory name for a ref or workspace name: readable part plus a stable hash of the
/// original, so names that normalise alike (`EX-1`, `ex_1`, `EX/1`) never share a directory.
/// `EX-123` → `ex-123-<8 hex>`; anything outside `[a-z0-9]` becomes `-`, and the readable
/// part is capped at 48 characters (discussion keys can be long URLs).
#[must_use]
pub fn slug(name: &str) -> String {
    let lowered: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(48)
        .collect();
    let trimmed = lowered.trim_matches('-');
    let readable = if trimmed.is_empty() { "card" } else { trimmed };
    format!("{readable}-{:08x}", fnv1a(name.as_bytes()) >> 32)
}

/// 64-bit FNV-1a: tiny, dependency-free and stable across Rust versions (unlike `DefaultHasher`).
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("git: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("git {}: {}", args.join(" "), stderr.trim()))
    }
}

/// The workspace path is the root of a git checkout (not merely somewhere inside one).
async fn is_checkout_root(path: &Path) -> bool {
    if !path.is_dir() {
        return false;
    }
    let Ok(top) = git(path, &["rev-parse", "--show-toplevel"]).await else {
        return false;
    };
    match (std::fs::canonicalize(&top), std::fs::canonicalize(path)) {
        (Ok(top), Ok(path)) => top == path,
        _ => false,
    }
}

/// Where a card (or a discussion, named by its claim key) runs:
/// `<worktrees_dir>/<workspace>/<name-slug>`, a detached worktree of the workspace, reused
/// when it already exists (resume). A workspace that is not a git checkout
/// is returned as is; the workflow skill reports it as blocked.
pub async fn card_checkout(
    workspace: &Workspace,
    card_ref: &str,
    worktrees_dir: &Path,
) -> Result<PathBuf, String> {
    if !is_checkout_root(&workspace.path).await {
        return Ok(workspace.path.clone());
    }
    let checkout = worktrees_dir
        .join(slug(&workspace.name))
        .join(slug(card_ref));
    if checkout.is_dir() {
        return Ok(checkout);
    }
    if let Some(parent) = checkout.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    // Clears registrations of worktrees whose directory is gone; never touches existing ones.
    git(&workspace.path, &["worktree", "prune"]).await?;
    let target = checkout.display().to_string();
    git(&workspace.path, &["worktree", "add", "--detach", &target]).await?;
    Ok(checkout)
}

/// The commit `checkout` has checked out, when it is one of our worktrees (not a workspace
/// used as is, which may sit inside someone else's repository).
pub async fn head(workspace: &Workspace, checkout: &Path) -> Option<String> {
    if checkout == workspace.path {
        return None;
    }
    git(checkout, &["rev-parse", "--verify", "HEAD"]).await.ok()
}

/// Commits reachable from `after` but not from `before`: what an attempt added to HEAD.
pub async fn commits_between(checkout: &Path, before: &str, after: &str) -> Option<u32> {
    if before == after {
        return Some(0);
    }
    let range = format!("{before}..{after}");
    git(checkout, &["rev-list", "--count", &range])
        .await
        .ok()?
        .parse()
        .ok()
}

/// Remove a finished card's worktree. `git worktree remove` without `--force` refuses when the
/// tree has uncommitted or untracked changes, so nothing unsaved is lost; commits stay on
/// their branch.
pub async fn release_checkout(workspace: &Workspace, checkout: &Path) -> Result<(), String> {
    if checkout == workspace.path {
        return Ok(());
    }
    let target = checkout.display().to_string();
    git(&workspace.path, &["worktree", "remove", &target])
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{commit, init_repo};

    fn workspace(path: &Path) -> Workspace {
        Workspace {
            name: "Example App".into(),
            path: path.to_path_buf(),
            match_: vec![],
        }
    }

    #[test]
    fn slugs() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(
            fnv1a(b"a"),
            0xaf63_dc4c_8601_ec8c,
            "FNV-1a reference vector"
        );
        let ex = slug("EX-123");
        assert!(
            ex.starts_with("ex-123-") && ex.len() == "ex-123-".len() + 8,
            "{ex}"
        );
        assert_eq!(slug("EX-123"), ex, "stable");
        assert!(slug("org/repo#7").starts_with("org-repo-7-"));
        assert!(slug("--").starts_with("card-"));
        let long = slug("discussion:https://example.com/a/very/long/path/to/a/comment/12345");
        assert!(long.starts_with("discussion-https---example-com-a-very-long-path-"));
        assert!(long.len() <= 48 + 1 + 8, "{long}");
    }

    #[test]
    fn slugs_of_refs_that_normalise_alike_differ() {
        let names = ["EX-1", "ex-1", "EX_1", "EX/1", "EX 1"];
        let slugs: std::collections::HashSet<String> = names.iter().map(|n| slug(n)).collect();
        assert_eq!(slugs.len(), names.len(), "{slugs:?}");
    }

    #[tokio::test]
    async fn non_git_workspace_is_used_as_is() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let ws = workspace(&plain);
        let wt = dir.path().join("worktrees");
        assert_eq!(card_checkout(&ws, "EX-1", &wt).await.unwrap(), plain);
        let missing = workspace(&dir.path().join("missing"));
        assert_eq!(
            card_checkout(&missing, "EX-1", &wt).await.unwrap(),
            missing.path
        );
    }

    #[tokio::test]
    async fn subdirectory_of_a_repo_is_not_a_checkout() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let ws = workspace(&sub);
        assert_eq!(
            card_checkout(&ws, "EX-1", &dir.path().join("wt"))
                .await
                .unwrap(),
            sub
        );
    }

    #[tokio::test]
    async fn creates_reuses_and_releases_a_worktree_per_card() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        init_repo(&repo);
        let ws = workspace(&repo);
        let wt = dir.path().join("state/worktrees");

        let first = card_checkout(&ws, "EX-1", &wt).await.unwrap();
        let second = card_checkout(&ws, "EX-2", &wt).await.unwrap();
        assert_eq!(first, wt.join(slug("Example App")).join(slug("EX-1")));
        assert_ne!(first, second);
        assert!(first.join("README.md").is_file());
        assert_eq!(card_checkout(&ws, "EX-1", &wt).await.unwrap(), first);

        let before = head(&ws, &first).await.unwrap();
        assert_eq!(
            head(&ws, &repo).await,
            None,
            "the workspace itself is not ours"
        );
        commit(&first, "one");
        commit(&first, "two");
        let after = head(&ws, &first).await.unwrap();
        assert_eq!(commits_between(&first, &before, &after).await, Some(2));
        assert_eq!(commits_between(&first, &after, &after).await, Some(0));
        assert_eq!(commits_between(&first, "nonsense", &after).await, None);

        std::fs::write(first.join("scratch.txt"), "unsaved").unwrap();
        assert!(
            release_checkout(&ws, &first).await.is_err(),
            "dirty tree must be kept"
        );
        assert!(first.is_dir());
        release_checkout(&ws, &second).await.unwrap();
        assert!(!second.exists());
        assert!(release_checkout(&ws, &repo).await.is_ok());
    }
}
