// Copyright © 2026 ComfyHome™
// All rights reserved.
//
// Licensed under the ComfyGit License v1.2
//
// For details, see the LICENSE file in the repository root.

//! Deviation branches — `cg new alt`, `cg new sub`, and `cg new off`.
//!
//! Alternative (`alt`) branches use `-alt` marker: `v0.1.5-dev-alt1`, `v0.1.5-dev-alt2A`.
//! Sub-development (`SUB`) branches use `-SUB` marker: `v0.1.5-dev-SUB1`, `v0.1.5-dev-SUB1A`.
//! Offline (`OFF`) branches use `-OFF` marker: `v0.1.5-dev-OFF1`, `v0.1.5-dev-OFF1A`.
//!
//! All follow the same numeric → letter pattern and can be nested cross-type:
//! `v0.1.5-dev-SUB1-alt1`, `v0.1.5-dev-alt1-OFF1A`, etc.

// ---------------------------------------------------------------------------
// Deviation kind descriptor
// ---------------------------------------------------------------------------

struct DeviationKind {
    marker: &'static str,
    label: &'static str,
    command: &'static str,
}

const ALT_KIND: DeviationKind = DeviationKind {
    marker: "-alt",
    label: "alt",
    command: "alt",
};

const SUB_KIND: DeviationKind = DeviationKind {
    marker: "-SUB",
    label: "SUB",
    command: "sub",
};

const OFF_KIND: DeviationKind = DeviationKind {
    marker: "-OFF",
    label: "OFF",
    command: "off",
};

const DEVIATION_MARKERS: [&str; 3] = ["-alt", "-SUB", "-OFF"];

pub(crate) fn find_rightmost_marker(base: &str) -> Option<(usize, &'static str)> {
    DEVIATION_MARKERS
        .iter()
        .filter_map(|&marker| base.rfind(marker).map(|pos| (pos, marker)))
        .max_by_key(|(pos, _)| *pos)
}

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
    cli::{
        best_effort_canonicalize, current_git_repo_root, find_project_for_cwd, find_scope_for_cwd,
    },
    config::{ConfigStore, ProjectType},
    git::{
        BranchNameOption, collect_all_branch_git_scope_contexts, create_branch_and_switch,
        current_branch_with_cancel, custom_branch_name_option_with_preview,
        fixed_branch_name_option_with_value, publish_branch_with_upstream,
        run_git_checked_with_cancel, specific_suffix_branch_name_option,
    },
    workflow::targets::resolve_project_target_paths,
};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub(crate) fn run_new_alt(option_name: Option<&str>) -> Result<()> {
    run_new_deviation(option_name, &ALT_KIND)
}

pub(crate) fn run_new_sub(option_name: Option<&str>) -> Result<()> {
    run_new_deviation(option_name, &SUB_KIND)
}

pub(crate) fn run_new_off(option_name: Option<&str>) -> Result<()> {
    run_new_deviation(option_name, &OFF_KIND)
}

