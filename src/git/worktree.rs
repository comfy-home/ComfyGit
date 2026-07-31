// Copyright © 2026 ComfyHome™
// All rights reserved.
//
// Licensed under the ComfyGit SA-PS License
//
// For details, see the LICENSE file in the repository root.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode},
};

use crate::cli::best_effort_canonicalize;
use crate::git::{GitCancellation, run_git_checked, run_git_checked_with_cancel};

// ---------------------------------------------------------------------------
// Worktree detection helpers
// ---------------------------------------------------------------------------

/// Returns the git common directory (shared `.git` dir) for the repo at `cwd`.
/// For the main worktree this is `<repo>/.git`.  For a linked worktree this is
/// also the main worktree's `.git` directory.
pub(crate) fn git_common_dir(cwd: &Path) -> Result<PathBuf> {
    let cwd_display = cwd.display().to_string();
    let output = run_git_checked(&cwd_display, &["rev-parse", "--git-common-dir"])?;
    let path = PathBuf::from(output.trim());
    Ok(best_effort_canonicalize(&path))
}

/// Returns the root of the main worktree (the directory that contains the
/// `.git` common dir).  Works correctly when called from a linked worktree.
pub(crate) fn main_worktree_root(cwd: &Path) -> Result<PathBuf> {
    let common_dir = git_common_dir(cwd)?;
    // The common dir is `<main_root>/.git` for a normal repo.
    // For bare repos or worktrees the common dir may be elsewhere,
    // but for standard setups the parent of `.git` is the main worktree root.
    let parent = common_dir
        .parent()
        .context("git common dir has no parent — cannot determine main worktree root")?;
    Ok(best_effort_canonicalize(parent))
}

/// True when `cwd` is inside a linked worktree (not the main worktree).
pub(crate) fn is_linked_worktree(cwd: &Path) -> bool {
    let Ok(toplevel) = current_worktree_toplevel(cwd) else {
        return false;
    };
    let Ok(main_root) = main_worktree_root(cwd) else {
        return false;
    };
    toplevel != main_root
}

/// Returns the toplevel of the working tree that contains `cwd`.
/// This is equivalent to `git rev-parse --show-toplevel`.
pub(crate) fn current_worktree_toplevel(cwd: &Path) -> Result<PathBuf> {
    let cwd_display = cwd.display().to_string();
    let output = run_git_checked(&cwd_display, &["rev-parse", "--show-toplevel"])?;
    let path = PathBuf::from(output.trim());
    Ok(best_effort_canonicalize(&path))
}

// ---------------------------------------------------------------------------
// Worktree listing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct WorktreeInfo {
    pub path: String,
    pub head: String,
    pub branch: Option<String>,
    pub locked: bool,
    pub bare: bool,
}

/// Lists all worktrees for the repository at `repo_root`.
/// `repo_root` can be the main worktree or any linked worktree.
pub(crate) fn list_worktrees(repo_root: &str) -> Result<Vec<WorktreeInfo>> {
    let output = run_git_checked(repo_root, &["worktree", "list", "--porcelain"])?;
    parse_worktree_list(&output)
}

fn parse_worktree_list(output: &str) -> Result<Vec<WorktreeInfo>> {
    let mut worktrees = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_head = String::new();
    let mut current_branch: Option<String> = None;
    let mut current_locked = false;
    let mut current_bare = false;

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            if let Some(path) = current_path.take() {
                worktrees.push(WorktreeInfo {
                    path,
                    head: std::mem::take(&mut current_head),
                    branch: current_branch.take(),
                    locked: current_locked,
                    bare: current_bare,
                });
                current_locked = false;
                current_bare = false;
            }
            continue;
        }

        if let Some(path) = line.strip_prefix("worktree ") {
            current_path = Some(path.to_string());
        } else if let Some(head) = line.strip_prefix("HEAD ") {
            current_head = head.to_string();
        } else if let Some(branch) = line.strip_prefix("branch ") {
            // branch refs/heads/<name>
            current_branch = Some(
                branch
                    .strip_prefix("refs/heads/")
                    .unwrap_or(branch)
                    .to_string(),
            );
        } else if line == "locked" {
            current_locked = true;
        } else if line == "bare" {
            current_bare = true;
        }
        // detached lines are just "detached" — we ignore them
    }

    // Flush the last entry
    if let Some(path) = current_path.take() {
        worktrees.push(WorktreeInfo {
            path,
            head: current_head,
            branch: current_branch,
            locked: current_locked,
            bare: current_bare,
        });
    }

    Ok(worktrees)
}

/// Checks whether the worktree at `path` has uncommitted changes.
fn worktree_is_dirty(path: &str) -> bool {
    run_git_checked(path, &["status", "--porcelain"])
        .map(|output| !output.trim().is_empty())
        .unwrap_or(false)
}

