// Copyright © 2026 ComfyHome™
// All rights reserved.
//
// Licensed under the ComfyGit License v1.2

use std::{
    env,
    io::{self, Write},
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use crossterm::{
    cursor::{MoveToColumn, MoveToNextLine, MoveUp},
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    queue,
    style::Print,
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode},
};

use crate::{
    cli::{
        best_effort_canonicalize, find_project_for_cwd, find_scope_for_cwd,
        mirror_sync_after_merge_for_repo, project_root, scope_root,
    },
    config::{ConfigStore, ProjectConfig, ProjectType},
    git::{
        MirrorSyncReport, check_mirror_sync, push_mirror_sync, run_git_with_cancel,
        unconfigured_remotes,
    },
};

const ANSI_CYAN: &str = "\x1b[36m";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_BRIGHT_GREEN: &str = "\x1b[38;2;94;255;0m"; // RGB: 94, 255, 0
const ANSI_YELLOW: &str = "\x1b[33m";
const ANSI_DARK_GREY: &str = "\x1b[90m";
const ANSI_RESET: &str = "\x1b[0m";

const EXTRA_REMOTES_PROMPT_SECONDS: u64 = 10;

pub(crate) fn run_sync() -> Result<()> {
    run_sync_with_options(false)
}

pub(crate) fn run_sync_with_options(skip_confirm: bool) -> Result<()> {
    let cwd =
        best_effort_canonicalize(&env::current_dir().context("failed to read current directory")?);
    let config = ConfigStore::locate()?.load()?;
    let project = find_project_for_cwd(&config.projects, &cwd).map_err(|_| {
        anyhow::anyhow!(
            "no ComfyGit project covers the current directory; run 'cg init' or open the TUI from a registered project root"
        )
    })?;

    if !project.integration_mode.is_dual_forge() {
        bail!(
            "cg sync requires a GitLab+GitHub project (current mode: {})",
            project.integration_mode.display_name()
        );
    }

    let repo_root = sync_repo_root_for_cwd(project, &cwd)?;
    run_mirror_sync_for_project(project, &repo_root, skip_confirm)
}

/// After a ComfyGit MR/PR merge, push to both remotes when the project opts in.
pub(crate) fn run_mirror_sync_after_comfygit_merge(repo_root: &str) -> Result<()> {
    let cwd =
        best_effort_canonicalize(&env::current_dir().context("failed to read current directory")?);
    let config = ConfigStore::locate()?.load()?;
    let policy = mirror_sync_after_merge_for_repo(&config.projects, repo_root, &cwd);
    if !policy.runs_automatically_after_merge() {
        return Ok(());
    }

    let canonical_repo_root = best_effort_canonicalize(Path::new(repo_root));
    let project = find_project_for_cwd(&config.projects, &cwd)
        .or_else(|_| find_project_for_cwd(&config.projects, &canonical_repo_root))?;
    if !project.integration_mode.is_dual_forge() {
        return Ok(());
    }

    run_mirror_sync_for_project(project, &canonical_repo_root, true)
}

fn sync_repo_root_for_cwd(project: &ProjectConfig, cwd: &Path) -> Result<std::path::PathBuf> {
    if project.project_type == ProjectType::AllInOne {
        return project_root(project);
    }

    let scope_index = find_scope_for_cwd(project, project, cwd)?;
    let branch = project
        .branches
        .get(scope_index)
        .with_context(|| format!("scope index {} is out of range", scope_index))?;
    scope_root(project, branch).with_context(|| {
        format!(
            "scope '{}' does not have a resolvable repository root",
            branch.display_name()
        )
    })
}

