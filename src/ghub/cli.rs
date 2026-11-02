// Copyright © 2026 ComfyHome™
// All rights reserved.
//
// Licensed under the ComfyGit License v1.2

use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail};

pub const CLI_NAME: &str = "gh";

pub fn ensure_available() -> Result<()> {
    let output = Command::new(CLI_NAME)
        .arg("--version")
        .output()
        .with_context(|| format!("failed to invoke {CLI_NAME}; install GitHub CLI"))?;
    if output.status.success() {
        Ok(())
    } else {
        bail!("{CLI_NAME} is not available or not functioning; install GitHub CLI")
    }
}

pub fn ensure_authenticated() -> Result<()> {
    ensure_available()?;
    let output = Command::new(CLI_NAME)
        .args(["auth", "status"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("failed to invoke gh auth status")?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if stderr.is_empty() { stdout } else { stderr };
        bail!("GitHub CLI is not authenticated: {detail}")
    }
}

pub fn run_in_repo(repo_root: &str, args: &[&str]) -> Result<Output> {
    // Resolve the repo slug from the default push remote and inject
    // `--repo <slug>` so that `gh` always targets the correct repository.
    // Without this, `gh` may pick the wrong remote when a repo has multiple
    // remotes (e.g. a fork + an upstream), causing PR creation to fail with
    // "No commits between ..." or "Head sha can't be blank".
    let slug = resolve_repo_slug(repo_root);
    let mut full_args: Vec<String> = Vec::with_capacity(args.len() + 2);
    if let Some(slug) = &slug {
        full_args.push("--repo".to_string());
        full_args.push(slug.clone());
    }
    full_args.extend(args.iter().map(|a| a.to_string()));

    Command::new(CLI_NAME)
        .current_dir(repo_root)
        .args(&full_args)
        .output()
        .with_context(|| format!("failed to execute {CLI_NAME} {}", full_args.join(" ")))
}

/// Resolves the `owner/repo` slug for the default push remote of the repo.
/// Returns `None` if the slug can't be resolved (e.g. no GitHub remote),
/// in which case `gh` will fall back to its own auto-detection.
fn resolve_repo_slug(repo_root: &str) -> Option<String> {
    let remote_name = crate::git::default_push_remote_name(repo_root).ok()?;
    let remote_url =
        crate::git::run_git_checked(repo_root, &["remote", "get-url", &remote_name]).ok()?;
    let (owner, repo) = crate::ghub::remote::owner_repo_from_remote_url(remote_url.trim())?;
    Some(format!("{owner}/{repo}"))
}