fn run_new_deviation(option_name: Option<&str>, kind: &DeviationKind) -> Result<()> {
    let config = ConfigStore::locate()?.load()?;
    let cwd =
        best_effort_canonicalize(&env::current_dir().context("failed to read current directory")?);
    let project = find_project_for_cwd(&config.projects, &cwd)?;
    let resolved_project = resolve_project_target_paths(project);
    let repo_root = current_git_repo_root(&cwd)?;
    let current_branch = current_branch_with_cancel(&repo_root, None)?;

    validate_deviation_creation_source(&current_branch, kind)?;

    let synced_work = match option_name.map(str::trim) {
        None => prompt_deviation_work_type_selection(&current_branch, kind)?,
        Some("1") => true,
        Some("2") => false,
        Some(other) => {
            bail!(
                "cg new {} option must be 1 (Synced Work) or 2 (Local Work); got '{}'",
                kind.command,
                other
            )
        }
    };

    if synced_work && !project.integration_mode.is_forge_enabled() {
        bail!(
            "cg new {} 1 (Synced Work) is only available for GitHub-enabled projects; \
             use option 2 for local-only branches",
            kind.command
        );
    }

    if !prompt_deviation_position_continue(&project.name, &current_branch)? {
        bail!("Cancelled by user");
    }

    let existing_branches = list_local_branch_names(&repo_root)?;
    let branch_options =
        suggest_deviation_branch_name_options(&current_branch, &existing_branches, kind)?;
    let branch_name = prompt_deviation_branch_name(&branch_options, kind)?;

    create_branch_and_switch(&repo_root, &branch_name)?;

    if synced_work {
        let remote_spec = resolve_remote_spec_for_repo(&resolved_project, &cwd, &repo_root)?;
        publish_branch_with_upstream(&repo_root, &branch_name, Some(&remote_spec), None)?;
        println!(
            "Created, switched to, and published branch '{}' to remote.",
            branch_name
        );
    } else {
        println!("Created and switched to branch '{}'.", branch_name);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Merge parent resolution (used by cg br end / cg pr)
// ---------------------------------------------------------------------------

/// When `current_branch` is an alt branch, returns the branch it should merge into.
///
/// When `existing_branches` is provided, prefers the first candidate that already exists
/// locally (e.g. falls back from `v0.9.1-dev--or-this-way` to `v0.9.1-dev` when only the
/// latter exists). When empty, returns the preferred (first) candidate.
#[cfg(test)]
fn alt_merge_parent_branch(current_branch: &str, existing_branches: &[String]) -> Option<String> {
    deviation_merge_parent_branch(current_branch, existing_branches)
}

/// Other local alt branches exploring alternatives on the same dev branch.
#[cfg(test)]
fn alt_sibling_branch_names(current_branch: &str, existing_branches: &[String]) -> Vec<String> {
    deviation_sibling_branch_names(current_branch, existing_branches)
}

// ---------------------------------------------------------------------------
// Generalized deviation API (handles both alt and SUB, including cross-type nesting)
// ---------------------------------------------------------------------------

/// Dev branch that any deviation branch (alt or SUB, possibly nested) originates from.
/// `v0.1.5-dev-alt2` → `v0.1.5-dev`, `v0.1.5-dev-SUB1-alt1` → `v0.1.5-dev`.
pub(crate) fn deviation_lineage_dev_base(branch: &str) -> Option<String> {
    let (base, _) = split_specific_suffix(branch);
    strip_to_dev_base(&base)
}

/// True if the branch is any kind of deviation (alt or SUB, including nested).
pub(crate) fn is_deviation_branch(branch: &str) -> bool {
    deviation_lineage_dev_base(branch).is_some()
}

/// When `current_branch` is any deviation branch, returns the branch it should merge into.
/// Handles alt, SUB, and cross-type nesting (e.g. `v0.1.5-dev-SUB1-alt1` → `v0.1.5-dev-SUB1`).
pub(crate) fn deviation_merge_parent_branch(
    current_branch: &str,
    existing_branches: &[String],
) -> Option<String> {
    let candidates = deviation_merge_parent_candidates(current_branch, existing_branches)?;
    pick_existing_branch_candidate(&candidates, existing_branches)
}

fn deviation_merge_parent_candidates(
    current_branch: &str,
    existing_branches: &[String],
) -> Option<Vec<String>> {
    let (base, specific_suffix) = split_specific_suffix(current_branch);
    let (pos, marker) = find_rightmost_marker(&base)?;

    let suffix = &base[pos + marker.len()..];
    let digit_len = suffix.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digit_len == 0 {
        return None;
    }
    let rest = &suffix[digit_len..];
    if !rest.is_empty() && !rest.chars().all(|ch| ch.is_ascii_uppercase()) {
        return None;
    }

    let parent_base = &base[..pos];
    if !parent_base.contains("-dev") {
        return None;
    }

    let mut candidates = Vec::new();

    if !rest.is_empty() {
        // Letter deviation: parent is the numeric deviation base
        let numeric_base = &base[..pos + marker.len() + digit_len];
        push_parent_branch_candidates(
            &mut candidates,
            numeric_base,
            specific_suffix.as_deref(),
            existing_branches,
        );
        // Also try the dev parent of the numeric base
        if let Some(dev_parent) = parse_numeric_deviation_dev_parent(numeric_base, marker) {
            push_parent_branch_candidates(
                &mut candidates,
                &dev_parent,
                specific_suffix.as_deref(),
                existing_branches,
            );
        }
    } else {
        // Numeric deviation: parent is the branch before this marker
        push_parent_branch_candidates(
            &mut candidates,
            parent_base,
            specific_suffix.as_deref(),
            existing_branches,
        );
    }

    dedup_branch_candidates(candidates)
}

/// Other local deviation branches (alt or SUB) sharing the same dev base.
pub(crate) fn deviation_sibling_branch_names(
    current_branch: &str,
    existing_branches: &[String],
) -> Vec<String> {
    let Some(dev_base) = deviation_lineage_dev_base(current_branch) else {
        return Vec::new();
    };

    let mut siblings = existing_branches
        .iter()
        .filter(|branch| {
            !branch.eq_ignore_ascii_case(current_branch)
                && is_deviation_branch(branch)
                && deviation_lineage_dev_base(branch).as_deref() == Some(dev_base.as_str())
        })
        .cloned()
        .collect::<Vec<_>>();
    siblings.sort_by_cached_key(|branch| branch.to_ascii_lowercase());
    siblings
}

fn push_parent_branch_candidates(
    candidates: &mut Vec<String>,
    parent_base: &str,
    specific_suffix: Option<&str>,
    existing_branches: &[String],
) {
    if let Some(suffix) = specific_suffix.filter(|value| !value.is_empty()) {
        insert_candidate(
            candidates,
            join_with_specific_suffix(parent_base, Some(suffix)),
        );
    }

    let mut matching_specific_branches = existing_branches
        .iter()
        .filter(|branch| is_parent_specific_branch(branch, parent_base))
        .cloned()
        .collect::<Vec<_>>();
    matching_specific_branches.sort_by_cached_key(|branch| branch.to_ascii_lowercase());
    for branch in matching_specific_branches {
        insert_candidate(candidates, branch);
    }

    insert_candidate(candidates, parent_base.to_string());
}

fn is_parent_specific_branch(branch: &str, parent_base: &str) -> bool {
    if branch.len() <= parent_base.len() + 2 {
        return false;
    }

    branch[..parent_base.len()].eq_ignore_ascii_case(parent_base)
        && branch[parent_base.len()..].starts_with("--")
}

fn insert_candidate(candidates: &mut Vec<String>, candidate: String) {
    if !candidates
        .iter()
        .any(|existing| existing.eq_ignore_ascii_case(&candidate))
    {
        candidates.push(candidate);
    }
}

fn dedup_branch_candidates(candidates: Vec<String>) -> Option<Vec<String>> {
    if candidates.is_empty() {
        None
    } else {
        Some(candidates)
    }
}

fn pick_existing_branch_candidate(
    candidates: &[String],
    existing_branches: &[String],
) -> Option<String> {
    if existing_branches.is_empty() {
        return candidates.first().cloned();
    }

    candidates
        .iter()
        .find(|candidate| branch_exists(existing_branches, candidate))
        .cloned()
}

fn branch_exists(existing_branches: &[String], candidate: &str) -> bool {
    existing_branches
        .iter()
        .any(|branch| branch.eq_ignore_ascii_case(candidate))
}

// ---------------------------------------------------------------------------
// Branch naming
// ---------------------------------------------------------------------------

fn validate_deviation_creation_source(current_branch: &str, kind: &DeviationKind) -> Result<()> {
    let (base, _) = split_specific_suffix(current_branch);
    if !base.contains("-dev") {
        bail!(
            "cg new {} can only be run from a dev branch or an existing deviation branch; \
             current branch is '{}'",
            kind.command,
            current_branch
        )
    }
    Ok(())
}

fn suggest_deviation_branch_name_options(
    current_branch: &str,
    existing_branches: &[String],
    kind: &DeviationKind,
) -> Result<Vec<BranchNameOption>> {
    let (base, specific_suffix) = split_specific_suffix(current_branch);
    let next_base = match classify_deviation_source(&base, kind)? {
        DeviationSourceKind::DevBranch => {
            let dev_base = base;
            let next_number = next_numeric_deviation_number(&dev_base, existing_branches, kind);
            format!("{}{}{}", dev_base, kind.marker, next_number)
        }
        DeviationSourceKind::NumericDeviation => {
            let numeric_base = base;
            let next_letter = next_letter_deviation_suffix(&numeric_base, existing_branches, kind)?;
            format!("{}{}", numeric_base, next_letter)
        }
        DeviationSourceKind::LetterDeviation => {
            let (numeric_base, _) =
                parse_letter_deviation_base(&base, kind.marker).ok_or_else(|| {
                    anyhow::anyhow!("invalid letter {} branch '{}'", kind.label, current_branch)
                })?;
            let next_letter = next_letter_deviation_suffix(&numeric_base, existing_branches, kind)?;
            format!("{}{}", numeric_base, next_letter)
        }
    };

    let preview = join_with_specific_suffix(&next_base, specific_suffix.as_deref());
    Ok(vec![
        fixed_branch_name_option_with_value(preview.clone(), preview.clone()),
        specific_suffix_branch_name_option(next_base),
        custom_branch_name_option_with_preview("custom (not recommended)"),
    ])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviationSourceKind {
    DevBranch,
    NumericDeviation,
    LetterDeviation,
}

fn classify_deviation_source(base: &str, kind: &DeviationKind) -> Result<DeviationSourceKind> {
    if parse_letter_deviation_base(base, kind.marker).is_some() {
        return Ok(DeviationSourceKind::LetterDeviation);
    }

    if parse_numeric_deviation_dev_parent(base, kind.marker).is_some() {
        return Ok(DeviationSourceKind::NumericDeviation);
    }

    if base.contains("-dev") {
        return Ok(DeviationSourceKind::DevBranch);
    }

    bail!(
        "cg new {} can only be run from a dev branch or an existing {} branch; \
         current branch base is '{}'",
        kind.command,
        kind.label,
        base
    )
}

fn parse_numeric_deviation_dev_parent(base: &str, marker: &str) -> Option<String> {
    let marker_index = base.rfind(marker)?;
    let dev_parent = base[..marker_index].trim();
    if !dev_parent.contains("-dev") {
        return None;
    }

    let suffix = &base[marker_index + marker.len()..];
    if suffix.is_empty() || !suffix.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }

    Some(dev_parent.to_string())
}

fn parse_letter_deviation_base(base: &str, marker: &str) -> Option<(String, char)> {
    let marker_index = base.rfind(marker)?;
    let dev_parent = base[..marker_index].trim();
    if !dev_parent.contains("-dev") {
        return None;
    }

    let suffix = &base[marker_index + marker.len()..];
    let digit_len = suffix.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digit_len == 0 {
        return None;
    }

    let letters = &suffix[digit_len..];
    if letters.is_empty() || !letters.chars().all(|ch| ch.is_ascii_uppercase()) {
        return None;
    }

    let numeric_base = base[..marker_index + marker.len() + digit_len].to_string();
    let letter = letters.chars().last()?;
    Some((numeric_base, letter))
}

fn strip_to_dev_base(base: &str) -> Option<String> {
    let (pos, marker) = find_rightmost_marker(base)?;
    let suffix = &base[pos + marker.len()..];
    let digit_len = suffix.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digit_len == 0 {
        return None;
    }
    let rest = &suffix[digit_len..];
    if !rest.is_empty() && !rest.chars().all(|ch| ch.is_ascii_uppercase()) {
        return None;
    }
    let parent = &base[..pos];
    if !parent.contains("-dev") {
        return None;
    }
    if find_rightmost_marker(parent).is_none() {
        return Some(parent.to_string());
    }
    strip_to_dev_base(parent)
}

fn next_numeric_deviation_number(
    dev_base: &str,
    existing_branches: &[String],
    kind: &DeviationKind,
) -> u32 {
    let max_number = existing_branches
        .iter()
        .filter_map(|branch| numeric_deviation_number_for_dev(branch, dev_base, kind))
        .max()
        .unwrap_or(0);
    max_number + 1
}

fn numeric_deviation_number_for_dev(
    branch: &str,
    dev_base: &str,
    kind: &DeviationKind,
) -> Option<u32> {
    let (base, _) = split_specific_suffix(branch);
    let suffix = base.strip_prefix(&format!("{dev_base}{}", kind.marker))?;
    if suffix.is_empty() {
        return None;
    }

    let digit_len = suffix.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digit_len == 0 {
        return None;
    }

    let rest = &suffix[digit_len..];
    if !rest.is_empty() {
        return None;
    }

    suffix[..digit_len].parse().ok()
}

fn next_letter_deviation_suffix(
    numeric_base: &str,
    existing_branches: &[String],
    kind: &DeviationKind,
) -> Result<char> {
    let max_letter = existing_branches
        .iter()
        .filter_map(|branch| letter_deviation_suffix_for_numeric(branch, numeric_base, kind))
        .max();

    match max_letter {
        None => Ok('A'),
        Some(letter) if letter < 'Z' => Ok(((letter as u8) + 1) as char),
        Some('Z') => bail!(
            "all letter {} branches A-Z already exist for '{}'",
            kind.label,
            numeric_base
        ),
        Some(_) => bail!(
            "invalid letter {} suffix state for '{}'",
            kind.label,
            numeric_base
        ),
    }
}

fn letter_deviation_suffix_for_numeric(
    branch: &str,
    numeric_base: &str,
    _kind: &DeviationKind,
) -> Option<char> {
    let (base, _) = split_specific_suffix(branch);
    if !base.starts_with(numeric_base) {
        return None;
    }

    let letters = base.strip_prefix(numeric_base)?;
    if letters.is_empty() || !letters.chars().all(|ch| ch.is_ascii_uppercase()) {
        return None;
    }

    letters.chars().last()
}

fn split_specific_suffix(branch: &str) -> (String, Option<String>) {
    if let Some((base, suffix)) = branch.split_once("--") {
        (base.to_string(), Some(suffix.to_string()))
    } else {
        (branch.to_string(), None)
    }
}

fn join_with_specific_suffix(base: &str, specific_suffix: Option<&str>) -> String {
    match specific_suffix.filter(|suffix| !suffix.is_empty()) {
        Some(suffix) => format!("{base}--{suffix}"),
        None => base.to_string(),
    }
}

fn list_local_branch_names(repo_root: &str) -> Result<Vec<String>> {
    let output = run_git_checked_with_cancel(
        repo_root,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
        None,
    )?;
    let mut branches = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    branches.sort_by_cached_key(|branch| branch.to_ascii_lowercase());
    branches.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
    Ok(branches)
}

fn resolve_remote_spec_for_repo(
    project: &crate::config::ProjectConfig,
    cwd: &std::path::Path,
    repo_root: &str,
) -> Result<String> {
    let contexts = collect_all_branch_git_scope_contexts(project)?;
    let scope_index = if project.project_type == ProjectType::AllInOne || project.unified_versioning
    {
        0
    } else {
        find_scope_for_cwd(project, project, cwd)?
    };
    let canonical_repo_root = best_effort_canonicalize(std::path::Path::new(repo_root));
    let context = contexts.get(scope_index).or_else(|| {
        contexts.iter().find(|context| {
            best_effort_canonicalize(std::path::Path::new(&context.repo_root))
                == canonical_repo_root
        })
    });

    context
        .and_then(|context| context.remote_spec.clone())
        .filter(|remote| !remote.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("no remote is configured for this project"))
}

// ---------------------------------------------------------------------------
// Wizards
// ---------------------------------------------------------------------------

const DEVIATION_WORK_OPTIONS: [(&str, &str); 2] = [
    ("1", "Synced Work"),
    ("2", "Local Work (will not push to remote now)"),
];

fn prompt_deviation_work_type_selection(
    current_branch: &str,
    _kind: &DeviationKind,
) -> Result<bool> {
    let mut selected = 0usize;
    let mut rendered_lines = 0usize;
    let raw_mode = RawModeGuard::enter()?;

    loop {
        render_deviation_work_type_picker(current_branch, selected, &mut rendered_lines)?;

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
                selected = selected
                    .checked_sub(1)
                    .unwrap_or(DEVIATION_WORK_OPTIONS.len() - 1);
            }
            KeyCode::Down | KeyCode::Tab => {
                selected = (selected + 1) % DEVIATION_WORK_OPTIONS.len();
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                if let Some(index) = c.to_digit(10).and_then(|d| d.checked_sub(1)) {
                    let index = index as usize;
                    if index < DEVIATION_WORK_OPTIONS.len() {
                        selected = index;
                    }
                }
            }
            KeyCode::Enter => {
                let synced = DEVIATION_WORK_OPTIONS[selected].0 == "1";
                drop(raw_mode);
                println!();
                return Ok(synced);
            }
            _ => {}
        }
    }
}