/// Returns a human-readable summary of why a worktree is dirty, or `None`
/// when it is clean.  Parses `git status --porcelain` output into counts by
/// category (modified, staged, untracked, deleted, etc.).
fn worktree_dirty_reason(path: &str) -> Option<String> {
    let output = run_git_checked(path, &["status", "--porcelain"]).ok()?;
    let lines: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return None;
    }

    let mut modified = 0u32;
    let mut staged = 0u32;
    let mut untracked = 0u32;
    let mut deleted = 0u32;

    for line in &lines {
        let bytes = line.as_bytes();
        let index = bytes.first().copied().unwrap_or(b' ');
        let worktree = bytes.get(1).copied().unwrap_or(b' ');

        if index == b'?' && worktree == b'?' {
            untracked += 1;
        } else if index == b'!' && worktree == b'!' {
            // ignored — not counted
        } else {
            if index != b' ' && index != b'?' {
                staged += 1;
            }
            if worktree == b'M' {
                modified += 1;
            }
            if worktree == b'D' || index == b'D' {
                deleted += 1;
            }
        }
    }

    let mut parts: Vec<String> = Vec::new();
    if staged > 0 {
        parts.push(format!("{staged} staged"));
    }
    if modified > 0 {
        parts.push(format!("{modified} modified"));
    }
    if deleted > 0 {
        parts.push(format!("{deleted} deleted"));
    }
    if untracked > 0 {
        parts.push(format!("{untracked} untracked"));
    }

    if parts.is_empty() {
        // Fallback: just show the raw line count.
        Some(format!("{} change(s)", lines.len()))
    } else {
        Some(parts.join(", "))
    }
}

// ---------------------------------------------------------------------------
// Worktree path calculation
// ---------------------------------------------------------------------------

/// Computes the default worktree directory for a project.
/// Default: `<parent_of_project_root>/<project_basename>.worktrees/<branch_name>`
/// If `worktree_root` is configured, uses that instead.
pub(crate) fn default_worktree_path(
    project_root: &Path,
    branch_name: &str,
    worktree_root: Option<&str>,
) -> PathBuf {
    if let Some(custom_root) = worktree_root.map(str::trim).filter(|s| !s.is_empty()) {
        return PathBuf::from(custom_root).join(branch_name);
    }

    let parent = project_root.parent().unwrap_or_else(|| Path::new("/"));
    let basename = project_root
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());
    parent
        .join(format!("{basename}.worktrees"))
        .join(branch_name)
}

// ---------------------------------------------------------------------------
// Worktree commands
// ---------------------------------------------------------------------------

/// `cg wt new` — interactive worktree creation.
/// Prompts for a base branch name, then creates a worktree at the default path.
pub(crate) fn run_wt_new(
    project_name: &str,
    project_root: &Path,
    worktree_root: Option<&str>,
) -> Result<()> {
    println!();
    println!("\x1b[36mCreate a new worktree for {project_name}\x1b[0m");
    println!();

    let branch_name = prompt_worktree_branch_name()?;
    let branch_name = sanitize_branch_name(&branch_name)?;

    let wt_path = default_worktree_path(project_root, &branch_name, worktree_root);
    let wt_path_display = wt_path.display().to_string();

    // Check if the worktree path already exists
    if wt_path.exists() {
        bail!("worktree path already exists: {}", wt_path_display);
    }

    // Check if branch exists locally
    let project_root_str = project_root.display().to_string();
    let branch_exists = run_git_checked(
        &project_root_str,
        &[
            "rev-parse",
            "--verify",
            &format!("refs/heads/{branch_name}"),
        ],
    )
    .is_ok();

    println!("Creating worktree at: \x1b[33m{wt_path_display}\x1b[0m");
    if branch_exists {
        run_git_checked(
            &project_root_str,
            &["worktree", "add", &wt_path_display, &branch_name],
        )?;
    } else {
        // Create a new branch from current HEAD
        run_git_checked(
            &project_root_str,
            &["worktree", "add", "-b", &branch_name, &wt_path_display],
        )?;
    }

    // Adjust relative paths in config files (Cargo.toml, pyproject.toml,
    // package.json, tsconfig.json) so external dependencies resolve
    // correctly from the worktree's deeper directory.
    let adjusted =
        crate::git::adjust_paths_for_worktree(project_root, &wt_path).unwrap_or_else(|err| {
            eprintln!(
                "\x1b[33mwarning: failed to adjust config paths in worktree: {}\x1b[0m",
                err
            );
            0
        });
    if adjusted > 0 {
        run_git_checked(&wt_path_display, &["add", "-A"])?;
        run_git_checked(
            &wt_path_display,
            &["commit", "-m", "chore: adjust relative paths for worktree"],
        )?;
        println!("  Adjusted \x1b[33m{adjusted}\x1b[0m relative path(s) in config files.");
    }

    println!();
    println!("\x1b[32mWorktree created successfully.\x1b[0m");
    println!();
    println!("  Branch: \x1b[33m{branch_name}\x1b[0m");
    println!("  Path:   \x1b[33m{wt_path_display}\x1b[0m");
    println!();
    println!("To switch to this worktree, run:");
    println!("  \x1b[36mcg wt cd\x1b[0m");
    println!();

    Ok(())
}

