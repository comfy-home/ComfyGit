// Copyright © 2026 ComfyHome™
// All rights reserved.
//
// Licensed under the ComfyGit License v1.2

use anyhow::{Result, bail};

use crate::forge::{ForgeKind, detect_forge_from_remote_url};

use super::{
    GitCancellation, current_branch_with_cancel, git_remote_names, resolve_push_remote_name,
    run_git_checked, run_git_checked_with_cancel, run_git_with_cancel,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorSyncReport {
    pub branch: String,
    pub head_line: String,
    pub gitlab_remote: String,
    pub github_remote: String,
    pub gitlab_tracking: bool,
    pub github_tracking: bool,
}

const ANSI_STATUS_IN_SYNC: &str = "\x1b[38;5;46m";
const ANSI_STATUS_OUT_OF_SYNC: &str = "\x1b[38;5;196m";
const ANSI_RESET: &str = "\x1b[0m";

impl MirrorSyncReport {
    pub fn in_sync(&self) -> bool {
        self.gitlab_tracking && self.github_tracking
    }

    pub fn status_colored(&self) -> String {
        if self.in_sync() {
            format!("{ANSI_STATUS_IN_SYNC}in sync{ANSI_RESET}")
        } else {
            format!("{ANSI_STATUS_OUT_OF_SYNC}out of sync{ANSI_RESET}")
        }
    }

    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines = vec![
            format!("Branch: {}", self.branch),
            format!("Status: {}", self.status_colored()),
            format!(
                "GitLab remote '{}': {}",
                self.gitlab_remote,
                if self.gitlab_tracking {
                    "tracking"
                } else {
                    "missing"
                }
            ),
            format!(
                "GitHub remote '{}': {}",
                self.github_remote,
                if self.github_tracking {
                    "tracking"
                } else {
                    "missing"
                }
            ),
            format!("git log --oneline -1: {}", self.head_line),
        ];
        if !self.in_sync() {
            lines.push(
                "Run 'cg sync' to push the current branch to both remotes when you choose to sync."
                    .to_string(),
            );
        }
        lines
    }
}

pub fn resolve_dual_remotes(
    repo_root: &str,
    configured_gitlab: Option<&str>,
    configured_github: Option<&str>,
) -> Result<(String, String)> {
    if let (Some(gitlab), Some(github)) = (configured_gitlab, configured_github) {
        let gitlab_name = resolve_push_remote_name(repo_root, gitlab.trim())?;
        let github_name = resolve_push_remote_name(repo_root, github.trim())?;
        return Ok((gitlab_name, github_name));
    }

    let mut gitlab_remote = None;
    let mut github_remote = None;
    for remote in git_remote_names(repo_root)? {
        let url = run_git_checked(repo_root, &["remote", "get-url", &remote])?;
        match detect_forge_from_remote_url(url.trim()) {
            Some(ForgeKind::GitLab) if gitlab_remote.is_none() || remote == "gitlab" => {
                gitlab_remote = Some(remote);
            }
            Some(ForgeKind::GitHub) if github_remote.is_none() || remote == "origin" => {
                github_remote = Some(remote);
            }
            _ => {}
        }
    }

    match (gitlab_remote, github_remote) {
        (Some(gitlab), Some(github)) => Ok((gitlab, github)),
        _ => bail!(
            "could not resolve GitLab and GitHub remotes; configure both remote URLs in the project or add 'gitlab' and 'origin' remotes"
        ),
    }
}

pub fn check_mirror_sync(
    repo_root: &str,
    configured_gitlab: Option<&str>,
    configured_github: Option<&str>,
) -> Result<MirrorSyncReport> {
    let (gitlab_remote, github_remote) =
        resolve_dual_remotes(repo_root, configured_gitlab, configured_github)?;
    let branch = current_branch_with_cancel(repo_root, None)?;
    if branch.starts_with("detached") {
        bail!("mirror sync check requires a checked-out branch, not a detached HEAD");
    }

    let output = run_git_with_cancel(repo_root, &["log", "--oneline", "-1", "--decorate"], None)?;
    if !output.success {
        bail!("failed to read latest commit for mirror sync check");
    }
    let head_line = output.stdout.trim().to_string();
    let gitlab_ref = format!("{gitlab_remote}/{branch}");
    let github_ref = format!("{github_remote}/{branch}");

    let gitlab_tracking = head_line.contains(&gitlab_ref);
    let github_tracking = head_line.contains(&github_ref);

    Ok(MirrorSyncReport {
        branch,
        head_line,
        gitlab_remote,
        github_remote,
        gitlab_tracking,
        github_tracking,
    })
}

