// Copyright © 2026 ComfyHome™
// All rights reserved.
//
// Licensed under the ComfyGit SA-PS License
//
// For details, see the LICENSE file in the repository root.

//! Branch-name search — `cg br find [ll] <query> [!'<expr>']... [-e]`.

use anyhow::{Result, bail};

use crate::git::{run_git_checked, split_output_lines};

const ANSI_CYAN: &str = "\x1b[36m";
const ANSI_YELLOW: &str = "\x1b[33m";
const ANSI_RESET: &str = "\x1b[0m";

#[derive(Clone, Debug, PartialEq, Eq)]
struct BranchFindArgs {
    local_only: bool,
    exact: bool,
    raw_query: String,
    exclusions: Vec<String>,
}

/// Parse the args that follow `cg br find`.
///
/// - `ll` before the query starts → local-only flag; after the query starts it is a query word
/// - `-e` anywhere → literal matching flag
/// - `!<expr>` anywhere → exclusion expression
/// - all remaining words join with spaces into the query string
///
/// If every positional was consumed as an `ll` flag, the last `ll` becomes the
/// query word so `cg br find ll` searches for the literal phrase "ll".
fn parse_branch_find_args(args: &[String]) -> Result<BranchFindArgs> {
    let mut local_only = false;
    let mut exact = false;
    let mut ll_count = 0usize;
    let mut exclusions = Vec::new();
    let mut words: Vec<&str> = Vec::new();
    let mut query_started = false;

    for arg in args {
        if let Some(exclusion) = arg.strip_prefix('!') {
            exclusions.push(exclusion.to_string());
            continue;
        }
        if arg == "-e" {
            exact = true;
            continue;
        }
        if !query_started && arg == "ll" {
            local_only = true;
            ll_count += 1;
            continue;
        }
        query_started = true;
        words.push(arg);
    }

    if words.is_empty() && local_only {
        words.push("ll");
        if ll_count <= 1 {
            local_only = false;
        }
    }

    if words.is_empty() {
        bail!(
            "missing search phrase — usage: cg br find [ll] '<phrases>' [!'<exclude>'] [-e]\n\
             'a|b' = either phrase, 'a||b' = both phrases, !'x' = exclude, -e = literal match"
        );
    }

    Ok(BranchFindArgs {
        local_only,
        exact,
        raw_query: words.join(" "),
        exclusions,
    })
}