/// `cg wt end` — merge worktree branch back to the main worktree via a PR/MR.
///
/// Creates a PR/MR from the worktree branch, merges it via the forge (GitHub/
/// GitLab), then syncs the target branch in the main worktree.  After the
/// merge, config paths are restored and the user is offered worktree/branch
/// cleanup.
pub(crate) fn run_wt_end(
    _project_root: &Path,
    _worktree_root: Option<&str>,
    custom_main_branch: Option<&str>,
    comfygitflow_enabled: bool,
    cancel: Option<GitCancellation>,
) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let cwd = best_effort_canonicalize(&cwd);

    if !is_linked_worktree(&cwd) {
        bail!(
            "cg wt end can only be run from inside a linked worktree; the current directory is the main worktree"
        );
    }

    let worktree_toplevel = current_worktree_toplevel(&cwd)?;
    let worktree_path = worktree_toplevel.display().to_string();
    let main_root = main_worktree_root(&cwd)?;
    let main_root_str = main_root.display().to_string();

    // Get the current branch in the worktree
    let current_branch = run_git_checked_with_cancel(
        &worktree_path,
        &["branch", "--show-current"],
        cancel.clone(),
    )?;
    let current_branch = current_branch.trim();
    if current_branch.is_empty() {
        bail!("cannot run cg wt end from a detached HEAD");
    }

    // Ensure the worktree is clean
    let status =
        run_git_checked_with_cancel(&worktree_path, &["status", "--porcelain"], cancel.clone())?;
    if !status.trim().is_empty() {
        bail!(
            "worktree has uncommitted changes; please commit or stash them before running cg wt end"
        );
    }

    // Resolve the forge (GitHub/GitLab) for this repo.
    let forge = crate::forge::require_forge_for_repo(&worktree_path)?;

    println!();
    println!("\x1b[36mEnding worktree via PR/MR\x1b[0m");
    println!();
    println!("  Worktree branch: \x1b[33m{current_branch}\x1b[0m");
    println!();

    // Step 1: Create the PR/MR from the worktree.
    println!("Creating PR/MR from worktree branch \x1b[33m{current_branch}\x1b[0m...");
    let created_pr = crate::git::run_pr_and_capture(
        &worktree_path,
        forge,
        false,
        custom_main_branch,
        comfygitflow_enabled,
        cancel.clone(),
    )?;
    println!();
    println!(
        "  PR/MR #{} created: \x1b[33m{}\x1b[0m",
        created_pr.number, created_pr.url
    );
    println!(
        "  Target branch:    \x1b[33m{}\x1b[0m",
        created_pr.target_branch
    );
    println!();

    // Step 2: Merge the PR/MR via the forge (remote merge only — no local
    // checkout, which would fail in a linked worktree if the target branch
    // is already checked out in the main worktree).
    println!(
        "Merging PR/MR #{} via {}...",
        created_pr.number,
        forge.display_name()
    );
    let merge_result =
        crate::git::merge_pull_request_remote_only(&worktree_path, forge, created_pr.number)?;

    // Step 3: Sync the target branch in the MAIN worktree.
    println!();
    println!(
        "Syncing \x1b[33m{}\x1b[0m in the main worktree...",
        merge_result.target_branch
    );
    crate::git::switch_to_existing_branch_after_merge(&main_root_str, &merge_result.target_branch)?;
    crate::git::sync_current_branch(&main_root_str, cancel.clone())?;

    // Step 4: Restore config paths in the main worktree.
    let restored = crate::git::restore_paths_after_merge(&main_root, &worktree_toplevel)
        .unwrap_or_else(|err| {
            eprintln!(
                "\x1b[33mwarning: failed to restore config paths after merge: {}\x1b[0m",
                err
            );
            0
        });
    if restored > 0 {
        run_git_checked_with_cancel(&main_root_str, &["add", "-A"], cancel.clone())?;
        run_git_checked_with_cancel(
            &main_root_str,
            &[
                "commit",
                "-m",
                "chore: restore relative paths after worktree merge",
            ],
            cancel.clone(),
        )?;
        println!("  Restored \x1b[33m{restored}\x1b[0m relative path(s) in config files.");
    }

    // Step 5: Delete the local source branch if policy says so.
    if merge_result.delete_local_after_merge {
        // The branch is checked out in the worktree, so we can't delete it
        // while the worktree exists.  We'll handle it after worktree removal.
    }

    // Step 6: Mirror sync.
    if let Err(error) =
        crate::workflow::cli_sync::run_mirror_sync_after_comfygit_merge(&main_root_str)
    {
        eprintln!("Warning: mirror sync after merge failed: {error:#}. Run `cg sync` to retry.");
    }

    println!();
    println!(
        "\x1b[32mPR/MR #{} merged, synced \x1b[33m{}\x1b[0m\x1b[32m, and restored config paths.\x1b[0m",
        created_pr.number, merge_result.target_branch
    );
    println!();

    // Step 7: Offer worktree + branch cleanup.
    println!("Remove the worktree now? [Y/n]");
    print!("> ");
    io::stdout().flush().context("failed to flush prompt")?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("failed to read response")?;
    let answer = answer.trim().to_ascii_lowercase();

    if answer.is_empty() || answer == "y" || answer == "yes" {
        println!();
        println!("Removing worktree at \x1b[33m{worktree_path}\x1b[0m...");
        run_git_checked_with_cancel(
            &main_root_str,
            &["worktree", "remove", &worktree_path],
            cancel.clone(),
        )?;

        // Now that the worktree is gone, we can delete the local branch.
        if merge_result.delete_local_after_merge {
            run_git_checked(&main_root_str, &["branch", "-d", current_branch])?;
            println!("\x1b[32mBranch {current_branch} deleted.\x1b[0m");
        } else {
            println!("Delete the merged branch \x1b[33m{current_branch}\x1b[0m? [y/N]");
            print!("> ");
            io::stdout().flush().context("failed to flush prompt")?;
            let mut branch_answer = String::new();
            io::stdin()
                .read_line(&mut branch_answer)
                .context("failed to read response")?;
            let branch_answer = branch_answer.trim().to_ascii_lowercase();
            if branch_answer == "y" || branch_answer == "yes" {
                run_git_checked(&main_root_str, &["branch", "-d", current_branch])?;
                println!("\x1b[32mBranch {current_branch} deleted.\x1b[0m");
            }
        }

        println!();
        println!("\x1b[32mWorktree removed.\x1b[0m");
    } else {
        println!();
        println!("Worktree kept at \x1b[33m{worktree_path}\x1b[0m");
    }

    println!();
    Ok(())
}