fn run_mirror_sync_for_project(
    project: &ProjectConfig,
    repo_root: &Path,
    skip_confirm: bool,
) -> Result<()> {
    let (gitlab_remote, github_remote) = configured_dual_remotes(project, repo_root)?;
    let repo_root = repo_root
        .to_str()
        .context("repo root path is not valid UTF-8")?;

    if skip_confirm {
        return run_mirror_sync_push(repo_root, gitlab_remote, github_remote, skip_confirm);
    }

    println!();
    println!("{ANSI_CYAN}ComfyGit mirror sync{ANSI_RESET} {ANSI_DARK_GREY}(cg sync){ANSI_RESET}");
    println!();
    println!("  Project: {ANSI_CYAN}{}{ANSI_RESET}", project.name);
    if project.project_type == ProjectType::Branched
        && let Some(scope_name) = scope_display_name_for_repo_root(project, Path::new(repo_root))
    {
        println!("  Scope:   {ANSI_CYAN}{scope_name}{ANSI_RESET}");
    }
    println!("  Repo:    {ANSI_DARK_GREY}{}{ANSI_RESET}", repo_root);

    run_mirror_sync_push(repo_root, gitlab_remote, github_remote, skip_confirm)
}

/// Remote specs (names or URLs, as stored in the project config) that
/// ComfyGit manages for the repository at `repo_root`. Best-effort: returns an
/// empty list when the config cannot be loaded or no project covers the repo,
/// in which case callers fall back to default-remote behavior.
pub(crate) fn configured_remote_specs_for_repo(repo_root: &str) -> Vec<String> {
    let canonical_repo_root = best_effort_canonicalize(Path::new(repo_root));
    let Ok(config) = ConfigStore::locate().and_then(|store| store.load()) else {
        return Vec::new();
    };
    let cwd = env::current_dir()
        .map(|cwd| best_effort_canonicalize(&cwd))
        .unwrap_or_else(|_| canonical_repo_root.clone());
    let Ok(project) = find_project_for_cwd(&config.projects, &canonical_repo_root)
        .or_else(|_| find_project_for_cwd(&config.projects, &cwd))
    else {
        return Vec::new();
    };
    configured_dual_remotes(project, &canonical_repo_root)
        .map(|(primary, secondary)| [primary, secondary].into_iter().flatten().collect())
        .unwrap_or_default()
}

fn run_mirror_sync_push(
    repo_root: &str,
    gitlab_remote: Option<String>,
    github_remote: Option<String>,
    skip_confirm: bool,
) -> Result<()> {
    let report = check_mirror_sync(
        repo_root,
        gitlab_remote.as_deref(),
        github_remote.as_deref(),
    )?;
    for line in report.summary_lines() {
        println!("  {line}");
    }
    println!();

    if report.in_sync() {
        println!("{ANSI_GREEN}GitLab and GitHub remotes are already in sync.{ANSI_RESET}");
    } else if !skip_confirm
        && !prompt_yes_no("Push the current branch to both remotes now?", false)?
    {
        println!("Sync skipped.");
    } else {
        let lines = push_mirror_sync(
            repo_root,
            gitlab_remote.as_deref(),
            github_remote.as_deref(),
        )?;
        for line in lines {
            println!("  {ANSI_GREEN}{line}{ANSI_RESET}");
        }

        let follow_up = check_mirror_sync(
            repo_root,
            gitlab_remote.as_deref(),
            github_remote.as_deref(),
        )?;
        println!();
        if follow_up.in_sync() {
            println!("{ANSI_BRIGHT_GREEN}GitLab and GitHub remotes are now in sync.{ANSI_RESET}");
        } else {
            println!(
                "{ANSI_YELLOW}Sync finished, but remotes still appear out of sync.{ANSI_RESET}"
            );
            for line in follow_up.summary_lines() {
                println!("  {line}");
            }
        }
    }

    offer_unconfigured_remote_sync(repo_root, &report)
}

