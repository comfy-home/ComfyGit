// Copyright © 2026 ComfyHome™
// All rights reserved.
//
// Licensed under the ComfyGit SA-PS License
//
// For details, see the LICENSE file in the repository root.

//! Adjust relative paths in config files when creating/removing worktrees.
//!
//! When a worktree is created at a different depth than the main worktree,
//! relative paths in config files (e.g. `path = "../../../crates/foo"` in
//! `Cargo.toml` or `"file:../../foo"` in `package.json`) that point *outside*
//! the project root become invalid.  This module rewrites those paths so they
//! resolve to the same absolute target from the new file location.
//!
//! Used by `cg wt new` (forward adjustment) and `cg wt end` (reverse
//! restoration after merge).

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};

use crate::git::run_git_checked;

// ---------------------------------------------------------------------------
// Relative path computation (no external dependency)
// ---------------------------------------------------------------------------

/// Computes the relative path from `base` to `target`.
///
/// Mirrors the behaviour of the `pathdiff` crate: returns `None` when the two
/// paths cannot be made relative (e.g. they live on different Windows drives).
/// Both paths should be canonical/absolute for meaningful results.
fn diff_paths(target: &Path, base: &Path) -> Option<PathBuf> {
    let mut target_iter = target.components();
    let mut base_iter = base.components();

    // Skip common prefix.
    let mut target_remaining: Vec<Component> = Vec::new();
    let mut base_remaining: Vec<Component> = Vec::new();

    loop {
        match (target_iter.next(), base_iter.next()) {
            (None, None) => break,
            (Some(t), None) => {
                target_remaining.push(t);
                target_remaining.extend(target_iter);
                break;
            }
            (None, Some(b)) => {
                base_remaining.push(b);
                base_remaining.extend(base_iter);
                break;
            }
            (Some(t), Some(b)) => {
                if t == b {
                    continue;
                }
                target_remaining.push(t);
                target_remaining.extend(target_iter);
                base_remaining.push(b);
                base_remaining.extend(base_iter);
                break;
            }
        }
    }

    // Reject if either side has a `PrefixComponent` (Windows drive letter) and
    // they differ — they cannot be made relative.
    if let (Some(Component::Prefix(_)), Some(Component::Prefix(_))) =
        (target_remaining.first(), base_remaining.first())
    {
        return None;
    }

    let mut result = PathBuf::new();
    for comp in &base_remaining {
        if matches!(comp, Component::CurDir) {
            continue;
        }
        // Any non-`..` base component requires going up one level.
        if !matches!(comp, Component::ParentDir) {
            result.push("..");
        }
    }
    for comp in &target_remaining {
        match comp {
            Component::CurDir => {}
            Component::Prefix(p) => result.push(p.as_os_str()),
            Component::RootDir => result.push(Component::RootDir.as_os_str()),
            Component::Normal(s) => result.push(s),
            Component::ParentDir => result.push(".."),
        }
    }

    if result.as_os_str().is_empty() {
        result.push(".");
    }
    Some(result)
}

// ---------------------------------------------------------------------------
// Config file discovery
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigFileKind {
    CargoToml,
    PyprojectToml,
    PackageJson,
    TsconfigJson,
}

impl ConfigFileKind {
    fn from_path(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?;
        match name {
            "Cargo.toml" => Some(Self::CargoToml),
            "pyproject.toml" => Some(Self::PyprojectToml),
            "package.json" => Some(Self::PackageJson),
            "tsconfig.json" => Some(Self::TsconfigJson),
            _ => None,
        }
    }

    fn is_toml(self) -> bool {
        matches!(self, Self::CargoToml | Self::PyprojectToml)
    }
}

/// Returns the tracked config files in `repo_root` (relative paths).
fn list_config_files(repo_root: &str) -> Result<Vec<PathBuf>> {
    let output = run_git_checked(repo_root, &["ls-files", "-z"])?;
    let mut files = Vec::new();
    for entry in output.split('\0') {
        if entry.is_empty() {
            continue;
        }
        let path = Path::new(entry);
        if ConfigFileKind::from_path(path).is_some() {
            files.push(path.to_path_buf());
        }
    }
    Ok(files)
}

// ---------------------------------------------------------------------------
// Path adjustment core
// ---------------------------------------------------------------------------