/// `cg wt end local` — merge worktree branch back to main worktree via a local
/// merge (no PR/MR).  Optionally remove the worktree and delete the branch.
pub(crate) fn run_wt_end_local(
    _project_root: &Path,
    _worktree_root: Option<&str>,
    _custom_main_branch: Option<&str>,
    _comfygitflow_enabled: bool,
    cancel: Option<GitCancellation>,
) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let cwd = best_effort_canonicalize(&cwd);

    if !is_linked_worktree(&cwd) {
        bail!(
            "cg wt end can only be run from inside a linked worktree; the current directory is the main worktree"
        );
    }

    let worktree_toplevel = current_worktree_toplevel(&cwd)?;
    let worktree_path = worktree_toplevel.display().to_string();

    // Use the actual git main worktree (the one containing `.git`), NOT the
    // configured `project_root`.  The configured root may itself be a linked
    // worktree on a different branch — operating on it would be wrong.
    let main_root = main_worktree_root(&cwd)?;
    let main_root_str = main_root.display().to_string();

    // Get the current branch in the worktree
    let current_branch = run_git_checked_with_cancel(
        &worktree_path,
        &["branch", "--show-current"],
        cancel.clone(),
    )?;
    let current_branch = current_branch.trim();
    if current_branch.is_empty() {
        bail!("cannot run cg wt end from a detached HEAD");
    }

    // Ensure the worktree is clean
    let status =
        run_git_checked_with_cancel(&worktree_path, &["status", "--porcelain"], cancel.clone())?;
    if !status.trim().is_empty() {
        bail!(
            "worktree has uncommitted changes; please commit or stash them before running cg wt end"
        );
    }

    // Determine the merge target: whatever branch is currently checked out
    // in the main worktree.  The user explicitly chose it by checking it out,
    // and the X shortcut lets them change it if needed.  We do NOT use
    // `custom_main_branch` here — that config is for PR operations, not
    // worktree merges.
    let mut target_branch = run_git_checked(&main_root_str, &["symbolic-ref", "--short", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "main".to_string());

    println!();
    println!("\x1b[36mEnding worktree\x1b[0m");
    println!();
    println!("  Worktree branch: \x1b[33m{current_branch}\x1b[0m");
    println!("  Merge target:    \x1b[33m{target_branch}\x1b[0m");
    println!();

    // Offer the user a chance to change the merge target (X) or proceed (ENTER).
    match prompt_merge_target_action(&target_branch)? {
        MergeTargetAction::Proceed => {}
        MergeTargetAction::ChangeTarget => {
            target_branch = crate::git::prompt_target_branch_change(
                &main_root_str,
                current_branch,
                &target_branch,
                cancel.clone(),
            )?;
            println!();
            println!("  New merge target: \x1b[33m{target_branch}\x1b[0m");
            println!();
        }
        MergeTargetAction::Cancel => bail!("cancelled by user"),
    }

    // Check if main worktree is clean BEFORE checking out the target branch.
    // Checking after the checkout can produce false positives: if the current
    // branch and the target branch have different `.gitignore` rules, files
    // that were ignored on the current branch (e.g. build artifacts) may show
    // up as untracked on the target branch.
    let main_status =
        run_git_checked_with_cancel(&main_root_str, &["status", "--porcelain"], cancel.clone())?;
    if !main_status.trim().is_empty() {
        bail!(
            "main worktree has uncommitted changes; please commit or stash them before running cg wt end"
        );
    }

    // Switch to the main worktree and merge
    println!("Switching to main worktree to merge...");
    run_git_checked_with_cancel(
        &main_root_str,
        &["checkout", &target_branch],
        cancel.clone(),
    )?;

    println!("Merging \x1b[33m{current_branch}\x1b[0m into \x1b[33m{target_branch}\x1b[0m...");
    let merge_result = run_git_checked_with_cancel(
        &main_root_str,
        &["merge", "--no-ff", current_branch],
        cancel.clone(),
    );

    match merge_result {
        Ok(_) => {
            println!();
            println!("\x1b[32mMerge successful.\x1b[0m");
            println!();

            // Restore relative paths in config files that were adjusted for
            // the worktree.  The merged files contain worktree-relative paths
            // which must be recomputed relative to the main worktree.
            let restored = crate::git::restore_paths_after_merge(&main_root, &worktree_toplevel)
                .unwrap_or_else(|err| {
                    eprintln!(
                        "\x1b[33mwarning: failed to restore config paths after merge: {}\x1b[0m",
                        err
                    );
                    0
                });
            if restored > 0 {
                run_git_checked_with_cancel(&main_root_str, &["add", "-A"], cancel.clone())?;
                run_git_checked_with_cancel(
                    &main_root_str,
                    &[
                        "commit",
                        "-m",
                        "chore: restore relative paths after worktree merge",
                    ],
                    cancel.clone(),
                )?;
                println!("  Restored \x1b[33m{restored}\x1b[0m relative path(s) in config files.");
                println!();
            }
        }
        Err(error) => {
            // List conflicted files before aborting so the user knows what
            // needs to be resolved.
            let conflicts =
                crate::git::list_unmerged_files(&main_root_str, cancel.clone()).unwrap_or_default();

            // Abort the merge to leave the main worktree clean.
            let _ =
                run_git_checked_with_cancel(&main_root_str, &["merge", "--abort"], cancel.clone());
            // Switch back to the worktree branch.
            let _ = run_git_checked_with_cancel(
                &worktree_path,
                &["checkout", current_branch],
                cancel.clone(),
            );

            println!();
            println!("\x1b[31mMerge failed: {error}\x1b[0m");
            println!();
            if conflicts.is_empty() {
                println!("\x1b[31mThe merge has been aborted.\x1b[0m");
            } else {
                println!("\x1b[31mThe merge has been aborted. Conflicted files:\x1b[0m");
                for file in &conflicts {
                    println!("  \x1b[33m{file}\x1b[0m");
                }
                println!();
                println!("  \x1b[2mTo resolve manually, run in the main worktree:\x1b[0m");
                println!("    \x1b[36mcg wt cd\x1b[0m \x1b[2m# switch to the main worktree\x1b[0m");
                println!("    \x1b[36mgit merge --no-ff {current_branch}\x1b[0m");
                println!("    \x1b[2m# resolve conflicts, then:\x1b[0m");
                println!("    \x1b[36mgit add <files> && git commit\x1b[0m");
            }
            println!();
            println!("Worktree has been kept.");
            bail!("merge failed");
        }
    }

    // Ask if user wants to remove the worktree
    println!("Remove the worktree now? [Y/n]");
    print!("> ");
    io::stdout().flush().context("failed to flush prompt")?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("failed to read response")?;
    let answer = answer.trim().to_ascii_lowercase();

    if answer.is_empty() || answer == "y" || answer == "yes" {
        println!();
        println!("Removing worktree at \x1b[33m{worktree_path}\x1b[0m...");
        run_git_checked_with_cancel(
            &main_root_str,
            &["worktree", "remove", &worktree_path],
            cancel.clone(),
        )?;

        // Optionally delete the merged branch
        println!("Delete the merged branch \x1b[33m{current_branch}\x1b[0m? [y/N]");
        print!("> ");
        io::stdout().flush().context("failed to flush prompt")?;
        let mut branch_answer = String::new();
        io::stdin()
            .read_line(&mut branch_answer)
            .context("failed to read response")?;
        let branch_answer = branch_answer.trim().to_ascii_lowercase();

        if branch_answer == "y" || branch_answer == "yes" {
            run_git_checked(&main_root_str, &["branch", "-d", current_branch])?;
            println!("\x1b[32mBranch {current_branch} deleted.\x1b[0m");
        }

        println!();
        println!("\x1b[32mWorktree removed.\x1b[0m");
    } else {
        println!();
        println!("Worktree kept at \x1b[33m{worktree_path}\x1b[0m");
    }

    println!();
    Ok(())
}