/// Offers to push the current branch to git remotes that are not configured
/// in ComfyGit (e.g. a local VM mirror). Defaults to skipping them after a
/// short timeout so extra remotes never block the sync flow; failures on
/// extra remotes are reported as warnings only.
fn offer_unconfigured_remote_sync(repo_root: &str, report: &MirrorSyncReport) -> Result<()> {
    let extras = unconfigured_remotes(repo_root, &[&report.gitlab_remote, &report.github_remote])?;
    if extras.is_empty() {
        return Ok(());
    }

    if !prompt_sync_unconfigured_remotes(&extras)? {
        return Ok(());
    }

    println!();
    for (remote, _url) in &extras {
        match run_git_with_cancel(repo_root, &["push", remote, &report.branch], None) {
            Ok(output) if output.success => {
                println!(
                    "  {ANSI_GREEN}Pushed '{}' to remote '{remote}'.{ANSI_RESET}",
                    report.branch
                );
            }
            Ok(output) => {
                let details = format!("{}{}", output.stdout, output.stderr);
                eprintln!(
                    "{ANSI_YELLOW}Warning: failed to push '{}' to remote '{remote}': {}{ANSI_RESET}",
                    report.branch,
                    details.trim()
                );
            }
            Err(error) => {
                eprintln!(
                    "{ANSI_YELLOW}Warning: failed to push '{}' to remote '{remote}': {error:#}{ANSI_RESET}",
                    report.branch
                );
            }
        }
    }
    Ok(())
}

/// Shows the extra-remotes menu and waits up to
/// [`EXTRA_REMOTES_PROMPT_SECONDS`] for a choice. Arrow keys move the
/// selection, Enter confirms, `1`/`2` pick directly. Returns `true` when the
/// user picks "Yes. Sync all."; defaults to `false` on Esc, timeout, or when
/// raw terminal input is unavailable.
fn prompt_sync_unconfigured_remotes(remotes: &[(String, String)]) -> Result<bool> {
    println!();
    println!("{ANSI_CYAN}ComfyGit found another remote(s) for this project:{ANSI_RESET}");
    println!();
    for (name, url) in remotes {
        if url.is_empty() {
            println!("  {ANSI_YELLOW}{name}{ANSI_RESET}");
        } else {
            println!("  {ANSI_YELLOW}{name}{ANSI_RESET}  {ANSI_DARK_GREY}{url}{ANSI_RESET}");
        }
    }
    println!();
    println!("Do you want to sync them as well?");
    println!();
    println!("{ANSI_GREEN}> 1.{ANSI_RESET} No. Sync only those configured in CG.");
    println!("  2. Yes. Sync all.");
    println!();

    let Ok(_raw_mode) = CliSyncRawModeGuard::enter() else {
        println!("{ANSI_DARK_GREY}Continuing with option 1 (No).{ANSI_RESET}");
        return Ok(false);
    };

    let deadline = Instant::now() + Duration::from_secs(EXTRA_REMOTES_PROMPT_SECONDS);
    let mut selected = 0usize;
    let mut rendered: Option<(usize, u64)> = None;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            println!("\r\x1b[2K{ANSI_DARK_GREY}Continuing with option 1 (No).{ANSI_RESET}");
            return Ok(false);
        }
        let seconds_left = remaining.as_secs().max(1);
        if rendered != Some((selected, seconds_left)) {
            render_extra_remotes_tail(selected, seconds_left)?;
            rendered = Some((selected, seconds_left));
        }

        if !event::poll(Duration::from_millis(200)).context("failed to poll remote sync input")? {
            continue;
        }
        let Event::Key(key) = event::read().context("failed to read remote sync input")? else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        match key.code {
            KeyCode::Up | KeyCode::Down => {
                selected = 1 - selected;
            }
            KeyCode::Enter => {
                println!("\r\x1b[2K");
                return Ok(selected == 1);
            }
            KeyCode::Char('1') | KeyCode::Esc => {
                println!("\r\x1b[2K");
                return Ok(false);
            }
            KeyCode::Char('2') => {
                println!("\r\x1b[2K");
                return Ok(true);
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                println!("\r\x1b[2K");
                bail!("cancelled by user");
            }
            _ => {}
        }
    }
}

