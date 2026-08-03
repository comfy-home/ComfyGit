// Copyright © 2026 ComfyHome™
// All rights reserved.
//
// Licensed under the ComfyGit License v1.2
//
// For details, see the LICENSE file in the repository root.

use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BumpAction {
    Auto,
    Major,
    Minor,
    Patch,
}

impl BumpAction {
    pub fn display_name(self) -> &'static str {
        match self {
            BumpAction::Auto => "Auto",
            BumpAction::Major => "Major",
            BumpAction::Minor => "Minor",
            BumpAction::Patch => "Patch",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VersionScheme {
    #[default]
    SemVer,
    CalVerYearMonthMicro,
    CalVerShortYearMonthMicro,
    CalVerYearMonthDayMicro,
    HybridYearMinorPatch,
    HybridYearPatch,
}

impl VersionScheme {
    pub const SEMVER_ACTIONS: [BumpAction; 3] =
        [BumpAction::Major, BumpAction::Minor, BumpAction::Patch];
    pub const CALVER_ACTIONS: [BumpAction; 1] = [BumpAction::Auto];
    pub const HYBRID_MINOR_PATCH_ACTIONS: [BumpAction; 2] = [BumpAction::Minor, BumpAction::Patch];
    pub const HYBRID_PATCH_ACTIONS: [BumpAction; 1] = [BumpAction::Patch];

    pub const ALL: [VersionScheme; 6] = [
        VersionScheme::SemVer,
        VersionScheme::CalVerYearMonthMicro,
        VersionScheme::CalVerShortYearMonthMicro,
        VersionScheme::CalVerYearMonthDayMicro,
        VersionScheme::HybridYearMinorPatch,
        VersionScheme::HybridYearPatch,
    ];

    pub fn display_name(self) -> &'static str {
        match self {
            VersionScheme::SemVer => "SemVer",
            VersionScheme::CalVerYearMonthMicro => "CalVer YYYY.MM.Micro",
            VersionScheme::CalVerShortYearMonthMicro => "CalVer YY.MM.Micro",
            VersionScheme::CalVerYearMonthDayMicro => "CalVer YYYY.MM.DD.Micro",
            VersionScheme::HybridYearMinorPatch => "Hybrid YYYY.MINOR.PATCH",
            VersionScheme::HybridYearPatch => "Hybrid YYYY.PATCH",
        }
    }

    pub fn example(self) -> &'static str {
        match self {
            VersionScheme::SemVer => "1.2.3",
            VersionScheme::CalVerYearMonthMicro => "2026.04.7",
            VersionScheme::CalVerShortYearMonthMicro => "26.04.7",
            VersionScheme::CalVerYearMonthDayMicro => "2026.04.06.2",
            VersionScheme::HybridYearMinorPatch => "2026.4.12",
            VersionScheme::HybridYearPatch => "2026.12",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            VersionScheme::SemVer => "MAJOR.MINOR.PATCH",
            VersionScheme::CalVerYearMonthMicro => "Year, month, then micro increment",
            VersionScheme::CalVerShortYearMonthMicro => {
                "Two-digit year, month, then micro increment"
            }
            VersionScheme::CalVerYearMonthDayMicro => "Year, month, day, then micro increment",
            VersionScheme::HybridYearMinorPatch => "Year followed by minor and patch counters",
            VersionScheme::HybridYearPatch => "Year followed by a single patch counter",
        }
    }