pub fn push_mirror_sync(
    repo_root: &str,
    configured_gitlab: Option<&str>,
    configured_github: Option<&str>,
) -> Result<Vec<String>> {
    let (gitlab_remote, github_remote) =
        resolve_dual_remotes(repo_root, configured_gitlab, configured_github)?;
    let branch = current_branch_with_cancel(repo_root, None)?;
    if branch.starts_with("detached") {
        bail!("mirror sync requires a checked-out branch, not a detached HEAD");
    }

    // Every remote is attempted even when one fails (e.g. unreachable or a
    // non-fast-forward rejection): a single failing remote must not starve
    // the remaining pushes.
    let mut lines = Vec::new();
    let mut failures = Vec::new();
    for remote in [&gitlab_remote, &github_remote] {
        match run_git_with_cancel(repo_root, &["push", remote, &branch], None) {
            Ok(output) if output.success => {
                lines.push(format!("Pushed '{branch}' to remote '{remote}'."));
            }
            Ok(output) => {
                let combined = format!("{}{}", output.stdout, output.stderr);
                failures.push(format!("'{remote}': {}", combined.trim()));
            }
            Err(error) => {
                failures.push(format!("'{remote}': {error:#}"));
            }
        }
    }
    if !failures.is_empty() {
        let mut message = format!(
            "failed to push branch '{branch}' to {} remote(s): {}",
            failures.len(),
            failures.join(" | ")
        );
        if !lines.is_empty() {
            message.push_str(&format!(" (succeeded: {})", lines.join(" ")));
        }
        bail!("{message}");
    }
    Ok(lines)
}

/// Fetches only the remotes configured in ComfyGit for this repository.
///
/// `configured_remotes` may contain remote names or URLs (as stored in the
/// project config); each is resolved to a git remote name before fetching.
/// When no configured remote resolves, falls back to a plain `git fetch`,
/// which only touches the default/upstream remote. Extra remotes that are not
/// part of the ComfyGit configuration (e.g. a local VM mirror that may be
/// offline) are never fetched, so an unreachable one cannot abort the flow
/// the way `git fetch --all` does.
///
/// Returns the names of the remotes that were fetched (empty when the
/// default-remote fallback was used).
pub fn fetch_configured_remotes(
    repo_root: &str,
    configured_remotes: &[String],
    cancel: Option<GitCancellation>,
) -> Result<Vec<String>> {
    let mut fetched: Vec<String> = Vec::new();
    for spec in configured_remotes {
        let spec = spec.trim();
        if spec.is_empty() {
            continue;
        }
        let remote = match resolve_push_remote_name(repo_root, spec) {
            Ok(remote) => remote,
            Err(error) => {
                eprintln!("warning: could not resolve configured remote '{spec}': {error:#}");
                continue;
            }
        };
        if fetched.iter().any(|done| done == &remote) {
            continue;
        }
        let output = run_git_with_cancel(repo_root, &["fetch", &remote], cancel.clone())?;
        if !output.success {
            let details = output.stderr.trim();
            bail!("git [\"fetch\", \"{remote}\"] failed in {repo_root}: {details}");
        }
        // `git fetch` reports remote updates on stderr even on success.
        let stderr = output.stderr.trim();
        if !stderr.is_empty() {
            eprintln!("{stderr}");
        }
        fetched.push(remote);
    }

    if fetched.is_empty() {
        run_git_checked_with_cancel(repo_root, &["fetch"], cancel)?;
    }
    Ok(fetched)
}