/// Redraws the two menu options and the countdown line in place. The cursor
/// is expected on the countdown line (the last line of the menu block); it is
/// moved up to the first option, all three lines are rewritten, and the
/// cursor is left on the countdown line again.
fn render_extra_remotes_tail(selected: usize, seconds_left: u64) -> Result<()> {
    let mut stdout = io::stdout();
    let option_line = |marked: bool, label: &str| {
        if marked {
            format!("{ANSI_GREEN}> {label}{ANSI_RESET}")
        } else {
            format!("  {label}")
        }
    };
    queue!(
        stdout,
        MoveToColumn(0),
        MoveUp(3),
        Clear(ClearType::CurrentLine),
        Print(option_line(
            selected == 0,
            "1. No. Sync only those configured in CG."
        )),
        MoveToNextLine(1),
        Clear(ClearType::CurrentLine),
        Print(option_line(selected == 1, "2. Yes. Sync all.")),
        MoveToNextLine(2),
        Clear(ClearType::CurrentLine),
        Print(format!(
            "{ANSI_DARK_GREY}Continuing with option {} in {}s — use ↑/↓ then Enter, or press 1/2.{ANSI_RESET}",
            selected + 1,
            seconds_left
        )),
    )
    .context("failed to render remote sync menu")?;
    stdout.flush()?;
    Ok(())
}

struct CliSyncRawModeGuard;

impl CliSyncRawModeGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("failed to enable raw mode for remote sync prompt")?;
        Ok(Self)
    }
}

impl Drop for CliSyncRawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

fn scope_display_name_for_repo_root(project: &ProjectConfig, repo_root: &Path) -> Option<String> {
    let canonical = best_effort_canonicalize(repo_root);
    project.branches.iter().find_map(|branch| {
        scope_root(project, branch).and_then(|root| {
            (best_effort_canonicalize(&root) == canonical)
                .then(|| branch.display_name().to_string())
        })
    })
}

fn configured_dual_remotes(
    project: &ProjectConfig,
    repo_root: &Path,
) -> Result<(Option<String>, Option<String>)> {
    if project.project_type == ProjectType::AllInOne {
        let repo = project.repo.as_ref().ok_or_else(|| {
            anyhow::anyhow!("GitLab+GitHub project is missing repo configuration")
        })?;
        return Ok((repo.remote_url.clone(), repo.secondary_remote_url.clone()));
    }

    let canonical = best_effort_canonicalize(repo_root);
    for branch in &project.branches {
        let Some(scope_root) = scope_root(project, branch) else {
            continue;
        };
        if best_effort_canonicalize(&scope_root) != canonical {
            continue;
        }
        let repo = branch
            .repo
            .as_ref()
            .or(project.repo.as_ref())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "GitLab+GitHub scope '{}' is missing repo configuration",
                    branch.display_name()
                )
            })?;
        return Ok((repo.remote_url.clone(), repo.secondary_remote_url.clone()));
    }

    if let Some(repo) = project.repo.as_ref()
        && best_effort_canonicalize(Path::new(repo.local_root.trim())) == canonical
    {
        return Ok((repo.remote_url.clone(), repo.secondary_remote_url.clone()));
    }

    bail!(
        "GitLab+GitHub project has no scope configured for repository '{}'",
        repo_root.display()
    )
}