    pub fn next(self) -> Self {
        let index = Self::ALL
            .iter()
            .position(|candidate| *candidate == self)
            .unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    pub fn previous(self) -> Self {
        let index = Self::ALL
            .iter()
            .position(|candidate| *candidate == self)
            .unwrap_or(0);
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    pub fn validate(self, value: &str) -> Result<(), String> {
        match self {
            VersionScheme::SemVer => {
                let (semver_part, _custom) = split_custom_suffix(value);
                let (core, suffix) = split_semver(&semver_part);
                validate_parts(&core, &[PartRule::Any, PartRule::Any, PartRule::Any])?;
                if let Some(suffix) = suffix {
                    validate_semver_suffix(&suffix)?;
                }
                Ok(())
            }
            VersionScheme::CalVerYearMonthMicro => validate_parts(
                value,
                &[PartRule::Digits(4), PartRule::Month, PartRule::Any],
            ),
            VersionScheme::CalVerShortYearMonthMicro => validate_parts(
                value,
                &[PartRule::Digits(2), PartRule::Month, PartRule::Any],
            ),
            VersionScheme::CalVerYearMonthDayMicro => validate_parts(
                value,
                &[
                    PartRule::Digits(4),
                    PartRule::Month,
                    PartRule::Day,
                    PartRule::Any,
                ],
            ),
            VersionScheme::HybridYearMinorPatch => {
                validate_parts(value, &[PartRule::Digits(4), PartRule::Any, PartRule::Any])
            }
            VersionScheme::HybridYearPatch => {
                validate_parts(value, &[PartRule::Digits(4), PartRule::Any])
            }
        }
    }

    pub fn supported_actions(self) -> &'static [BumpAction] {
        match self {
            VersionScheme::SemVer => &Self::SEMVER_ACTIONS,
            VersionScheme::CalVerYearMonthMicro
            | VersionScheme::CalVerShortYearMonthMicro
            | VersionScheme::CalVerYearMonthDayMicro => &Self::CALVER_ACTIONS,
            VersionScheme::HybridYearMinorPatch => &Self::HYBRID_MINOR_PATCH_ACTIONS,
            VersionScheme::HybridYearPatch => &Self::HYBRID_PATCH_ACTIONS,
        }
    }

    pub fn bump(self, value: &str, action: BumpAction, today: NaiveDate) -> Result<String, String> {
        self.validate(value)?;
        match self {
            VersionScheme::SemVer => bump_semver(value, action),
            VersionScheme::CalVerYearMonthMicro => {
                bump_calver_year_month_micro(value, action, today)
            }
            VersionScheme::CalVerShortYearMonthMicro => {
                bump_calver_short_year_month_micro(value, action, today)
            }
            VersionScheme::CalVerYearMonthDayMicro => {
                bump_calver_year_month_day_micro(value, action, today)
            }
            VersionScheme::HybridYearMinorPatch => {
                bump_hybrid_year_minor_patch(value, action, today)
            }
            VersionScheme::HybridYearPatch => bump_hybrid_year_patch(value, action, today),
        }
    }
}

#[derive(Clone, Copy)]
enum PartRule {
    Any,
    Digits(usize),
    Month,
    Day,
}

/// Splits a version string into its standard SemVer part and an optional
/// custom suffix (everything after the first `:`).
///
/// The custom suffix is a ComfyGit extension used by forked projects to mark
/// custom changes (e.g. `0.8.0-alpha.0:comfy` or `0.8.0:comfy-alpha.0`).
/// It is preserved as-is during validation and bumping — it is never modified,
/// stripped, or dropped regardless of the bump action.
///
/// Examples:
///   "0.8.0-alpha.0-comfy"    -> ("0.8.0-alpha.0-comfy", None)
///   "0.8.0-alpha.0:comfy"    -> ("0.8.0-alpha.0", ":comfy")
///   "0.8.0:comfy-alpha.0"    -> ("0.8.0", ":comfy-alpha.0")
fn split_custom_suffix(value: &str) -> (String, Option<String>) {
    match value.find(':') {
        Some(pos) => (value[..pos].to_string(), Some(value[pos..].to_string())),
        None => (value.to_string(), None),
    }
}

/// Splits a SemVer string into its numeric core and optional suffix
/// (pre-release and/or build metadata).
///
/// Per https://semver.org/, a version is: `MAJOR.MINOR.PATCH[-prerelease][+build]`
/// The suffix starts at the first `-` or `+` after the patch component.
///
/// Examples:
///   "1.2.3"               -> ("1.2.3", None)
///   "0.8.0-alpha.0-comfy" -> ("0.8.0", "-alpha.0-comfy")
///   "1.0.0+build.42"      -> ("1.0.0", "+build.42")
///   "1.0.0-beta+exp.sha"  -> ("1.0.0", "-beta+exp.sha")
fn split_semver(value: &str) -> (String, Option<String>) {
    // Find the first `-` or `+` that appears after the third dot.
    // We need to skip past the MAJOR.MINOR.PATCH core to avoid splitting
    // on a `-` that could theoretically appear in a non-standard version.
    let dot_positions: Vec<usize> = value
        .char_indices()
        .filter(|(_, c)| *c == '.')
        .map(|(i, _)| i)
        .collect();

    // If there are at least 2 dots (3 parts), the suffix can only start
    // after the 3rd part begins.
    let search_start = if dot_positions.len() >= 2 {
        dot_positions[1] + 1 // position right after the second dot
    } else {
        0
    };

    if let Some(suffix_pos) = value[search_start..]
        .char_indices()
        .find(|(_, c)| *c == '-' || *c == '+')
        .map(|(i, _)| search_start + i)
    {
        let core = value[..suffix_pos].to_string();
        let suffix = value[suffix_pos..].to_string();
        (core, Some(suffix))
    } else {
        (value.to_string(), None)
    }
}

/// Validates a SemVer pre-release and/or build metadata suffix.
///
/// Per the spec:
/// - Pre-release: dot-separated identifiers of `[0-9A-Za-z-]+`
/// - Build: dot-separated identifiers of `[0-9A-Za-z-]+`
/// - Numeric identifiers must not have leading zeros (except `0` itself)
fn validate_semver_suffix(suffix: &str) -> Result<(), String> {
    // suffix starts with `-` (pre-release) and/or `+` (build)
    let (prerelease, build) = if let Some(plus_pos) = suffix.find('+') {
        let pre = &suffix[..plus_pos]; // includes leading `-` or is empty
        let build = &suffix[plus_pos..]; // includes leading `+`
        (if pre.is_empty() { None } else { Some(pre) }, Some(build))
    } else {
        (Some(suffix), None)
    };

    if let Some(pre) = prerelease {
        // pre starts with `-`
        if !pre.starts_with('-') {
            return Err("pre-release must start with '-'".to_string());
        }
        validate_semver_identifiers(&pre[1..], "pre-release")?;
    }

    if let Some(bld) = build {
        if !bld.starts_with('+') {
            return Err("build metadata must start with '+'".to_string());
        }
        validate_semver_identifiers(&bld[1..], "build")?;
    }

    Ok(())
}

fn validate_semver_identifiers(s: &str, label: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err(format!("{label} identifier is empty"));
    }
    for identifier in s.split('.') {
        if identifier.is_empty() {
            return Err(format!("{label} has an empty identifier"));
        }
        if !identifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err(format!(
                "{label} identifier '{}' contains invalid characters (only [0-9A-Za-z-] allowed)",
                identifier
            ));
        }
        // Numeric identifiers must not have leading zeros
        if identifier.chars().all(|c| c.is_ascii_digit())
            && identifier.len() > 1
            && identifier.starts_with('0')
        {
            return Err(format!(
                "{label} numeric identifier '{}' has leading zero",
                identifier
            ));
        }
    }
    Ok(())
}