fn render_deviation_work_type_picker(
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

    queue!(stdout, MoveToColumn(0), Print("\r\n")).context("render work type: blank 1")?;
    queue!(
        stdout,
        MoveToColumn(0),
        SetForegroundColor(Color::DarkGrey),
        Print("Use Up/Down or Tab to select, then press Enter.\r\n"),
        ResetColor
    )
    .context("render work type: hint")?;
    queue!(stdout, MoveToColumn(0), Print("\r\n")).context("render work type: blank 2")?;
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
    queue!(stdout, MoveToColumn(0), Print("\r\n")).context("render work type: blank 3")?;
    queue!(
        stdout,
        MoveToColumn(0),
        SetForegroundColor(Color::Cyan),
        Print("What would you like to do?\r\n"),
        ResetColor
    )
    .context("render work type: question")?;
    queue!(stdout, MoveToColumn(0), Print("\r\n")).context("render work type: blank 4")?;

    for (index, (_, label)) in DEVIATION_WORK_OPTIONS.iter().enumerate() {
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
    queue!(stdout, MoveToColumn(0), Print("\r\n")).context("render work type: trailing blank")?;
    stdout.flush().context("failed to flush work type picker")?;
    *rendered_lines = 8 + DEVIATION_WORK_OPTIONS.len();
    Ok(())
}

fn prompt_deviation_position_continue(project_name: &str, current_branch: &str) -> Result<bool> {
    println!();
    println!("You are here:");
    println!("  {} -> {}", project_name, current_branch);
    println!();
    prompt_confirm_default_yes("Press ENTER or Y to continue; N to cancel: ")
}

fn prompt_confirm_default_yes(prompt: &str) -> Result<bool> {
    loop {
        print!("{prompt}");
        io::stdout().flush().context("failed to flush prompt")?;

        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .context("failed to read response")?;

        match answer.trim().to_lowercase().as_str() {
            "" | "y" => return Ok(true),
            "n" => return Ok(false),
            _ => println!("Please answer Y or N."),
        }
    }
}

fn prompt_deviation_branch_name(
    options: &[BranchNameOption],
    kind: &DeviationKind,
) -> Result<String> {
    if options.is_empty() {
        bail!("{} branch name options are unavailable", kind.label)
    }

    let mut selected = 0usize;
    let mut rendered_lines = 0usize;
    let raw_mode = RawModeGuard::enter()?;

    loop {
        render_deviation_branch_name_picker(options, selected, &mut rendered_lines, kind)?;

        let Event::Key(key) = event::read().context("failed to read branch name selection")? else {
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
            KeyCode::Char(character) => {
                if let Some(index) = digit_to_index(character) {
                    selected = index.min(options.len().saturating_sub(1));
                }
            }
            KeyCode::Enter => {
                let option = options[selected].clone();
                drop(raw_mode);
                println!();
                let input = if option.requires_input() {
                    Some(prompt_deviation_branch_name_input(option.input_label())?)
                } else {
                    None
                };
                return option.resolve_name(input.as_deref());
            }
            _ => {}
        }
    }
}

fn render_deviation_branch_name_picker(
    options: &[BranchNameOption],
    selected: usize,
    rendered_lines: &mut usize,
    kind: &DeviationKind,
) -> Result<()> {
    let mut stdout = io::stdout();

    if *rendered_lines > 0 {
        execute!(
            stdout,
            MoveUp(*rendered_lines as u16),
            MoveToColumn(0),
            Clear(ClearType::FromCursorDown)
        )
        .context("failed to redraw branch name picker")?;
    }

    queue!(stdout, MoveToColumn(0), Print("\r\n")).context("render name: blank 1")?;
    queue!(
        stdout,
        MoveToColumn(0),
        Print("----------------------------------------\r\n")
    )
    .context("render name: separator")?;
    queue!(stdout, MoveToColumn(0), Print("\r\n")).context("render name: blank 2")?;
    queue!(
        stdout,
        MoveToColumn(0),
        SetForegroundColor(Color::Cyan),
        Print(format!(
            "Please choose a name for the {} branch:\r\n",
            kind.label
        )),
        ResetColor
    )
    .context("render name: question")?;
    queue!(stdout, MoveToColumn(0), Print("\r\n")).context("render name: blank 3")?;
    queue!(
        stdout,
        MoveToColumn(0),
        SetForegroundColor(Color::DarkGrey),
        Print("Use Up/Down or Tab to select, then press Enter.\r\n"),
        ResetColor
    )
    .context("render name: hint")?;
    queue!(stdout, MoveToColumn(0), Print("\r\n")).context("render name: blank 4")?;

    for (index, option) in options.iter().enumerate() {
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
            Print(format!(
                "{} {}. {}\r\n",
                marker,
                index + 1,
                option.preview()
            )),
            ResetColor
        )
        .context("render name: option")?;
    }
    queue!(stdout, MoveToColumn(0), Print("\r\n")).context("render name: trailing blank")?;
    stdout
        .flush()
        .context("failed to flush branch name picker")?;
    *rendered_lines = 6 + options.len();
    Ok(())
}