/// `cg wt list` — list all worktrees for the current project.
pub(crate) fn run_wt_list(project_root: &str) -> Result<()> {
    let worktrees = list_worktrees(project_root)?;

    if worktrees.is_empty() {
        println!("No worktrees found.");
        return Ok(());
    }

    println!();
    println!("\x1b[36mWorktrees\x1b[0m");
    println!();
    println!("  {:<30} {:<60} Status", "Branch", "Path");
    println!("  {}", "-".repeat(100));

    for wt in &worktrees {
        let branch_display = if wt.bare {
            "(bare)".to_string()
        } else {
            wt.branch
                .clone()
                .unwrap_or_else(|| "(detached)".to_string())
        };
        let path_display = wt.path.clone();
        let status = if worktree_is_dirty(&wt.path) {
            let reason = worktree_dirty_reason(&wt.path)
                .map(|r| format!(" \x1b[2m({r})\x1b[0m"))
                .unwrap_or_default();
            format!("\x1b[33mdirty\x1b[0m{reason}")
        } else {
            "\x1b[32mclean\x1b[0m".to_string()
        };

        println!("  {:<30} {:<60} {}", branch_display, path_display, status);
    }

    println!();
    Ok(())
}

/// `cg wt status` — show current worktree info.
pub(crate) fn run_wt_status(project_root: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let cwd = best_effort_canonicalize(&cwd);

    let current_toplevel = current_worktree_toplevel(&cwd)?;
    let main_root = main_worktree_root(&cwd)?;
    let is_linked = current_toplevel != main_root;

    let current_branch = run_git_checked(
        &current_toplevel.display().to_string(),
        &["branch", "--show-current"],
    )
    .map(|s| s.trim().to_string())
    .unwrap_or_else(|_| "(detached)".to_string());

    let is_dirty = worktree_is_dirty(&current_toplevel.display().to_string());
    let dirty_reason = worktree_dirty_reason(&current_toplevel.display().to_string());

    println!();
    println!("\x1b[36mWorktree Status\x1b[0m");
    println!();
    println!(
        "  Current worktree: \x1b[33m{}\x1b[0m",
        current_toplevel.display()
    );
    println!("  Branch:           \x1b[33m{current_branch}\x1b[0m");
    println!(
        "  Status:           {}{}",
        if is_dirty {
            "\x1b[33mdirty\x1b[0m"
        } else {
            "\x1b[32mclean\x1b[0m"
        },
        if let Some(reason) = &dirty_reason {
            format!(" \x1b[2m({reason})\x1b[0m")
        } else {
            String::new()
        }
    );
    println!("  Main worktree:    \x1b[33m{}\x1b[0m", main_root.display());
    println!(
        "  Type:             {}",
        if is_linked {
            "linked worktree"
        } else {
            "main worktree"
        }
    );

    // List other worktrees
    let worktrees = list_worktrees(project_root)?;
    let others: Vec<_> = worktrees
        .iter()
        .filter(|wt| best_effort_canonicalize(Path::new(&wt.path)) != current_toplevel)
        .collect();

    if !others.is_empty() {
        println!();
        println!("  Other worktrees:");
        for wt in others {
            let branch_display = if wt.bare {
                "(bare)".to_string()
            } else {
                wt.branch
                    .clone()
                    .unwrap_or_else(|| "(detached)".to_string())
            };
            let status = if worktree_is_dirty(&wt.path) {
                let reason = worktree_dirty_reason(&wt.path)
                    .map(|r| format!(" \x1b[2m({r})\x1b[0m"))
                    .unwrap_or_default();
                format!("\x1b[33mdirty\x1b[0m{reason}")
            } else {
                "\x1b[32mclean\x1b[0m".to_string()
            };
            println!("    {branch_display:<30} {} {}", wt.path, status);
        }
    }

    println!();
    Ok(())
}