fn validate_parts(value: &str, rules: &[PartRule]) -> Result<(), String> {
    let parts = value.split('.').collect::<Vec<_>>();
    if parts.len() != rules.len() {
        return Err(format!("expected {} dot-separated parts", rules.len()));
    }

    for (part, rule) in parts.into_iter().zip(rules.iter().copied()) {
        if part.is_empty() || !part.chars().all(|character| character.is_ascii_digit()) {
            return Err("version parts must be numeric".to_string());
        }

        match rule {
            PartRule::Any => {}
            PartRule::Digits(width) => {
                if part.len() != width {
                    return Err(format!("expected a {}-digit component", width));
                }
            }
            PartRule::Month => {
                let month = part
                    .parse::<u32>()
                    .map_err(|_| "invalid month value".to_string())?;
                if !(1..=12).contains(&month) {
                    return Err("month must be between 1 and 12".to_string());
                }
            }
            PartRule::Day => {
                let day = part
                    .parse::<u32>()
                    .map_err(|_| "invalid day value".to_string())?;
                if !(1..=31).contains(&day) {
                    return Err("day must be between 1 and 31".to_string());
                }
            }
        }
    }

    Ok(())
}

fn bump_semver(value: &str, action: BumpAction) -> Result<String, String> {
    // Split off the custom suffix (everything after `:`) first — it is
    // preserved as-is and never modified by the bump.
    let (semver_part, custom_suffix) = split_custom_suffix(value);
    let (core, suffix) = split_semver(&semver_part);
    let parts = parse_numeric_parts(&core)?;
    let [major, minor, patch]: [u32; 3] = parts
        .try_into()
        .map_err(|_| "expected 3 semver components".to_string())?;

    let bumped = match action {
        BumpAction::Major => [major + 1, 0, 0],
        BumpAction::Minor => [major, minor + 1, 0],
        BumpAction::Patch => [major, minor, patch + 1],
        BumpAction::Auto => return Err("auto bump is not supported for SemVer".to_string()),
    };

    // Per SemVer convention: major/minor bumps drop the pre-release suffix
    // (a new release cycle starts clean).  Patch bumps preserve it.
    // The custom suffix (`:...`) is always preserved regardless of bump type.
    let semver_result = match (action, suffix) {
        (BumpAction::Patch, Some(suffix)) => {
            format!("{}.{}.{}{}", bumped[0], bumped[1], bumped[2], suffix)
        }
        _ => format!("{}.{}.{}", bumped[0], bumped[1], bumped[2]),
    };

    let result = match custom_suffix {
        Some(custom) => format!("{}{}", semver_result, custom),
        None => semver_result,
    };

    Ok(result)
}