fn prompt_deviation_branch_name_input(label: &str) -> Result<String> {
    print!("{label}: ");
    io::stdout()
        .flush()
        .context("failed to flush branch name input prompt")?;

    let mut branch_name = String::new();
    io::stdin()
        .read_line(&mut branch_name)
        .context("failed to read branch name input")?;

    Ok(branch_name.trim().to_string())
}

fn digit_to_index(character: char) -> Option<usize> {
    character
        .to_digit(10)
        .and_then(|digit| digit.checked_sub(1))
        .map(|digit| digit as usize)
}

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
    fn alt_merge_parent_for_numeric_alt_returns_dev_branch() {
        let existing = vec!["v0.1.5-dev".to_string()];
        assert_eq!(
            alt_merge_parent_branch("v0.1.5-dev-alt2", &existing).as_deref(),
            Some("v0.1.5-dev")
        );
        assert_eq!(
            alt_merge_parent_branch("v0.1.5-dev-alt2--menu", &existing).as_deref(),
            Some("v0.1.5-dev")
        );
    }

    #[test]
    fn alt_merge_parent_prefers_matching_dev_specific_branch_when_present() {
        let existing = vec!["v0.1.5-dev".to_string(), "v0.1.5-dev--menu".to_string()];
        assert_eq!(
            alt_merge_parent_branch("v0.1.5-dev-alt2--menu", &existing).as_deref(),
            Some("v0.1.5-dev--menu")
        );
    }

    #[test]
    fn alt_merge_parent_uses_only_existing_dev_specific_branch() {
        let existing = vec![
            "v0.9.4-dev--bubbly-talk-clipboard-abilities".to_string(),
            "v0.9.4-dev-alt2--bubbly-talk-clipboard-abilities".to_string(),
        ];
        assert_eq!(
            alt_merge_parent_branch(
                "v0.9.4-dev-alt2--bubbly-talk-clipboard-abilities",
                &existing
            )
            .as_deref(),
            Some("v0.9.4-dev--bubbly-talk-clipboard-abilities")
        );
    }

    #[test]
    fn alt_merge_parent_discovers_dev_specific_when_alt_has_no_suffix() {
        let existing = vec![
            "v0.9.4-dev--bubbly-talk-clipboard-abilities".to_string(),
            "v0.9.4-dev-alt2".to_string(),
        ];
        assert_eq!(
            alt_merge_parent_branch("v0.9.4-dev-alt2", &existing).as_deref(),
            Some("v0.9.4-dev--bubbly-talk-clipboard-abilities")
        );
    }

    #[test]
    fn alt_merge_parent_for_letter_alt_returns_numeric_alt() {
        let existing = vec!["v0.1.5-dev-alt2".to_string(), "v0.1.5-dev".to_string()];
        assert_eq!(
            alt_merge_parent_branch("v0.1.5-dev-alt2B", &existing).as_deref(),
            Some("v0.1.5-dev-alt2")
        );
        assert_eq!(
            alt_merge_parent_branch("v0.1.5-dev-alt2B--menu", &existing).as_deref(),
            Some("v0.1.5-dev-alt2")
        );
    }

    #[test]
    fn alt_merge_parent_returns_none_for_non_alt_branch() {
        assert_eq!(alt_merge_parent_branch("v0.1.5-dev", &[]), None);
        assert_eq!(alt_merge_parent_branch("main", &[]), None);
    }

    #[test]
    fn alt_sibling_branch_names_lists_other_alts_on_same_dev_branch() {
        let existing = vec![
            "v0.9.1-dev".to_string(),
            "v0.9.1-dev-alt1".to_string(),
            "v0.9.1-dev-alt2--or-this-way".to_string(),
            "v0.9.1-dev-alt3".to_string(),
        ];
        let siblings = alt_sibling_branch_names("v0.9.1-dev-alt2--or-this-way", &existing);
        assert_eq!(
            siblings,
            vec!["v0.9.1-dev-alt1".to_string(), "v0.9.1-dev-alt3".to_string(),]
        );
    }

    #[test]
    fn next_numeric_alt_number_skips_existing_branches() {
        let existing = vec![
            "v0.1.5-dev-alt1".to_string(),
            "v0.1.5-dev-alt2".to_string(),
            "v0.1.5-dev-alt2A".to_string(),
        ];
        assert_eq!(
            next_numeric_deviation_number("v0.1.5-dev", &existing, &ALT_KIND),
            3
        );
    }

    #[test]
    fn next_letter_alt_suffix_starts_at_a_then_increments() {
        let existing = vec!["v0.1.5-dev-alt2A".to_string()];
        assert_eq!(
            next_letter_deviation_suffix("v0.1.5-dev-alt2", &existing, &ALT_KIND)
                .expect("next letter"),
            'B'
        );
    }

    #[test]
    fn suggest_alt_branch_name_options_from_dev_branch() {
        let options = suggest_deviation_branch_name_options("v0.1.5-dev", &[], &ALT_KIND)
            .expect("suggest options");
        assert_eq!(options.len(), 3);
        assert_eq!(options[0].preview(), "v0.1.5-dev-alt1");
    }

    #[test]
    fn suggest_alt_branch_name_options_from_numeric_alt() {
        let options = suggest_deviation_branch_name_options("v0.1.5-dev-alt2", &[], &ALT_KIND)
            .expect("suggest options");
        assert_eq!(options[0].preview(), "v0.1.5-dev-alt2A");
    }

    #[test]
    fn sub_merge_parent_branch_returns_dev_for_numeric_sub() {
        let existing = vec!["v0.1.5-dev".to_string()];
        assert_eq!(
            deviation_merge_parent_branch("v0.1.5-dev-SUB1", &existing).as_deref(),
            Some("v0.1.5-dev")
        );
    }

    #[test]
    fn sub_merge_parent_branch_returns_numeric_sub_for_letter_sub() {
        let existing = vec!["v0.1.5-dev-SUB1".to_string(), "v0.1.5-dev".to_string()];
        assert_eq!(
            deviation_merge_parent_branch("v0.1.5-dev-SUB1A", &existing).as_deref(),
            Some("v0.1.5-dev-SUB1")
        );
    }

    #[test]
    fn sub_sibling_branch_names_lists_other_subs_on_same_dev_branch() {
        let existing = vec![
            "v0.9.1-dev".to_string(),
            "v0.9.1-dev-SUB1".to_string(),
            "v0.9.1-dev-SUB2".to_string(),
            "v0.9.1-dev-SUB3".to_string(),
        ];
        let siblings = deviation_sibling_branch_names("v0.9.1-dev-SUB2", &existing);
        assert_eq!(
            siblings,
            vec!["v0.9.1-dev-SUB1".to_string(), "v0.9.1-dev-SUB3".to_string(),]
        );
    }

    #[test]
    fn is_deviation_branch_recognizes_both_alt_and_sub() {
        assert!(is_deviation_branch("v0.1.5-dev-alt1"));
        assert!(is_deviation_branch("v0.1.5-dev-alt2B"));
        assert!(is_deviation_branch("v0.1.5-dev-SUB1"));
        assert!(is_deviation_branch("v0.1.5-dev-SUB1A"));
        assert!(!is_deviation_branch("v0.1.5-dev"));
        assert!(!is_deviation_branch("main"));
    }

    #[test]
    fn deviation_merge_parent_cross_type_alt_from_sub() {
        let existing = vec!["v0.1.5-dev".to_string(), "v0.1.5-dev-SUB1".to_string()];
        assert_eq!(
            deviation_merge_parent_branch("v0.1.5-dev-SUB1-alt1", &existing).as_deref(),
            Some("v0.1.5-dev-SUB1")
        );
    }

    #[test]
    fn deviation_merge_parent_cross_type_sub_from_alt() {
        let existing = vec!["v0.1.5-dev".to_string(), "v0.1.5-dev-alt1".to_string()];
        assert_eq!(
            deviation_merge_parent_branch("v0.1.5-dev-alt1-SUB1", &existing).as_deref(),
            Some("v0.1.5-dev-alt1")
        );
    }

    #[test]
    fn deviation_merge_parent_deeply_nested() {
        let existing = vec![
            "v0.1.5-dev".to_string(),
            "v0.1.5-dev-SUB1".to_string(),
            "v0.1.5-dev-SUB1-alt1".to_string(),
        ];
        assert_eq!(
            deviation_merge_parent_branch("v0.1.5-dev-SUB1-alt1-SUB1", &existing).as_deref(),
            Some("v0.1.5-dev-SUB1-alt1")
        );
    }

    #[test]
    fn deviation_lineage_dev_base_for_nested_sub() {
        assert_eq!(
            deviation_lineage_dev_base("v0.1.5-dev-SUB1-alt1-SUB1").as_deref(),
            Some("v0.1.5-dev")
        );
        assert_eq!(
            deviation_lineage_dev_base("v0.1.5-dev-alt1-SUB1A").as_deref(),
            Some("v0.1.5-dev")
        );
    }

    #[test]
    fn suggest_sub_branch_name_options_from_dev_branch() {
        let options = suggest_deviation_branch_name_options("v0.1.5-dev", &[], &SUB_KIND)
            .expect("suggest options");
        assert_eq!(options.len(), 3);
        assert_eq!(options[0].preview(), "v0.1.5-dev-SUB1");
    }

    #[test]
    fn suggest_sub_branch_name_options_from_numeric_sub() {
        let options = suggest_deviation_branch_name_options("v0.1.5-dev-SUB2", &[], &SUB_KIND)
            .expect("suggest options");
        assert_eq!(options[0].preview(), "v0.1.5-dev-SUB2A");
    }

    #[test]
    fn suggest_sub_branch_name_options_from_letter_sub() {
        let existing = vec!["v0.1.5-dev-SUB2A".to_string()];
        let options =
            suggest_deviation_branch_name_options("v0.1.5-dev-SUB2A", &existing, &SUB_KIND)
                .expect("suggest options");
        assert_eq!(options[0].preview(), "v0.1.5-dev-SUB2B");
    }

    #[test]
    fn next_numeric_sub_number_skips_existing_branches() {
        let existing = vec!["v0.1.5-dev-SUB1".to_string(), "v0.1.5-dev-SUB2".to_string()];
        assert_eq!(
            next_numeric_deviation_number("v0.1.5-dev", &existing, &SUB_KIND),
            3
        );
    }

    #[test]
    fn off_merge_parent_branch_returns_dev_for_numeric_off() {
        let existing = vec!["v0.1.5-dev".to_string()];
        assert_eq!(
            deviation_merge_parent_branch("v0.1.5-dev-OFF1", &existing).as_deref(),
            Some("v0.1.5-dev")
        );
    }

    #[test]
    fn off_merge_parent_branch_returns_numeric_off_for_letter_off() {
        let existing = vec!["v0.1.5-dev-OFF1".to_string(), "v0.1.5-dev".to_string()];
        assert_eq!(
            deviation_merge_parent_branch("v0.1.5-dev-OFF1A", &existing).as_deref(),
            Some("v0.1.5-dev-OFF1")
        );
    }

    #[test]
    fn off_sibling_branch_names_lists_other_offs_on_same_dev_branch() {
        let existing = vec![
            "v0.9.1-dev".to_string(),
            "v0.9.1-dev-OFF1".to_string(),
            "v0.9.1-dev-OFF2".to_string(),
            "v0.9.1-dev-OFF3".to_string(),
        ];
        let siblings = deviation_sibling_branch_names("v0.9.1-dev-OFF2", &existing);
        assert_eq!(
            siblings,
            vec!["v0.9.1-dev-OFF1".to_string(), "v0.9.1-dev-OFF3".to_string(),]
        );
    }

    #[test]
    fn is_deviation_branch_recognizes_off() {
        assert!(is_deviation_branch("v0.1.5-dev-OFF1"));
        assert!(is_deviation_branch("v0.1.5-dev-OFF1A"));
        assert!(!is_deviation_branch("v0.1.5-dev"));
    }

    #[test]
    fn deviation_merge_parent_cross_type_off_from_sub() {
        let existing = vec!["v0.1.5-dev".to_string(), "v0.1.5-dev-SUB1".to_string()];
        assert_eq!(
            deviation_merge_parent_branch("v0.1.5-dev-SUB1-OFF1", &existing).as_deref(),
            Some("v0.1.5-dev-SUB1")
        );
    }

    #[test]
    fn deviation_lineage_dev_base_for_off() {
        assert_eq!(
            deviation_lineage_dev_base("v0.1.5-dev-OFF1-alt1-OFF1").as_deref(),
            Some("v0.1.5-dev")
        );
    }

    #[test]
    fn suggest_off_branch_name_options_from_dev_branch() {
        let options = suggest_deviation_branch_name_options("v0.1.5-dev", &[], &OFF_KIND)
            .expect("suggest options");
        assert_eq!(options.len(), 3);
        assert_eq!(options[0].preview(), "v0.1.5-dev-OFF1");
    }

    #[test]
    fn suggest_off_branch_name_options_from_numeric_off() {
        let options = suggest_deviation_branch_name_options("v0.1.5-dev-OFF2", &[], &OFF_KIND)
            .expect("suggest options");
        assert_eq!(options[0].preview(), "v0.1.5-dev-OFF2A");
    }

    #[test]
    fn next_numeric_off_number_skips_existing_branches() {
        let existing = vec!["v0.1.5-dev-OFF1".to_string(), "v0.1.5-dev-OFF2".to_string()];
        assert_eq!(
            next_numeric_deviation_number("v0.1.5-dev", &existing, &OFF_KIND),
            3
        );
    }

    #[test]
    fn resolve_remote_spec_for_repo_prefers_deepest_matching_scope() {
        use std::path::Path;

        use crate::config::{
            BranchConfig, BranchScopeKind, ChangelogSettings, IntegrationMode, ProjectConfig,
            ProjectType, RepoConfig,
        };
        use crate::workflow::versioning::VersionScheme;

        let project = ProjectConfig {
            name: "demo".to_string(),
            alias: String::new(),
            project_type: ProjectType::Branched,
            integration_mode: IntegrationMode::GitHubEnabled,
            unified_versioning: false,
            version_scheme: VersionScheme::SemVer,
            changelog: ChangelogSettings::default(),
            release_now: crate::config::ReleaseNowSettings::default(),
            tile_info: crate::config::TileInfoSettings::default(),
            targets: Vec::new(),
            branches: vec![
                BranchConfig {
                    name: "core".to_string(),
                    label: "Core".to_string(),
                    scope_kind: BranchScopeKind::Module,
                    repo: Some(RepoConfig {
                        local_root: "/tmp/comfyhome".to_string(),
                        remote_url: Some(
                            "git@gitlab.com:comfyhome/x-project/comfyhome.git".to_string(),
                        ),
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
                    targets: Vec::new(),
                    advanced_alias: Default::default(),
                },
                BranchConfig {
                    name: "comfycast".to_string(),
                    label: "ComfyCast".to_string(),
                    scope_kind: BranchScopeKind::Module,
                    repo: Some(RepoConfig {
                        local_root: "/tmp/comfyhome/apps/modules/comfycast".to_string(),
                        remote_url: Some("git@github.com:comfyhome/comfycast.git".to_string()),
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
                    targets: Vec::new(),
                    advanced_alias: Default::default(),
                },
            ],
            repo: None,
            ..Default::default()
        };

        let cwd = Path::new("/tmp/comfyhome/apps/modules/comfycast");
        let remote =
            resolve_remote_spec_for_repo(&project, cwd, "/tmp/comfyhome/apps/modules/comfycast")
                .expect("resolve scope remote");

        assert_eq!(remote, "git@github.com:comfyhome/comfycast.git");
    }
}