/// `cg wt remove` — remove a specific worktree (interactive picker).
pub(crate) fn run_wt_remove(project_root: &str) -> Result<()> {
    let worktrees = list_worktrees(project_root)?;
    let main_root = main_worktree_root(&std::env::current_dir()?)?;

    // Filter out the main worktree and bare worktrees
    let candidates: Vec<_> = worktrees
        .iter()
        .filter(|wt| !wt.bare && best_effort_canonicalize(Path::new(&wt.path)) != main_root)
        .collect();

    if candidates.is_empty() {
        println!("No linked worktrees to remove.");
        return Ok(());
    }

    println!();
    println!("\x1b[36mSelect a worktree to remove:\x1b[0m");
    println!();
    for (i, wt) in candidates.iter().enumerate() {
        let branch_display = wt
            .branch
            .clone()
            .unwrap_or_else(|| "(detached)".to_string());
        let status = if worktree_is_dirty(&wt.path) {
            let reason = worktree_dirty_reason(&wt.path)
                .map(|r| format!(": {r}"))
                .unwrap_or_default();
            format!(" \x1b[33m(dirty{reason})\x1b[0m")
        } else {
            String::new()
        };
        println!("  {}. {} — {}{}", i + 1, branch_display, wt.path, status);
    }
    println!();
    print!("Enter number (or press Enter to cancel): ");
    io::stdout().flush().context("failed to flush prompt")?;

    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("failed to read selection")?;
    let answer = answer.trim();

    if answer.is_empty() {
        println!("Cancelled.");
        return Ok(());
    }

    let index: usize = answer
        .parse::<usize>()
        .context("invalid number")?
        .checked_sub(1)
        .context("number out of range")?;

    if index >= candidates.len() {
        bail!("number out of range");
    }

    let wt = &candidates[index];
    let wt_path = &wt.path;

    // Check for uncommitted changes
    if worktree_is_dirty(wt_path) {
        println!();
        println!("\x1b[33mWarning: worktree has uncommitted changes.\x1b[0m");
        print!("Force remove anyway? [y/N]: ");
        io::stdout().flush().context("failed to flush prompt")?;
        let mut confirm = String::new();
        io::stdin()
            .read_line(&mut confirm)
            .context("failed to read response")?;
        if !matches!(confirm.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Cancelled.");
            return Ok(());
        }
        run_git_checked(project_root, &["worktree", "remove", "--force", wt_path])?;
    } else {
        run_git_checked(project_root, &["worktree", "remove", wt_path])?;
    }

    println!();
    println!("\x1b[32mWorktree removed: {wt_path}\x1b[0m");

    // Optionally delete the branch
    if let Some(branch) = &wt.branch {
        print!("Delete branch \x1b[33m{branch}\x1b[0m? [y/N]: ");
        io::stdout().flush().context("failed to flush prompt")?;
        let mut branch_answer = String::new();
        io::stdin()
            .read_line(&mut branch_answer)
            .context("failed to read response")?;
        if matches!(
            branch_answer.trim().to_ascii_lowercase().as_str(),
            "y" | "yes"
        ) {
            run_git_checked(project_root, &["branch", "-d", branch])?;
            println!("\x1b[32mBranch {branch} deleted.\x1b[0m");
        }
    }

    println!();
    Ok(())
}