fn bump_calver_year_month_micro(
    value: &str,
    action: BumpAction,
    today: NaiveDate,
) -> Result<String, String> {
    require_action(action, &[BumpAction::Auto])?;
    let parts = parse_numeric_parts(value)?;
    let [year, month, micro]: [u32; 3] = parts
        .try_into()
        .map_err(|_| "expected 3 calver components".to_string())?;
    let current_year = today.year() as u32;
    let current_month = today.month();
    let next_micro = if year == current_year && month == current_month {
        micro + 1
    } else {
        0
    };
    Ok(format!(
        "{:04}.{:02}.{}",
        current_year, current_month, next_micro
    ))
}

fn bump_calver_short_year_month_micro(
    value: &str,
    action: BumpAction,
    today: NaiveDate,
) -> Result<String, String> {
    require_action(action, &[BumpAction::Auto])?;
    let parts = parse_numeric_parts(value)?;
    let [year, month, micro]: [u32; 3] = parts
        .try_into()
        .map_err(|_| "expected 3 calver components".to_string())?;
    let current_year = (today.year() % 100) as u32;
    let current_month = today.month();
    let next_micro = if year == current_year && month == current_month {
        micro + 1
    } else {
        0
    };
    Ok(format!(
        "{:02}.{:02}.{}",
        current_year, current_month, next_micro
    ))
}

fn bump_calver_year_month_day_micro(
    value: &str,
    action: BumpAction,
    today: NaiveDate,
) -> Result<String, String> {
    require_action(action, &[BumpAction::Auto])?;
    let parts = parse_numeric_parts(value)?;
    let [year, month, day, micro]: [u32; 4] = parts
        .try_into()
        .map_err(|_| "expected 4 calver components".to_string())?;
    let current_year = today.year() as u32;
    let current_month = today.month();
    let current_day = today.day();
    let next_micro = if year == current_year && month == current_month && day == current_day {
        micro + 1
    } else {
        0
    };
    Ok(format!(
        "{:04}.{:02}.{:02}.{}",
        current_year, current_month, current_day, next_micro
    ))
}

fn bump_hybrid_year_minor_patch(
    value: &str,
    action: BumpAction,
    today: NaiveDate,
) -> Result<String, String> {
    require_action(action, &[BumpAction::Minor, BumpAction::Patch])?;
    let parts = parse_numeric_parts(value)?;
    let [year, minor, patch]: [u32; 3] = parts
        .try_into()
        .map_err(|_| "expected 3 hybrid components".to_string())?;
    let current_year = today.year() as u32;

    let (next_minor, next_patch) = if year != current_year {
        match action {
            BumpAction::Minor => (1, 0),
            BumpAction::Patch => (0, 1),
            _ => unreachable!(),
        }
    } else {
        match action {
            BumpAction::Minor => (minor + 1, 0),
            BumpAction::Patch => (minor, patch + 1),
            _ => unreachable!(),
        }
    };

    Ok(format!("{:04}.{}.{}", current_year, next_minor, next_patch))
}

fn bump_hybrid_year_patch(
    value: &str,
    action: BumpAction,
    today: NaiveDate,
) -> Result<String, String> {
    require_action(action, &[BumpAction::Patch])?;
    let parts = parse_numeric_parts(value)?;
    let [year, patch]: [u32; 2] = parts
        .try_into()
        .map_err(|_| "expected 2 hybrid components".to_string())?;
    let current_year = today.year() as u32;
    let next_patch = if year == current_year { patch + 1 } else { 1 };
    Ok(format!("{:04}.{}", current_year, next_patch))
}

fn require_action(action: BumpAction, allowed: &[BumpAction]) -> Result<(), String> {
    if allowed.contains(&action) {
        Ok(())
    } else {
        Err(format!(
            "{} bump is not supported for this version scheme",
            action.display_name()
        ))
    }
}

