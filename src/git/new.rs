// Copyright © 2026 ComfyHome™
// All rights reserved.
//
// Licensed under the ComfyGit License v1.2
//
// For details, see the LICENSE file in the repository root.

//! `cg new` — a GitHub-only shortcut for the most common bump workflow.
//!
//! # Usage
//! ```text
//! cg new                   # interactive two-step wizard
//! cg new <action> [1|2]    # direct: 1=Synced Work (push), 2=Local Work (no push)
//! ```
//!
//! `cg new <action> 1`  ≡  `cg bmp <action> 5`  (BranchCommitAndPush — "Synced Work")
//! `cg new <action> 2`  ≡  `cg bmp <action> 4`  (BranchCommit      — "Local Work")

use std::{
    env,
    io::{self, Write},
};

use anyhow::{Context, Result, bail};
use crossterm::{
    cursor::{MoveToColumn, MoveUp},
    event::{self, Event, KeyCode, KeyEventKind},
    execute, queue,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode},
};

use crate::{
    cli::{best_effort_canonicalize, current_git_repo_root, find_project_for_cwd, run_bump},
    config::ConfigStore,
    git::{current_branch_with_cancel, is_mainline_branch_name},
};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub(crate) fn run_new(action_name: Option<&str>, option_name: Option<&str>) -> Result<()> {
    let config = ConfigStore::locate()?.load()?;
    let cwd =
        best_effort_canonicalize(&env::current_dir().context("failed to read current directory")?);
    let project = find_project_for_cwd(&config.projects, &cwd)?;

    if !project.integration_mode.is_forge_enabled() {
        bail!(
            "cg new is only available for GitHub-enabled projects; \
             this project uses {} mode",
            project.integration_mode.display_name()
        )
    }

    match action_name {
        Some(action) => {
            // Direct form: cg new <action> [1|2]
            let bmp_option = translate_work_option(option_name)?;
            run_bump(action, Some(bmp_option))
        }
        None => {
            // Wizard form
            let repo_root = current_git_repo_root(&cwd)?;
            let current_branch = current_branch_with_cancel(&repo_root, None)?;
            let custom_main_branch = crate::cli::find_repo_custom_main_branch(&repo_root);
            let is_on_main =
                is_mainline_branch_name(&current_branch, custom_main_branch.as_deref());
            let work_option = prompt_work_type_selection(&current_branch)?;
            let action = prompt_branch_action_selection(is_on_main)?;
            match action {
                BranchAction::Bump(bump) => run_bump(bump, Some(work_option)),
                BranchAction::Alt => crate::git::run_new_alt(None),
                BranchAction::Sub => crate::git::run_new_sub(None),
                BranchAction::Off => crate::git::run_new_off(None),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Option translation (1 → "5" / Synced, 2 → "4" / Local)
// ---------------------------------------------------------------------------

fn translate_work_option(option_name: Option<&str>) -> Result<&'static str> {
    match option_name.map(str::trim) {
        None | Some("1") => Ok("5"),
        Some("2") => Ok("4"),
        Some(other) => bail!(
            "cg new option must be 1 (Synced Work) or 2 (Local Work); got '{}'",
            other
        ),
    }
}

// ---------------------------------------------------------------------------
// Step 1 — work type picker
// ---------------------------------------------------------------------------

const WORK_OPTIONS: [(&str, &str); 2] = [
    ("5", "Synced Work"),
    ("4", "Local Work (will not push to remote now)"),
];

fn prompt_work_type_selection(current_branch: &str) -> Result<&'static str> {
    let mut selected = 0usize;
    let mut rendered_lines = 0usize;
    let raw_mode = RawModeGuard::enter()?;

    loop {
        render_work_type_picker(current_branch, selected, &mut rendered_lines)?;

        let Event::Key(key) = event::read().context("failed to read key event")? else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }

        match key.code {
            KeyCode::Esc => {
                drop(raw_mode);
                println!();
                bail!("Cancelled by user")
            }
            KeyCode::Up | KeyCode::BackTab => {
                selected = selected.checked_sub(1).unwrap_or(WORK_OPTIONS.len() - 1);
            }
            KeyCode::Down | KeyCode::Tab => {
                selected = (selected + 1) % WORK_OPTIONS.len();
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                if let Some(index) = c.to_digit(10).and_then(|d| d.checked_sub(1)) {
                    let index = index as usize;
                    if index < WORK_OPTIONS.len() {
                        selected = index;
                    }
                }
            }
            KeyCode::Enter => {
                let result = WORK_OPTIONS[selected].0;
                drop(raw_mode);
                println!();
                return Ok(result);
            }
            _ => {}
        }
    }
}

fn render_work_type_picker(
    current_branch: &str,
    selected: usize,
    rendered_lines: &mut usize,
) -> Result<()> {
    let mut stdout = io::stdout();

    if *rendered_lines > 0 {
        execute!(
            stdout,
            MoveUp(*rendered_lines as u16),
            MoveToColumn(0),
            Clear(ClearType::FromCursorDown)
        )
        .context("failed to redraw work type picker")?;
    }

    // blank
    queue!(stdout, MoveToColumn(0), Print("\r\n")).context("render work type: blank 1")?;
    // hint (DarkGrey)
    queue!(
        stdout,
        MoveToColumn(0),
        SetForegroundColor(Color::DarkGrey),
        Print("Use Up/Down or Tab to select, then press Enter.\r\n"),
        ResetColor
    )
    .context("render work type: hint")?;
    // blank
    queue!(stdout, MoveToColumn(0), Print("\r\n")).context("render work type: blank 2")?;
    // branch line: label plain, branch name Yellow
    queue!(
        stdout,
        MoveToColumn(0),
        Print("Your current active branch: "),
        SetForegroundColor(Color::Yellow),
        Print(current_branch),
        ResetColor,
        Print("\r\n")
    )
    .context("render work type: branch")?;
    // blank
    queue!(stdout, MoveToColumn(0), Print("\r\n")).context("render work type: blank 3")?;
    // question (Cyan)
    queue!(
        stdout,
        MoveToColumn(0),
        SetForegroundColor(Color::Cyan),
        Print("What would you like to do?\r\n"),
        ResetColor
    )
    .context("render work type: question")?;
    // blank
    queue!(stdout, MoveToColumn(0), Print("\r\n")).context("render work type: blank 4")?;
    // options
    for (index, (_, label)) in WORK_OPTIONS.iter().enumerate() {
        let marker = if index == selected { ">" } else { " " };
        let color = if index == selected {
            Color::Yellow
        } else {
            Color::DarkGrey
        };
        queue!(
            stdout,
            MoveToColumn(0),
            SetForegroundColor(color),
            Print(format!("{} {}. {}\r\n", marker, index + 1, label)),
            ResetColor
        )
        .context("render work type: option")?;
    }
    // trailing blank
    queue!(stdout, MoveToColumn(0), Print("\r\n")).context("render work type: trailing blank")?;

    stdout.flush().context("failed to flush work type picker")?;

    // Line count: 1 blank + 1 hint + 1 blank + 1 branch + 1 blank + 1 question + 1 blank
    //             + N options + 1 trailing blank = 8 + N
    *rendered_lines = 8 + WORK_OPTIONS.len();
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 2 — branch action picker (context-aware)
// ---------------------------------------------------------------------------

enum BranchAction {
    Bump(&'static str),
    Alt,
    Sub,
    Off,
}

const MAIN_OPTIONS: [(&str, &str); 3] =
    [("patch", "Patch"), ("minor", "Minor"), ("major", "Major")];

const NON_MAIN_OPTIONS: [(&str, &str); 4] = [
    ("patch", "Patch"),
    ("alt", "alt"),
    ("sub", "SUB"),
    ("off", "OFF"),
];

fn branch_action_options(is_on_main: bool) -> &'static [(&'static str, &'static str)] {
    if is_on_main {
        &MAIN_OPTIONS
    } else {
        &NON_MAIN_OPTIONS
    }
}

fn prompt_branch_action_selection(is_on_main: bool) -> Result<BranchAction> {
    let options = branch_action_options(is_on_main);
    let mut selected = 0usize;
    let mut rendered_lines = 0usize;
    let raw_mode = RawModeGuard::enter()?;

    loop {
        render_branch_action_picker(is_on_main, selected, &mut rendered_lines)?;

        let Event::Key(key) = event::read().context("failed to read key event")? else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }

        match key.code {
            KeyCode::Esc => {
                drop(raw_mode);
                println!();
                bail!("Cancelled by user")
            }
            KeyCode::Up | KeyCode::BackTab => {
                selected = selected.checked_sub(1).unwrap_or(options.len() - 1);
            }
            KeyCode::Down | KeyCode::Tab => {
                selected = (selected + 1) % options.len();
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                if let Some(index) = c.to_digit(10).and_then(|d| d.checked_sub(1)) {
                    let index = index as usize;
                    if index < options.len() {
                        selected = index;
                    }
                }
            }
            KeyCode::Enter => {
                let (key, _) = options[selected];
                drop(raw_mode);
                println!();
                return Ok(match key {
                    "patch" => BranchAction::Bump("patch"),
                    "minor" => BranchAction::Bump("minor"),
                    "major" => BranchAction::Bump("major"),
                    "alt" => BranchAction::Alt,
                    "sub" => BranchAction::Sub,
                    "off" => BranchAction::Off,
                    _ => bail!("unknown branch action '{}'", key),
                });
            }
            _ => {}
        }
    }
}

fn render_branch_action_picker(
    is_on_main: bool,
    selected: usize,
    rendered_lines: &mut usize,
) -> Result<()> {
    let options = branch_action_options(is_on_main);
    let mut stdout = io::stdout();

    if *rendered_lines > 0 {
        execute!(
            stdout,
            MoveUp(*rendered_lines as u16),
            MoveToColumn(0),
            Clear(ClearType::FromCursorDown)
        )
        .context("failed to redraw branch action picker")?;
    }

    // blank
    queue!(stdout, MoveToColumn(0), Print("\r\n")).context("render action: blank 1")?;
    // question (Cyan)
    queue!(
        stdout,
        MoveToColumn(0),
        SetForegroundColor(Color::Cyan),
        Print(if is_on_main {
            "What kind of version bump?\r\n"
        } else {
            "What would you like to do?\r\n"
        }),
        ResetColor
    )
    .context("render action: question")?;
    // blank
    queue!(stdout, MoveToColumn(0), Print("\r\n")).context("render action: blank 2")?;
    // options
    for (index, (_, label)) in options.iter().enumerate() {
        let marker = if index == selected { ">" } else { " " };
        let color = if index == selected {
            Color::Yellow
        } else {
            Color::DarkGrey
        };
        queue!(
            stdout,
            MoveToColumn(0),
            SetForegroundColor(color),
            Print(format!("{} {}. {}\r\n", marker, index + 1, label)),
            ResetColor
        )
        .context("render action: option")?;
    }
    // trailing blank
    queue!(stdout, MoveToColumn(0), Print("\r\n")).context("render action: trailing blank")?;

    stdout
        .flush()
        .context("failed to flush branch action picker")?;

    // Line count: 1 blank + 1 question + 1 blank + N options + 1 trailing blank
    *rendered_lines = 4 + options.len();
    Ok(())
}

// ---------------------------------------------------------------------------
// Raw mode RAII guard (local copy — avoids coupling to cli.rs internals)
// ---------------------------------------------------------------------------

struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("failed to enable raw terminal mode")?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_work_option_1_maps_to_branch_commit_and_push() {
        assert_eq!(translate_work_option(Some("1")).unwrap(), "5");
    }

    #[test]
    fn translate_work_option_2_maps_to_branch_commit() {
        assert_eq!(translate_work_option(Some("2")).unwrap(), "4");
    }

    #[test]
    fn translate_work_option_none_defaults_to_synced() {
        assert_eq!(translate_work_option(None).unwrap(), "5");
    }

    #[test]
    fn translate_work_option_rejects_invalid() {
        assert!(translate_work_option(Some("3")).is_err());
        assert!(translate_work_option(Some("0")).is_err());
        assert!(translate_work_option(Some("x")).is_err());
    }

    #[test]
    fn work_options_have_two_entries() {
        assert_eq!(WORK_OPTIONS.len(), 2);
        assert_eq!(WORK_OPTIONS[0].0, "5"); // Synced Work → BranchCommitAndPush
        assert_eq!(WORK_OPTIONS[1].0, "4"); // Local Work  → BranchCommit
    }

    #[test]
    fn main_options_have_three_entries() {
        assert_eq!(MAIN_OPTIONS.len(), 3);
        let actions: Vec<&str> = MAIN_OPTIONS.iter().map(|(a, _)| *a).collect();
        assert_eq!(actions, ["patch", "minor", "major"]);
    }

    #[test]
    fn non_main_options_have_four_entries() {
        assert_eq!(NON_MAIN_OPTIONS.len(), 4);
        let actions: Vec<&str> = NON_MAIN_OPTIONS.iter().map(|(a, _)| *a).collect();
        assert_eq!(actions, ["patch", "alt", "sub", "off"]);
    }

    #[test]
    fn branch_action_options_returns_main_on_main() {
        let options = branch_action_options(true);
        assert_eq!(options.len(), 3);
        assert_eq!(options[0].0, "patch");
        assert_eq!(options[1].0, "minor");
        assert_eq!(options[2].0, "major");
    }

    #[test]
    fn branch_action_options_returns_non_main_on_dev() {
        let options = branch_action_options(false);
        assert_eq!(options.len(), 4);
        assert_eq!(options[0].0, "patch");
        assert_eq!(options[1].0, "alt");
        assert_eq!(options[2].0, "sub");
        assert_eq!(options[3].0, "off");
    }
}