/// `cg wt cd` — interactive picker that prints the selected worktree path.
/// Used by shell integration to cd into the selected worktree.
pub(crate) fn run_wt_cd_pwd(project_root: &str) -> Result<()> {
    let worktrees = list_worktrees(project_root)?;

    if worktrees.is_empty() {
        bail!("no worktrees found for this project");
    }

    println!();
    println!("\x1b[36mSelect a worktree to cd into:\x1b[0m");
    println!();
    for (i, wt) in worktrees.iter().enumerate() {
        let branch_display = if wt.bare {
            "(bare)".to_string()
        } else {
            wt.branch
                .clone()
                .unwrap_or_else(|| "(detached)".to_string())
        };
        println!("  {}. {} — {}", i + 1, branch_display, wt.path);
    }
    println!();
    print!("Enter number: ");
    io::stdout().flush().context("failed to flush prompt")?;

    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("failed to read selection")?;
    let index: usize = answer
        .trim()
        .parse::<usize>()
        .context("invalid number")?
        .checked_sub(1)
        .context("number out of range")?;

    if index >= worktrees.len() {
        bail!("number out of range");
    }

    print!("{}", worktrees[index].path);
    Ok(())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Action chosen by the user at the `cg wt end` merge-target prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergeTargetAction {
    Proceed,
    ChangeTarget,
    Cancel,
}

/// Shows a one-line prompt: "Press ENTER to merge into <target>, X to change
/// target, or Ctrl+C to abort."  Reads a single keystroke in raw mode.
fn prompt_merge_target_action(target_branch: &str) -> Result<MergeTargetAction> {
    print!(
        "\x1b[33mPress ENTER to merge into {target_branch}, X to change target, or Ctrl+C to abort\x1b[0m"
    );
    io::stdout().flush().context("failed to flush prompt")?;

    enable_raw_mode().context("failed to enable raw mode")?;
    let result = loop {
        let Event::Key(key) = event::read().context("failed to read merge target input")? else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        match key.code {
            KeyCode::Enter => break MergeTargetAction::Proceed,
            KeyCode::Char('x' | 'X') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                break MergeTargetAction::ChangeTarget;
            }
            KeyCode::Char('c' | 'C') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                break MergeTargetAction::Cancel;
            }
            KeyCode::Esc => break MergeTargetAction::Cancel,
            _ => {}
        }
    };
    disable_raw_mode().context("failed to disable raw mode")?;
    // Move to the next line after the prompt.
    println!();
    Ok(result)
}