fn parse_numeric_parts(value: &str) -> Result<Vec<u32>, String> {
    value
        .split('.')
        .map(|part| {
            part.parse::<u32>()
                .map_err(|_| format!("invalid numeric component '{}'", part))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;

    use super::{BumpAction, VersionScheme};

    #[test]
    fn semver_accepts_three_numeric_segments() {
        assert!(VersionScheme::SemVer.validate("1.2.3").is_ok());
        assert!(VersionScheme::SemVer.validate("1.2").is_err());
    }

    #[test]
    fn calver_requires_month_range() {
        assert!(
            VersionScheme::CalVerYearMonthMicro
                .validate("2026.04.1")
                .is_ok()
        );
        assert!(
            VersionScheme::CalVerYearMonthMicro
                .validate("2026.13.1")
                .is_err()
        );
    }

    #[test]
    fn hybrid_year_patch_requires_four_digit_year() {
        assert!(VersionScheme::HybridYearPatch.validate("2026.12").is_ok());
        assert!(VersionScheme::HybridYearPatch.validate("26.12").is_err());
    }

    #[test]
    fn year_month_day_calver_requires_four_parts() {
        assert!(
            VersionScheme::CalVerYearMonthDayMicro
                .validate("2026.04.06.2")
                .is_ok()
        );
        assert!(
            VersionScheme::CalVerYearMonthDayMicro
                .validate("2026.04.2")
                .is_err()
        );
    }

    #[test]
    fn semver_patch_bump_increments_patch() {
        let today = NaiveDate::from_ymd_opt(2026, 4, 6).unwrap();
        let bumped = VersionScheme::SemVer
            .bump("1.2.3", BumpAction::Patch, today)
            .unwrap();
        assert_eq!(bumped, "1.2.4");
    }

    #[test]
    fn calver_auto_rolls_to_current_month_and_resets_micro() {
        let today = NaiveDate::from_ymd_opt(2026, 4, 6).unwrap();
        let bumped = VersionScheme::CalVerYearMonthMicro
            .bump("2026.03.8", BumpAction::Auto, today)
            .unwrap();
        assert_eq!(bumped, "2026.04.0");
    }

    #[test]
    fn hybrid_minor_patch_rolls_year_forward() {
        let today = NaiveDate::from_ymd_opt(2026, 4, 6).unwrap();
        let bumped = VersionScheme::HybridYearMinorPatch
            .bump("2025.7.4", BumpAction::Patch, today)
            .unwrap();
        assert_eq!(bumped, "2026.0.1");
    }

    // --- SemVer pre-release / build metadata tests ---

    #[test]
    fn semver_accepts_prerelease_suffix() {
        assert!(VersionScheme::SemVer.validate("0.8.0-alpha.0-comfy").is_ok());
        assert!(VersionScheme::SemVer.validate("1.0.0-beta").is_ok());
        assert!(VersionScheme::SemVer.validate("1.0.0-alpha.1").is_ok());
        assert!(VersionScheme::SemVer.validate("1.0.0-rc.2").is_ok());
    }

    #[test]
    fn semver_accepts_build_metadata() {
        assert!(VersionScheme::SemVer.validate("1.0.0+build.42").is_ok());
        assert!(VersionScheme::SemVer.validate("1.0.0+20130313144700").is_ok());
        assert!(VersionScheme::SemVer.validate("1.0.0-beta+exp.sha.5114f85").is_ok());
    }

    #[test]
    fn semver_rejects_invalid_prerelease() {
        assert!(VersionScheme::SemVer.validate("1.0.0-").is_err());
        assert!(VersionScheme::SemVer.validate("1.0.0-alpha..1").is_err()); // empty identifier
        assert!(VersionScheme::SemVer.validate("1.0.0-alpha.01").is_err()); // leading zero
        assert!(VersionScheme::SemVer.validate("1.0.0-alpha@1").is_err()); // invalid char
    }

    #[test]
    fn semver_rejects_too_few_numeric_parts() {
        assert!(VersionScheme::SemVer.validate("1.2-alpha").is_err());
        assert!(VersionScheme::SemVer.validate("1-alpha").is_err());
    }

    #[test]
    fn semver_patch_bump_preserves_prerelease() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
        let bumped = VersionScheme::SemVer
            .bump("0.8.0-alpha.0-comfy", BumpAction::Patch, today)
            .unwrap();
        assert_eq!(bumped, "0.8.1-alpha.0-comfy");
    }

    #[test]
    fn semver_minor_bump_drops_prerelease() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
        let bumped = VersionScheme::SemVer
            .bump("0.8.0-alpha.0-comfy", BumpAction::Minor, today)
            .unwrap();
        assert_eq!(bumped, "0.9.0");
    }

    #[test]
    fn semver_major_bump_drops_prerelease() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
        let bumped = VersionScheme::SemVer
            .bump("0.8.0-alpha.0-comfy", BumpAction::Major, today)
            .unwrap();
        assert_eq!(bumped, "1.0.0");
    }

    #[test]
    fn semver_patch_bump_preserves_build_metadata() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
        let bumped = VersionScheme::SemVer
            .bump("1.0.0+build.42", BumpAction::Patch, today)
            .unwrap();
        assert_eq!(bumped, "1.0.1+build.42");
    }

    #[test]
    fn semver_plain_version_still_bumps_correctly() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
        assert_eq!(
            VersionScheme::SemVer
                .bump("1.2.3", BumpAction::Patch, today)
                .unwrap(),
            "1.2.4"
        );
        assert_eq!(
            VersionScheme::SemVer
                .bump("1.2.3", BumpAction::Minor, today)
                .unwrap(),
            "1.3.0"
        );
        assert_eq!(
            VersionScheme::SemVer
                .bump("1.2.3", BumpAction::Major, today)
                .unwrap(),
            "2.0.0"
        );
    }

    #[test]
    fn split_semver_separates_core_and_suffix() {
        use super::split_semver;
        assert_eq!(split_semver("1.2.3"), ("1.2.3".to_string(), None));
        assert_eq!(
            split_semver("0.8.0-alpha.0-comfy"),
            ("0.8.0".to_string(), Some("-alpha.0-comfy".to_string()))
        );
        assert_eq!(
            split_semver("1.0.0+build.42"),
            ("1.0.0".to_string(), Some("+build.42".to_string()))
        );
        assert_eq!(
            split_semver("1.0.0-beta+exp.sha"),
            ("1.0.0".to_string(), Some("-beta+exp.sha".to_string()))
        );
    }

    // --- Custom suffix (`:...`) tests ---

    #[test]
    fn split_custom_suffix_separates_at_colon() {
        use super::split_custom_suffix;
        assert_eq!(
            split_custom_suffix("0.8.0-alpha.0-comfy"),
            ("0.8.0-alpha.0-comfy".to_string(), None)
        );
        assert_eq!(
            split_custom_suffix("0.8.0-alpha.0:comfy"),
            ("0.8.0-alpha.0".to_string(), Some(":comfy".to_string()))
        );
        assert_eq!(
            split_custom_suffix("0.8.0:comfy-alpha.0"),
            ("0.8.0".to_string(), Some(":comfy-alpha.0".to_string()))
        );
    }

    #[test]
    fn semver_accepts_custom_suffix_after_prerelease() {
        assert!(VersionScheme::SemVer.validate("0.8.0-alpha.0:comfy").is_ok());
    }

    #[test]
    fn semver_accepts_custom_suffix_without_prerelease() {
        assert!(VersionScheme::SemVer.validate("0.8.0:comfy-alpha.0").is_ok());
    }

    #[test]
    fn semver_patch_bump_preserves_custom_suffix() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
        let bumped = VersionScheme::SemVer
            .bump("0.8.0-alpha.0:comfy", BumpAction::Patch, today)
            .unwrap();
        assert_eq!(bumped, "0.8.1-alpha.0:comfy");
    }

    #[test]
    fn semver_minor_bump_drops_prerelease_but_keeps_custom_suffix() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
        let bumped = VersionScheme::SemVer
            .bump("0.8.0-alpha.0:comfy", BumpAction::Minor, today)
            .unwrap();
        assert_eq!(bumped, "0.9.0:comfy");
    }

    #[test]
    fn semver_major_bump_drops_prerelease_but_keeps_custom_suffix() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
        let bumped = VersionScheme::SemVer
            .bump("0.8.0-alpha.0:comfy", BumpAction::Major, today)
            .unwrap();
        assert_eq!(bumped, "1.0.0:comfy");
    }

    #[test]
    fn semver_patch_bump_with_custom_suffix_no_prerelease() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
        let bumped = VersionScheme::SemVer
            .bump("0.8.0:comfy-alpha.0", BumpAction::Patch, today)
            .unwrap();
        assert_eq!(bumped, "0.8.1:comfy-alpha.0");
    }
}