/// Lexically normalizes a path (collapses `.` and `..` without touching the
/// filesystem).  Used as a fallback when `fs::canonicalize` fails because the
/// target does not exist.
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut stack: Vec<Component> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                match stack.last() {
                    Some(Component::Normal(_)) => {
                        stack.pop();
                    }
                    Some(Component::RootDir) | None => {}
                    // A `..` on top of `..` stays `..`.
                    _ => stack.push(comp),
                }
            }
            other => stack.push(other),
        }
    }
    let mut result = PathBuf::new();
    for comp in stack {
        match comp {
            Component::Prefix(p) => result.push(p.as_os_str()),
            Component::RootDir => result.push(Component::RootDir.as_os_str()),
            Component::Normal(s) => result.push(s),
            Component::CurDir => {}
            Component::ParentDir => result.push(".."),
        }
    }
    if result.as_os_str().is_empty() {
        result.push(".");
    }
    result
}

/// Recomputes a single relative path string.
///
/// `current_value` is relative to `resolve_dir`.  It is resolved to an absolute
/// target, then recomputed relative to `recompute_dir`.  Returns `None` when
/// the path does not start with `../` (so absolute or in-tree paths are left
/// untouched) or when it cannot be made relative.
fn recompute_path(
    current_value: &str,
    resolve_dir: &Path,
    recompute_dir: &Path,
    project_root: &Path,
) -> Option<String> {
    // Only adjust upward-relative paths.  In-tree relative paths (e.g.
    // `src/main.rs`) and absolute paths are left alone.
    if !current_value.starts_with("../") {
        return None;
    }
    let absolute = resolve_dir.join(current_value);
    let absolute = fs::canonicalize(&absolute).unwrap_or_else(|_| normalize_lexical(&absolute));
    // Only adjust paths that escape the project root.
    if !absolute.starts_with(project_root)
        && let Some(rel) = diff_paths(&absolute, recompute_dir)
    {
        return Some(rel.to_string_lossy().into_owned());
    }
    None
}

// ---------------------------------------------------------------------------
// TOML adjustment (Cargo.toml, pyproject.toml)
// ---------------------------------------------------------------------------

fn adjust_toml_paths(
    file_path: &Path,
    resolve_dir: &Path,
    recompute_dir: &Path,
    project_root: &Path,
) -> Result<usize> {
    let content = fs::read_to_string(file_path)
        .with_context(|| format!("failed to read {}", file_path.display()))?;
    let mut document = content
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("failed to parse {}", file_path.display()))?;

    let mut count = 0usize;
    visit_toml_tables(
        document.as_item_mut(),
        resolve_dir,
        recompute_dir,
        project_root,
        &mut count,
    );

    if count > 0 {
        fs::write(file_path, document.to_string())
            .with_context(|| format!("failed to write {}", file_path.display()))?;
    }
    Ok(count)
}

/// Recursively visits all tables in a TOML document, adjusting `path` keys.
fn visit_toml_tables(
    item: &mut toml_edit::Item,
    resolve_dir: &Path,
    recompute_dir: &Path,
    project_root: &Path,
    count: &mut usize,
) {
    if let Some(table) = item.as_table_like_mut() {
        for (_, value) in table.iter_mut() {
            if let Some(path_str) = value.as_value().and_then(toml_edit::Value::as_str)
                && let Some(new_path) =
                    recompute_path(path_str, resolve_dir, recompute_dir, project_root)
            {
                *value = toml_edit::value(new_path);
                *count += 1;
            }
            // Recurse into nested tables / inline tables / arrays of tables.
            visit_toml_tables(value, resolve_dir, recompute_dir, project_root, count);
        }
    }
}

// ---------------------------------------------------------------------------
// JSON adjustment (package.json, tsconfig.json)
// ---------------------------------------------------------------------------