fn prompt_worktree_branch_name() -> Result<String> {
    print!("Branch name for the worktree: ");
    io::stdout().flush().context("failed to flush prompt")?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("failed to read branch name")?;
    let trimmed = answer.trim();
    if trimmed.is_empty() {
        bail!("branch name cannot be empty");
    }
    Ok(trimmed.to_string())
}

fn sanitize_branch_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("branch name cannot be empty");
    }
    if name.contains(' ') || name.contains('\\') || name.contains("..") {
        bail!("branch name contains invalid characters");
    }
    Ok(name.to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_worktree_list_basic() {
        let output = "worktree /home/user/project\nHEAD abc123\nbranch refs/heads/main\n\nworktree /home/user/project.wt/feature\nHEAD def456\nbranch refs/heads/feature\n\n";
        let worktrees = parse_worktree_list(output).expect("parse");
        assert_eq!(worktrees.len(), 2);
        assert_eq!(worktrees[0].path, "/home/user/project");
        assert_eq!(worktrees[0].branch.as_deref(), Some("main"));
        assert_eq!(worktrees[0].head, "abc123");
        assert!(!worktrees[0].bare);
        assert!(!worktrees[0].locked);
        assert_eq!(worktrees[1].path, "/home/user/project.wt/feature");
        assert_eq!(worktrees[1].branch.as_deref(), Some("feature"));
        assert_eq!(worktrees[1].head, "def456");
    }

    #[test]
    fn parse_worktree_list_with_bare_and_locked() {
        let output = "worktree /home/user/project\nbare\nHEAD abc123\n\nworktree /home/user/project.wt/locked\nHEAD def456\nbranch refs/heads/locked-branch\nlocked\n\n";
        let worktrees = parse_worktree_list(output).expect("parse");
        assert_eq!(worktrees.len(), 2);
        assert!(worktrees[0].bare);
        assert!(!worktrees[0].locked);
        assert!(!worktrees[1].bare);
        assert!(worktrees[1].locked);
        assert_eq!(worktrees[1].branch.as_deref(), Some("locked-branch"));
    }

    #[test]
    fn parse_worktree_list_detached() {
        let output = "worktree /home/user/project.wt/detached\nHEAD abc123\ndetached\n\n";
        let worktrees = parse_worktree_list(output).expect("parse");
        assert_eq!(worktrees.len(), 1);
        assert_eq!(worktrees[0].branch, None);
        assert_eq!(worktrees[0].head, "abc123");
    }

    #[test]
    fn parse_worktree_list_empty() {
        let worktrees = parse_worktree_list("").expect("parse");
        assert!(worktrees.is_empty());
    }

    #[test]
    fn default_worktree_path_uses_parent_and_basename() {
        let project_root = Path::new("/home/user/myproject");
        let wt_path = default_worktree_path(project_root, "feature-branch", None);
        assert_eq!(
            wt_path,
            PathBuf::from("/home/user/myproject.worktrees/feature-branch")
        );
    }

    #[test]
    fn default_worktree_path_uses_custom_root() {
        let project_root = Path::new("/home/user/myproject");
        let wt_path = default_worktree_path(project_root, "feature", Some("/custom/wt/root"));
        assert_eq!(wt_path, PathBuf::from("/custom/wt/root/feature"));
    }

    #[test]
    fn sanitize_branch_name_rejects_invalid() {
        assert!(sanitize_branch_name("valid-name").is_ok());
        assert!(sanitize_branch_name("name with spaces").is_err());
        assert!(sanitize_branch_name("path\\with\\backslash").is_err());
        assert!(sanitize_branch_name("double..dot").is_err());
        assert!(sanitize_branch_name("").is_err());
        assert!(sanitize_branch_name("   ").is_err());
    }
}