/// Lists git remotes that are not in `configured_names`, as (name, url) pairs.
/// Used to detect extra remotes (e.g. a local VM mirror) that ComfyGit does
/// not manage, so the user can be offered to sync them explicitly.
pub fn unconfigured_remotes(
    repo_root: &str,
    configured_names: &[&str],
) -> Result<Vec<(String, String)>> {
    let mut extras = Vec::new();
    for name in git_remote_names(repo_root)? {
        if configured_names
            .iter()
            .any(|configured| configured == &name)
        {
            continue;
        }
        let url = run_git_checked(repo_root, &["remote", "get-url", &name])
            .map(|url| url.trim().to_string())
            .unwrap_or_default();
        extras.push((name, url));
    }
    Ok(extras)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_sync_report_in_sync_when_both_remotes_track() {
        let report = MirrorSyncReport {
            branch: "main".to_string(),
            head_line: "abc1234 (HEAD -> main, origin/main, gitlab/main) init".to_string(),
            gitlab_remote: "gitlab".to_string(),
            github_remote: "origin".to_string(),
            gitlab_tracking: true,
            github_tracking: true,
        };
        assert!(report.in_sync());
    }

    #[test]
    fn mirror_sync_report_out_of_sync_when_github_missing() {
        let report = MirrorSyncReport {
            branch: "main".to_string(),
            head_line: "abc1234 (HEAD -> main, gitlab/main) init".to_string(),
            gitlab_remote: "gitlab".to_string(),
            github_remote: "origin".to_string(),
            gitlab_tracking: true,
            github_tracking: false,
        };
        assert!(!report.in_sync());
    }

    #[test]
    fn mirror_sync_status_colored_uses_palette_codes() {
        let in_sync = MirrorSyncReport {
            branch: "main".to_string(),
            head_line: String::new(),
            gitlab_remote: "gitlab".to_string(),
            github_remote: "origin".to_string(),
            gitlab_tracking: true,
            github_tracking: true,
        };
        assert!(in_sync.status_colored().contains("38;5;46m"));
        assert!(in_sync.status_colored().contains("in sync"));

        let out_of_sync = MirrorSyncReport {
            github_tracking: false,
            ..in_sync
        };
        assert!(out_of_sync.status_colored().contains("38;5;196m"));
        assert!(out_of_sync.status_colored().contains("out of sync"));
    }

    fn temp_dir(test_name: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "comfygit-sync-tests-{}-{}-{}",
            test_name,
            std::process::id(),
            unique
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn init_repo(path: &std::path::Path, bare: bool) {
        let path = path.display().to_string();
        if bare {
            run_git_checked(&path, &["init", "--bare"]).expect("init bare repo");
        } else {
            run_git_checked(&path, &["init"]).expect("init repo");
            run_git_checked(&path, &["config", "user.name", "ComfyGit Tests"])
                .expect("configure user.name");
            run_git_checked(&path, &["config", "user.email", "tests@comfygit.invalid"])
                .expect("configure user.email");
            run_git_checked(&path, &["commit", "--allow-empty", "-m", "init"])
                .expect("create initial commit");
        }
    }

    #[test]
    fn unconfigured_remotes_returns_remotes_outside_the_configured_set() {
        let repo_dir = temp_dir("unconfigured");
        init_repo(&repo_dir, false);
        let repo_root = repo_dir.display().to_string();
        run_git_checked(
            &repo_root,
            &["remote", "add", "gitLAB", "git@gitlab.com:org/repo.git"],
        )
        .expect("add gitLAB remote");
        run_git_checked(
            &repo_root,
            &["remote", "add", "gitHUB", "git@github.com:org/repo.git"],
        )
        .expect("add gitHUB remote");
        run_git_checked(&repo_root, &["remote", "add", "win", "vmhost:C:/repo.git"])
            .expect("add win remote");

        let extras = unconfigured_remotes(&repo_root, &["gitLAB", "gitHUB"])
            .expect("list unconfigured remotes");
        assert_eq!(
            extras,
            vec![("win".to_string(), "vmhost:C:/repo.git".to_string())]
        );

        let none = unconfigured_remotes(&repo_root, &["gitLAB", "gitHUB", "win"])
            .expect("no extras when all remotes are configured");
        assert!(none.is_empty());

        std::fs::remove_dir_all(&repo_dir).ok();
    }

    #[test]
    fn fetch_configured_remotes_does_not_touch_unlisted_remotes() {
        // `win` points at a path that does not exist: `git fetch --all` would
        // fail on it, but fetching only the configured remote must succeed.
        let remote_dir = temp_dir("fetch-remote");
        init_repo(&remote_dir, true);
        let repo_dir = temp_dir("fetch-configured");
        init_repo(&repo_dir, false);
        let repo_root = repo_dir.display().to_string();
        let remote_url = remote_dir.display().to_string();
        run_git_checked(&repo_root, &["remote", "add", "gitLAB", &remote_url])
            .expect("add gitLAB remote");
        run_git_checked(
            &repo_root,
            &["remote", "add", "win", "/nonexistent/win-remote.git"],
        )
        .expect("add win remote");

        // The configured spec may also be the URL rather than the remote name.
        let fetched = fetch_configured_remotes(&repo_root, &[remote_url], None)
            .expect("fetch only the configured remote");
        assert_eq!(fetched, vec!["gitLAB".to_string()]);

        // With no resolvable configured remote, falls back to a plain fetch.
        let fallback_dir = temp_dir("fetch-fallback");
        init_repo(&fallback_dir, false);
        let fallback_root = fallback_dir.display().to_string();
        run_git_checked(
            &fallback_root,
            &["remote", "add", "origin", &remote_dir.display().to_string()],
        )
        .expect("add origin remote");
        run_git_checked(
            &fallback_root,
            &["remote", "add", "win", "/nonexistent/win-remote.git"],
        )
        .expect("add win remote");
        let fetched = fetch_configured_remotes(&fallback_root, &[], None)
            .expect("plain fetch uses only the default remote");
        assert!(fetched.is_empty());

        std::fs::remove_dir_all(&remote_dir).ok();
        std::fs::remove_dir_all(&repo_dir).ok();
        std::fs::remove_dir_all(&fallback_dir).ok();
    }
}