fn prompt_yes_no(prompt: &str, default_yes: bool) -> Result<bool> {
    let suffix = if default_yes { "[Y/n]" } else { "[y/N]" };
    print!("{prompt} {suffix} ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let answer = line.trim().to_ascii_lowercase();
    if answer.is_empty() {
        return Ok(default_yes);
    }
    Ok(matches!(answer.as_str(), "y" | "yes"))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::config::{
        BranchConfig, BranchScopeKind, IntegrationMode, MirrorSyncAfterMerge, ProjectConfig,
        ProjectType, RepoConfig, TargetFormat, TargetSpec,
    };
    use crate::workflow::versioning::VersionScheme;

    #[test]
    fn dual_forge_mode_requires_sync_command() {
        assert!(IntegrationMode::GitLabGitHubEnabled.is_dual_forge());
        assert!(!IntegrationMode::GitLabEnabled.is_dual_forge());
    }

    #[test]
    fn mirror_sync_policy_default_runs_after_merge() {
        assert!(MirrorSyncAfterMerge::default().runs_automatically_after_merge());
    }

    fn dual_scope_project() -> ProjectConfig {
        ProjectConfig {
            name: "demo".to_string(),
            alias: String::new(),
            project_type: ProjectType::Branched,
            integration_mode: IntegrationMode::GitLabGitHubEnabled,
            unified_versioning: false,
            version_scheme: VersionScheme::SemVer,
            targets: Vec::new(),
            branches: vec![
                BranchConfig {
                    name: "core".to_string(),
                    label: "Core".to_string(),
                    scope_kind: BranchScopeKind::Branch,
                    repo: Some(RepoConfig {
                        local_root: "/tmp/demo/core".to_string(),
                        remote_url: Some("git@gitlab.com:org/core.git".to_string()),
                        secondary_remote_url: Some("git@github.com:org/core.git".to_string()),
                        ..RepoConfig::default()
                    }),
                    changelog_enabled: false,
                    changelog_path: None,
                    changelog_hide_pr_messages: false,
                    changelog_hide_bump_messages: false,
                    changelog_mini_commit_hashes: false,
                    changelog_mirror_summary_to_root_changelog: false,
                    changelog_wrap_detailed_if_top_picks: false,
                    release_now: crate::config::ReleaseNowSettings::default(),
                    version_scheme: VersionScheme::SemVer,
                    targets: vec![TargetSpec {
                        label: "Version".to_string(),
                        path: "Cargo.toml".to_string(),
                        key_path: "package.version".to_string(),
                        format: TargetFormat::Toml,
                    }],
                    advanced_alias: Default::default(),
                },
                BranchConfig {
                    name: "api".to_string(),
                    label: "API".to_string(),
                    scope_kind: BranchScopeKind::Service,
                    repo: Some(RepoConfig {
                        local_root: "/tmp/demo/api".to_string(),
                        remote_url: Some("git@gitlab.com:org/api.git".to_string()),
                        secondary_remote_url: Some("git@github.com:org/api.git".to_string()),
                        ..RepoConfig::default()
                    }),
                    changelog_enabled: false,
                    changelog_path: None,
                    changelog_hide_pr_messages: false,
                    changelog_hide_bump_messages: false,
                    changelog_mini_commit_hashes: false,
                    changelog_mirror_summary_to_root_changelog: false,
                    changelog_wrap_detailed_if_top_picks: false,
                    release_now: crate::config::ReleaseNowSettings::default(),
                    version_scheme: VersionScheme::SemVer,
                    targets: vec![TargetSpec {
                        label: "Version".to_string(),
                        path: "Cargo.toml".to_string(),
                        key_path: "package.version".to_string(),
                        format: TargetFormat::Toml,
                    }],
                    advanced_alias: Default::default(),
                },
            ],
            repo: None,
            ..Default::default()
        }
    }

    #[test]
    fn sync_repo_root_for_cwd_uses_matching_scope_not_core() {
        let project = dual_scope_project();
        let cwd = PathBuf::from("/tmp/demo/api/src");
        let repo_root =
            super::sync_repo_root_for_cwd(&project, &cwd).expect("api scope root should resolve");
        assert_eq!(repo_root, PathBuf::from("/tmp/demo/api"));
    }

    #[test]
    fn configured_dual_remotes_selects_scope_matching_repo_root() {
        let project = dual_scope_project();
        let (gitlab, github) = super::configured_dual_remotes(&project, Path::new("/tmp/demo/api"))
            .expect("api remotes should resolve");
        assert_eq!(gitlab.as_deref(), Some("git@gitlab.com:org/api.git"));
        assert_eq!(github.as_deref(), Some("git@github.com:org/api.git"));
    }
}