/// Adjusts JSON config files using parse-to-identify + string-replace, which
/// preserves the original formatting exactly.
fn adjust_json_paths(
    file_path: &Path,
    resolve_dir: &Path,
    recompute_dir: &Path,
    project_root: &Path,
    kind: ConfigFileKind,
) -> Result<usize> {
    let content = fs::read_to_string(file_path)
        .with_context(|| format!("failed to read {}", file_path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", file_path.display()))?;

    // Collect (old, new) string replacements.
    let mut replacements: Vec<(String, String)> = Vec::new();
    collect_json_replacements(
        &value,
        resolve_dir,
        recompute_dir,
        project_root,
        kind,
        &mut replacements,
    );

    if replacements.is_empty() {
        return Ok(0);
    }

    // Apply replacements as exact `"old"` -> `"new"` substitutions on the raw
    // content.  This preserves indentation, key order, comments (JSONC), etc.
    let mut updated = content;
    let mut count = 0usize;
    for (old, new) in &replacements {
        let old_quoted = format!("\"{}\"", old.replace('\\', "\\\\").replace('"', "\\\""));
        let new_quoted = format!("\"{}\"", new.replace('\\', "\\\\").replace('"', "\\\""));
        if updated.contains(&old_quoted) {
            updated = updated.replace(&old_quoted, &new_quoted);
            count += 1;
        }
    }

    if count > 0 {
        fs::write(file_path, updated)
            .with_context(|| format!("failed to write {}", file_path.display()))?;
    }
    Ok(count)
}

fn collect_json_replacements(
    value: &serde_json::Value,
    resolve_dir: &Path,
    recompute_dir: &Path,
    project_root: &Path,
    kind: ConfigFileKind,
    out: &mut Vec<(String, String)>,
) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                match child {
                    serde_json::Value::String(s) => {
                        if let Some(new) = json_path_replacement(
                            key,
                            s,
                            resolve_dir,
                            recompute_dir,
                            project_root,
                            kind,
                        ) {
                            out.push((s.clone(), new));
                        }
                    }
                    _ => collect_json_replacements(
                        child,
                        resolve_dir,
                        recompute_dir,
                        project_root,
                        kind,
                        out,
                    ),
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_json_replacements(
                    item,
                    resolve_dir,
                    recompute_dir,
                    project_root,
                    kind,
                    out,
                );
            }
        }
        _ => {}
    }
}