/// Parse a `|`/`||` expression into AND-groups of OR-alternative phrases.
///
/// `a|b||c|d` → `[[a, b], [c, d]]` — every group must have at least one
/// matching alternative. Empty phrases and empty groups are dropped.
/// Unless `exact`, each phrase is normalized (trimmed, whitespace runs → `-`).
fn parse_find_expression(raw: &str, exact: bool) -> Vec<Vec<String>> {
    raw.split("||")
        .map(|group| {
            group
                .split('|')
                .map(|phrase| {
                    if exact {
                        phrase.to_string()
                    } else {
                        phrase.split_whitespace().collect::<Vec<_>>().join("-")
                    }
                })
                .filter(|phrase| !phrase.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|group| !group.is_empty())
        .collect()
}

fn is_token_boundary_byte(byte: u8) -> bool {
    matches!(byte, b'-' | b'_' | b'/' | b'.')
}

/// Case-insensitive token match: the (already lowercased) phrase must appear
/// bounded on both sides by a string edge or one of `-`, `_`, `/`, `.`.
fn token_phrase_match(name_lower: &str, phrase_lower: &str) -> bool {
    name_lower.match_indices(phrase_lower).any(|(index, _)| {
        let before_ok = index == 0 || is_token_boundary_byte(name_lower.as_bytes()[index - 1]);
        let after = index + phrase_lower.len();
        let after_ok =
            after == name_lower.len() || is_token_boundary_byte(name_lower.as_bytes()[after]);
        before_ok && after_ok
    })
}

fn phrase_matches(name: &str, phrase: &str, exact: bool) -> bool {
    if exact {
        name.contains(phrase)
    } else {
        token_phrase_match(&name.to_lowercase(), &phrase.to_lowercase())
    }
}

/// Every AND-group must contain at least one matching alternative.
fn expression_matches(name: &str, expression: &[Vec<String>], exact: bool) -> bool {
    expression.iter().all(|group| {
        group
            .iter()
            .any(|phrase| phrase_matches(name, phrase, exact))
    })
}

pub(crate) fn run_branch_find(repo_root: &str, args: &[String]) -> Result<()> {
    let parsed = parse_branch_find_args(args)?;
    let include = parse_find_expression(&parsed.raw_query, parsed.exact);
    if include.is_empty() {
        bail!("search phrase is empty — nothing to match");
    }
    let exclusions = parsed
        .exclusions
        .iter()
        .map(|raw| parse_find_expression(raw, parsed.exact))
        .filter(|expression| !expression.is_empty())
        .collect::<Vec<_>>();

    let branch_args: &[&str] = if parsed.local_only {
        &["branch"]
    } else {
        &["branch", "-a"]
    };
    let output = run_git_checked(repo_root, branch_args)?;

    let mut locals: Vec<(bool, String)> = Vec::new();
    let mut remotes: Vec<String> = Vec::new();

    for line in split_output_lines(&output) {
        let is_current = line.starts_with("* ");
        let name = line.trim_start_matches('*').trim().to_string();
        if name.is_empty() || name.contains("->") {
            continue;
        }

        let included = expression_matches(&name, &include, parsed.exact);
        let excluded = exclusions
            .iter()
            .any(|expression| expression_matches(&name, expression, parsed.exact));
        if !included || excluded {
            continue;
        }

        if name.starts_with("remotes/") {
            remotes.push(name);
        } else {
            locals.push((is_current, name));
        }
    }

    println!();
    println!("{ANSI_CYAN}local ({}):{ANSI_RESET}", locals.len());
    for (is_current, name) in &locals {
        if *is_current {
            println!("  {ANSI_YELLOW}*{ANSI_RESET} {name}");
        } else {
            println!("    {name}");
        }
    }
    if !parsed.local_only {
        println!();
        println!("{ANSI_CYAN}remotes ({}):{ANSI_RESET}", remotes.len());
        for name in &remotes {
            println!("    {name}");
        }
    }
    println!();

    if locals.is_empty() && remotes.is_empty() {
        println!("no branches matched '{}'", parsed.raw_query);
        println!();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn parse_args_collects_query_flags_and_exclusions() {
        let parsed = parse_branch_find_args(&args(&["ll", "HA|Home Assistant", "!CC", "-e"]))
            .expect("args should parse");
        assert!(parsed.local_only);
        assert!(parsed.exact);
        assert_eq!(parsed.raw_query, "HA|Home Assistant");
        assert_eq!(parsed.exclusions, vec!["CC".to_string()]);
    }

    #[test]
    fn parse_args_joins_unquoted_words() {
        let parsed = parse_branch_find_args(&args(&["HA", "Control"])).expect("should parse");
        assert_eq!(parsed.raw_query, "HA Control");
        assert!(!parsed.local_only);
    }

    #[test]
    fn parse_args_treats_ll_after_query_as_word() {
        let parsed = parse_branch_find_args(&args(&["x", "ll"])).expect("should parse");
        assert!(!parsed.local_only);
        assert_eq!(parsed.raw_query, "x ll");
    }

    #[test]
    fn parse_args_sole_ll_becomes_query() {
        let parsed = parse_branch_find_args(&args(&["ll"])).expect("should parse");
        assert!(!parsed.local_only);
        assert_eq!(parsed.raw_query, "ll");

        let parsed = parse_branch_find_args(&args(&["ll", "ll"])).expect("should parse");
        assert!(parsed.local_only);
        assert_eq!(parsed.raw_query, "ll");
    }

    #[test]
    fn parse_args_allows_flags_and_exclusions_anywhere() {
        let parsed = parse_branch_find_args(&args(&["-e", "!CC", "query"])).expect("should parse");
        assert!(parsed.exact);
        assert_eq!(parsed.exclusions, vec!["CC".to_string()]);
        assert_eq!(parsed.raw_query, "query");

        let parsed =
            parse_branch_find_args(&args(&["query", "-e", "!A", "!B|C"])).expect("should parse");
        assert_eq!(parsed.exclusions, vec!["A".to_string(), "B|C".to_string()]);
    }

    #[test]
    fn parse_args_errors_on_missing_query() {
        assert!(parse_branch_find_args(&args(&[])).is_err());
        assert!(parse_branch_find_args(&args(&["-e"])).is_err());
        assert!(parse_branch_find_args(&args(&["!x"])).is_err());
    }

    #[test]
    fn expression_splits_double_pipe_into_and_groups() {
        let expression = parse_find_expression("a|b||c|d", false);
        assert_eq!(
            expression,
            vec![
                vec!["a".to_string(), "b".to_string()],
                vec!["c".to_string(), "d".to_string()]
            ]
        );
    }

    #[test]
    fn expression_normalizes_whitespace_and_drops_empty_phrases() {
        let expression = parse_find_expression(" Home  Assistant | CAI ||", false);
        assert_eq!(
            expression,
            vec![vec!["Home-Assistant".to_string(), "CAI".to_string()]]
        );
        assert!(parse_find_expression(" || ", false).is_empty());
    }

    #[test]
    fn expression_exact_mode_keeps_phrases_literal() {
        let expression = parse_find_expression("Home Assistant|CAI", true);
        assert_eq!(
            expression,
            vec![vec!["Home Assistant".to_string(), "CAI".to_string()]]
        );
    }

    #[test]
    fn token_match_uses_delimiters() {
        assert!(token_phrase_match("x-ha-y", "ha"));
        assert!(token_phrase_match("ha-bridge", "ha"));
        assert!(token_phrase_match("bridge-ha", "ha"));
        assert!(token_phrase_match("x/ha", "ha"));
        assert!(token_phrase_match("x_ha", "ha"));
        assert!(!token_phrase_match("x-chat-y", "ha"));
        assert!(!token_phrase_match("x-haircut", "ha"));
    }

    #[test]
    fn token_match_treats_dot_as_boundary() {
        assert!(token_phrase_match("v0.16.1-dev", "16.1"));
        assert!(token_phrase_match("0.10.x", "x"));
        assert!(!token_phrase_match("v0.16.1-dev", "6.1"));
    }

    #[test]
    fn phrase_matching_is_case_insensitive_by_default() {
        assert!(phrase_matches("0.90.x--ComfyAI-HA-Control", "ha", false));
        assert!(!phrase_matches("0.90.x--ComfyAI-HA-Control", "ha", true));
        assert!(phrase_matches("0.90.x--ComfyAI-HA-Control", "HA", true));
    }

    #[test]
    fn phrase_matching_exact_is_literal_substring() {
        assert!(phrase_matches("x--Home-Assistant-y", "ssistant", true));
        assert!(!phrase_matches("x--Home-Assistant-y", "ssistant", false));
        assert!(!phrase_matches(
            "x--Home-Assistant-y",
            "home-assistant",
            true
        ));
    }

    #[test]
    fn expression_requires_every_and_group() {
        let expression = parse_find_expression("HA|Home Assistant||CAI|ComfyAI", false);
        assert!(expression_matches(
            "0.90.x--ComfyAI-HA-Control",
            &expression,
            false
        ));
        assert!(!expression_matches(
            "0.86.x--ComfyAI-Home-Direct",
            &expression,
            false
        ));
        assert!(!expression_matches(
            "v0.16.1-dev--HA-alone",
            &expression,
            false
        ));
    }

    #[test]
    fn exclusion_uses_full_grammar() {
        let include = parse_find_expression("HA", false);
        let exclude = parse_find_expression("Electron||CC", false);
        let excluded = |name: &str| {
            expression_matches(name, &include, false) && !expression_matches(name, &exclude, false)
        };
        assert!(excluded("v0.16.1-dev--HA-instance"));
        assert!(!excluded("v0.6.4-dev--HA-Electron-CC"));
        assert!(excluded("v0.6.4-dev--HA-Electron")); // Electron alone is not enough to exclude
    }
}