/// Decides whether a JSON string value under `key` is a relative path that
/// should be adjusted, and returns the recomputed value if so.
fn json_path_replacement(
    key: &str,
    value: &str,
    resolve_dir: &Path,
    recompute_dir: &Path,
    project_root: &Path,
    kind: ConfigFileKind,
) -> Option<String> {
    match kind {
        ConfigFileKind::PackageJson => {
            // `file:` and `link:` specifiers carry relative paths.
            for prefix in ["file:", "link:"] {
                if let Some(rest) = value.strip_prefix(prefix) {
                    let trimmed = rest.trim_start();
                    if trimmed.starts_with("../") {
                        let new_rel =
                            recompute_path(trimmed, resolve_dir, recompute_dir, project_root)?;
                        return Some(format!("{prefix}{new_rel}"));
                    }
                }
            }
            None
        }
        ConfigFileKind::TsconfigJson => {
            // `extends` and `references[].path` hold relative paths.
            if (key == "extends" || key == "path") && value.starts_with("../") {
                return recompute_path(value, resolve_dir, recompute_dir, project_root);
            }
            None
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Adjusts relative paths in all tracked config files inside the worktree at
/// `worktree_root`, recomputing them relative to the worktree location.
///
/// `project_root` is the main worktree root (used to decide which paths escape
/// the project and therefore need adjustment).
pub(crate) fn adjust_paths_for_worktree(
    project_root: &Path,
    worktree_root: &Path,
) -> Result<usize> {
    let worktree_root_str = worktree_root.display().to_string();
    let files = list_config_files(&worktree_root_str)?;
    let mut total = 0usize;
    for rel in &files {
        let worktree_file = worktree_root.join(rel);
        let main_file = project_root.join(rel);
        let resolve_dir = main_file
            .parent()
            .context("config file has no parent directory")?;
        let recompute_dir = worktree_file
            .parent()
            .context("config file has no parent directory")?;
        let kind = ConfigFileKind::from_path(rel).context("unreachable: kind already checked")?;
        let count = if kind.is_toml() {
            adjust_toml_paths(&worktree_file, resolve_dir, recompute_dir, project_root)
        } else {
            adjust_json_paths(
                &worktree_file,
                resolve_dir,
                recompute_dir,
                project_root,
                kind,
            )
        };
        match count {
            Ok(c) => total += c,
            Err(e) => {
                eprintln!(
                    "\x1b[33mwarning: failed to adjust {}: {}\x1b[0m",
                    worktree_file.display(),
                    e
                );
            }
        }
    }
    Ok(total)
}

/// Restores relative paths in all tracked config files inside the main
/// worktree at `project_root`, recomputing them relative to the main location.
///
/// Called after merging a worktree branch back into main — the merged files
/// contain worktree-relative paths that must be restored to main-relative.
pub(crate) fn restore_paths_after_merge(
    project_root: &Path,
    worktree_root: &Path,
) -> Result<usize> {
    let project_root_str = project_root.display().to_string();
    let files = list_config_files(&project_root_str)?;
    let mut total = 0usize;
    for rel in &files {
        let main_file = project_root.join(rel);
        let worktree_file = worktree_root.join(rel);
        let resolve_dir = worktree_file
            .parent()
            .context("config file has no parent directory")?;
        let recompute_dir = main_file
            .parent()
            .context("config file has no parent directory")?;
        let kind = ConfigFileKind::from_path(rel).context("unreachable: kind already checked")?;
        let count = if kind.is_toml() {
            adjust_toml_paths(&main_file, resolve_dir, recompute_dir, project_root)
        } else {
            adjust_json_paths(&main_file, resolve_dir, recompute_dir, project_root, kind)
        };
        match count {
            Ok(c) => total += c,
            Err(e) => {
                eprintln!(
                    "\x1b[33mwarning: failed to restore {}: {}\x1b[0m",
                    main_file.display(),
                    e
                );
            }
        }
    }
    Ok(total)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_diff_paths_basic() {
        let base = Path::new("/home/user/project");
        let target = Path::new("/home/user/crates/foo");
        let rel = diff_paths(target, base).unwrap();
        assert_eq!(rel, PathBuf::from("../crates/foo"));
    }

    #[test]
    fn test_diff_paths_same_dir() {
        let base = Path::new("/home/user/project");
        let target = Path::new("/home/user/project");
        let rel = diff_paths(target, base).unwrap();
        assert_eq!(rel, PathBuf::from("."));
    }

    #[test]
    fn test_diff_paths_sibling() {
        let base = Path::new("/home/user/project");
        let target = Path::new("/home/user/sibling");
        let rel = diff_paths(target, base).unwrap();
        assert_eq!(rel, PathBuf::from("../sibling"));
    }

    #[test]
    fn test_diff_paths_deeper_target() {
        let base = Path::new("/home/user/project");
        let target = Path::new("/home/user/project/sub/deep");
        let rel = diff_paths(target, base).unwrap();
        assert_eq!(rel, PathBuf::from("sub/deep"));
    }

    #[test]
    fn test_diff_paths_deeper_base() {
        let base = Path::new("/home/user/project/sub/deep");
        let target = Path::new("/home/user/crates/foo");
        let rel = diff_paths(target, base).unwrap();
        assert_eq!(rel, PathBuf::from("../../../crates/foo"));
    }

    #[test]
    fn test_normalize_lexical_collapses_dots() {
        let path = Path::new("/home/user/project/../crates/foo");
        let normalized = normalize_lexical(path);
        assert_eq!(normalized, PathBuf::from("/home/user/crates/foo"));
    }

    #[test]
    fn test_recompute_path_adds_level() {
        // Main file at /home/user/project, worktree at /home/user/project.wt/branch
        let main_dir = Path::new("/home/user/project");
        let worktree_dir = Path::new("/home/user/project.wt/branch");
        let project_root = Path::new("/home/user/project");
        // Path `../../../crates/foo` from main dir -> /home/user/crates/foo
        // From worktree dir -> ../../crates/foo (one more `..`)
        let result =
            recompute_path("../../../crates/foo", main_dir, worktree_dir, project_root).unwrap();
        assert_eq!(result, "../../../../crates/foo");
    }

    #[test]
    fn test_recompute_path_skips_internal() {
        let main_dir = Path::new("/home/user/project");
        let worktree_dir = Path::new("/home/user/project.wt/branch");
        let project_root = Path::new("/home/user/project");
        // `../sibling` from main dir resolves to /home/user/sibling — outside
        // project root, so it should be adjusted.
        let result = recompute_path("../sibling", main_dir, worktree_dir, project_root);
        assert!(result.is_some(), "external path should be adjusted");
        // A path inside the project root should not be adjusted.
        let result = recompute_path("src/main.rs", main_dir, worktree_dir, project_root);
        assert!(result.is_none(), "in-tree path should not be adjusted");
    }

    #[test]
    fn test_recompute_path_skips_non_relative() {
        let main_dir = Path::new("/home/user/project");
        let worktree_dir = Path::new("/home/user/project.wt/branch");
        let project_root = Path::new("/home/user/project");
        assert!(recompute_path("src/main.rs", main_dir, worktree_dir, project_root).is_none());
        assert!(recompute_path("/abs/path", main_dir, worktree_dir, project_root).is_none());
    }

    #[test]
    fn test_adjust_toml_adds_level() {
        let dir = tempdir().unwrap();
        let project_root = dir.join("project");
        let worktree_root = dir.join("project.wt").join("branch");
        fs::create_dir_all(&project_root).unwrap();
        fs::create_dir_all(&worktree_root).unwrap();

        let cargo = r#"[dependencies]
comfy-foo = { path = "../../../crates/foo" }
comfy-bar = { path = "src/bar" }
version-only = "1.0"
"#;
        let worktree_file = worktree_root.join("Cargo.toml");
        fs::write(&worktree_file, cargo).unwrap();

        // Create the absolute target so canonicalize works.
        fs::create_dir_all(dir.join("crates/foo")).unwrap();

        let main_file = project_root.join("Cargo.toml");
        let resolve_dir = main_file.parent().unwrap();
        let recompute_dir = worktree_file.parent().unwrap();
        let count =
            adjust_toml_paths(&worktree_file, resolve_dir, recompute_dir, &project_root).unwrap();
        assert_eq!(count, 1);

        let updated = fs::read_to_string(&worktree_file).unwrap();
        assert!(updated.contains("path = \"../../../../crates/foo\""));
        assert!(updated.contains("path = \"src/bar\""));
        assert!(updated.contains("version-only = \"1.0\""));
    }

    #[test]
    fn test_adjust_toml_preserves_comments() {
        let dir = tempdir().unwrap();
        let project_root = dir.join("project");
        let worktree_root = dir.join("project.wt").join("branch");
        fs::create_dir_all(&project_root).unwrap();
        fs::create_dir_all(&worktree_root).unwrap();
        fs::create_dir_all(dir.join("crates/foo")).unwrap();

        let cargo = r#"# Top comment
[dependencies]
# inline comment
comfy-foo = { path = "../../../crates/foo" } # trailing
"#;
        let worktree_file = worktree_root.join("Cargo.toml");
        fs::write(&worktree_file, cargo).unwrap();

        let main_file = project_root.join("Cargo.toml");
        let resolve_dir = main_file.parent().unwrap();
        let recompute_dir = worktree_file.parent().unwrap();
        let _ =
            adjust_toml_paths(&worktree_file, resolve_dir, recompute_dir, &project_root).unwrap();

        let updated = fs::read_to_string(&worktree_file).unwrap();
        assert!(updated.contains("# Top comment"));
        assert!(updated.contains("# inline comment"));
        assert!(updated.contains("# trailing"));
    }

    #[test]
    fn test_adjust_package_json_file_prefix() {
        let dir = tempdir().unwrap();
        let project_root = dir.join("project");
        let worktree_root = dir.join("project.wt").join("branch");
        fs::create_dir_all(&project_root).unwrap();
        fs::create_dir_all(&worktree_root).unwrap();
        fs::create_dir_all(dir.join("packages/foo")).unwrap();

        let pkg = r#"{
  "dependencies": {
    "foo": "file:../../packages/foo",
    "bar": "^1.0.0"
  }
}
"#;
        let worktree_file = worktree_root.join("package.json");
        fs::write(&worktree_file, pkg).unwrap();

        let main_file = project_root.join("package.json");
        let resolve_dir = main_file.parent().unwrap();
        let recompute_dir = worktree_file.parent().unwrap();
        let count = adjust_json_paths(
            &worktree_file,
            resolve_dir,
            recompute_dir,
            &project_root,
            ConfigFileKind::PackageJson,
        )
        .unwrap();
        assert_eq!(count, 1);

        let updated = fs::read_to_string(&worktree_file).unwrap();
        assert!(updated.contains("file:../../../packages/foo"));
        assert!(updated.contains("\"bar\": \"^1.0.0\""));
    }

    #[test]
    fn test_adjust_package_json_link_prefix() {
        let dir = tempdir().unwrap();
        let project_root = dir.join("project");
        let worktree_root = dir.join("project.wt").join("branch");
        fs::create_dir_all(&project_root).unwrap();
        fs::create_dir_all(&worktree_root).unwrap();
        fs::create_dir_all(dir.join("packages/foo")).unwrap();

        let pkg = r#"{
  "dependencies": {
    "foo": "link:../../packages/foo"
  }
}
"#;
        let worktree_file = worktree_root.join("package.json");
        fs::write(&worktree_file, pkg).unwrap();

        let main_file = project_root.join("package.json");
        let resolve_dir = main_file.parent().unwrap();
        let recompute_dir = worktree_file.parent().unwrap();
        let count = adjust_json_paths(
            &worktree_file,
            resolve_dir,
            recompute_dir,
            &project_root,
            ConfigFileKind::PackageJson,
        )
        .unwrap();
        assert_eq!(count, 1);

        let updated = fs::read_to_string(&worktree_file).unwrap();
        assert!(updated.contains("link:../../../packages/foo"));
    }

    #[test]
    fn test_adjust_tsconfig_extends_and_references() {
        let dir = tempdir().unwrap();
        let project_root = dir.join("project");
        let worktree_root = dir.join("project.wt").join("branch");
        fs::create_dir_all(&project_root).unwrap();
        fs::create_dir_all(&worktree_root).unwrap();
        fs::create_dir_all(dir.join("shared/tsconfig-base")).unwrap();
        fs::create_dir_all(dir.join("shared/utils")).unwrap();

        let tsconfig = r#"{
  "extends": "../../shared/tsconfig-base",
  "references": [
    { "path": "../../shared/utils" }
  ]
}
"#;
        let worktree_file = worktree_root.join("tsconfig.json");
        fs::write(&worktree_file, tsconfig).unwrap();

        let main_file = project_root.join("tsconfig.json");
        let resolve_dir = main_file.parent().unwrap();
        let recompute_dir = worktree_file.parent().unwrap();
        let count = adjust_json_paths(
            &worktree_file,
            resolve_dir,
            recompute_dir,
            &project_root,
            ConfigFileKind::TsconfigJson,
        )
        .unwrap();
        assert_eq!(count, 2);

        let updated = fs::read_to_string(&worktree_file).unwrap();
        assert!(updated.contains("\"extends\": \"../../../shared/tsconfig-base\""));
        assert!(updated.contains("\"path\": \"../../../shared/utils\""));
    }

    #[test]
    fn test_restore_reverses_adjustment() {
        let dir = tempdir().unwrap();
        let project_root = dir.join("project");
        let worktree_root = dir.join("project.wt").join("branch");
        fs::create_dir_all(&project_root).unwrap();
        fs::create_dir_all(&worktree_root).unwrap();
        fs::create_dir_all(dir.join("crates/foo")).unwrap();

        let original = r#"[dependencies]
comfy-foo = { path = "../../../crates/foo" }
"#;
        let worktree_file = worktree_root.join("Cargo.toml");
        fs::write(&worktree_file, original).unwrap();

        // Forward: main -> worktree
        let main_file = project_root.join("Cargo.toml");
        let resolve_dir = main_file.parent().unwrap();
        let recompute_dir = worktree_file.parent().unwrap();
        let _ =
            adjust_toml_paths(&worktree_file, resolve_dir, recompute_dir, &project_root).unwrap();
        let adjusted = fs::read_to_string(&worktree_file).unwrap();
        assert!(adjusted.contains("path = \"../../../../crates/foo\""));

        // Simulate merge: copy worktree file into main.
        fs::write(&main_file, &adjusted).unwrap();

        // Reverse: worktree -> main
        let resolve_dir = worktree_file.parent().unwrap();
        let recompute_dir = main_file.parent().unwrap();
        let _ = adjust_toml_paths(&main_file, resolve_dir, recompute_dir, &project_root).unwrap();
        let restored = fs::read_to_string(&main_file).unwrap();
        assert_eq!(restored, original);
    }

    // Helper: tempdir without pulling in a crate.
    fn tempdir() -> Result<std::path::PathBuf> {
        let dir = std::env::temp_dir().join(format!(
            "cg-wt-paths-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }
}
