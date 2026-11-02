// Copyright © 2026 ComfyHome™
// All rights reserved.
//
// Licensed under the ComfyGit License v1.2

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{
    forge::ForgePullRequest,
    glab::cli::{self, CLI_NAME},
    glab::remote,
};

pub fn list_open_merge_requests(repo_root: &str, limit: usize) -> Result<Vec<ForgePullRequest>> {
    // Use the GitLab REST API directly instead of `glab mr list --output json`.
    // The `glab mr list` command omits the `merge_status` field from its JSON
    // output, leaving only `detailed_merge_status` which can be stuck at
    // "checking" for a long time even when the MR is actually mergeable.
    let project_path = resolve_gitlab_project_path(repo_root)?;
    let encoded_path = percent_encode_path(&project_path);
    let per_page = limit.to_string();
    let endpoint = format!(
        "projects/{}/merge_requests?state=opened&per_page={}&order_by=created_at&sort=desc",
        encoded_path, per_page
    );
    let output = cli::run_in_repo(repo_root, &["api", &endpoint])?;
    if !output.status.success() {
        bail_cli_failure("api merge_requests list", &output)?;
    }

    let listed: Vec<GlabMergeRequest> = serde_json::from_slice(&output.stdout)
        .context("failed to parse GitLab API merge_requests list output")?;
    let repository_issue_root = remote::repository_web_url(repo_root);
    Ok(listed
        .into_iter()
        .map(|mr| mr.into_forge(repository_issue_root.as_deref()))
        .collect())
}

pub fn view_merge_request(repo_root: &str, number: u64) -> Result<ForgePullRequest> {
    // Use the GitLab REST API directly to get the reliable `merge_status` field.
    let project_path = resolve_gitlab_project_path(repo_root)?;
    let encoded_path = percent_encode_path(&project_path);
    let endpoint = format!("projects/{}/merge_requests/{}", encoded_path, number);
    let output = cli::run_in_repo(repo_root, &["api", &endpoint])?;
    if !output.status.success() {
        bail_cli_failure("api merge_requests view", &output)?;
    }
    let mr: GlabMergeRequest = serde_json::from_slice(&output.stdout)
        .context("failed to parse GitLab API merge_request output")?;
    let repository_issue_root = remote::repository_web_url(repo_root);
    Ok(mr.into_forge(repository_issue_root.as_deref()))
}

pub fn fetch_mergeability(repo_root: &str, number: u64) -> Result<crate::forge::ForgeMergeability> {
    // Use the GitLab REST API directly instead of `glab mr view --output json`.
    // The `glab mr view` command omits the `merge_status` field from its JSON
    // output, leaving only `detailed_merge_status` which can be stuck at
    // "checking" for a long time even when the MR is actually mergeable.
    let project_path = resolve_gitlab_project_path(repo_root)?;
    let encoded_path = percent_encode_path(&project_path);

    // Force GitLab to recompute mergeability by hitting the merge_ref endpoint.
    // GitLab's `merge_status` can be stuck at "checking" indefinitely, especially
    // for newly created MRs. The `merge_ref` endpoint forces a recheck as a side
    // effect and returns a commit SHA if the MR is mergeable.
    let merge_ref_endpoint = format!(
        "projects/{}/merge_requests/{}/merge_ref",
        encoded_path, number
    );
    let _ = cli::run_in_repo(repo_root, &["api", &merge_ref_endpoint]);

    // Now fetch the (recomputed) merge status.
    let endpoint = format!("projects/{}/merge_requests/{}", encoded_path, number);
    let output = cli::run_in_repo(repo_root, &["api", &endpoint])?;
    if !output.status.success() {
        bail_cli_failure("api merge_requests", &output)?;
    }
    let api_response: GitlabMergeRequestApi =
        serde_json::from_slice(&output.stdout).context("failed to parse GitLab API response")?;
    Ok(crate::forge::ForgeMergeability {
        // `merge_status` is the reliable field (e.g. "can_be_merged").
        // Fall back to `detailed_merge_status` if `merge_status` is absent.
        mergeable: api_response
            .merge_status
            .or(api_response.detailed_merge_status.clone())
            .unwrap_or_else(|| "unknown".to_string()),
        merge_state_status: api_response
            .detailed_merge_status
            .unwrap_or_else(|| "unknown".to_string()),
    })
}

/// Resolves the GitLab project path (URL-encoded) for API calls.
fn resolve_gitlab_project_path(repo_root: &str) -> Result<String> {
    let remote_name = crate::git::default_push_remote_name(repo_root)?;
    let remote_url = crate::git::run_git_checked(repo_root, &["remote", "get-url", &remote_name])?;
    let (owner, repo) = crate::glab::remote::owner_repo_from_remote_url(remote_url.trim())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "could not parse GitLab project path from remote URL '{}'",
                remote_url.trim()
            )
        })?;
    Ok(format!("{owner}/{repo}"))
}

/// Percent-encodes a GitLab project path for use in API URLs.
/// The `/` separator between namespace segments must be encoded as `%2F`
/// so GitLab treats the entire path as a single URL segment (project ID).
fn percent_encode_path(path: &str) -> String {
    path.chars()
        .map(|c| match c {
            '/' => "%2F".to_string(),
            c if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') => c.to_string(),
            c => {
                let mut encoded = String::new();
                for byte in c.to_string().as_bytes() {
                    encoded.push_str(&format!("%{:02X}", byte));
                }
                encoded
            }
        })
        .collect()
}

pub fn merge_merge_request(
    repo_root: &str,
    number: u64,
    subject: &str,
    delete_remote_branch: bool,
) -> Result<String> {
    // Use the GitLab REST API directly to avoid `glab mr merge` 401 issues
    // with projects in subgroups.
    let project_path = resolve_gitlab_project_path(repo_root)?;
    let encoded_path = percent_encode_path(&project_path);
    let endpoint = format!("projects/{}/merge_requests/{}/merge", encoded_path, number);
    let remove_flag = if delete_remote_branch {
        "remove_source_branch=true"
    } else {
        "remove_source_branch=false"
    };
    let output = cli::run_in_repo(
        repo_root,
        &[
            "api",
            "--method",
            "PUT",
            &endpoint,
            "--field",
            &format!("merge_commit_message={subject}"),
            "--field",
            remove_flag,
        ],
    )?;
    if !output.status.success() {
        bail_cli_failure("api merge_requests merge", &output)?;
    }
    // Return empty string — the caller prints its own merge confirmation.
    // The API response is a large JSON blob that should not be printed.
    Ok(String::new())
}

pub fn create_merge_request(
    repo_root: &str,
    target_branch: &str,
    current_branch: &str,
    title: &str,
    body_path: &Path,
) -> Result<String> {
    let body = std::fs::read_to_string(body_path).with_context(|| {
        format!(
            "failed to read merge request body from '{}'",
            body_path.display()
        )
    })?;

    // Use the GitLab REST API directly instead of `glab mr create`.
    // `glab mr create` can fail with 401 Unauthorized for projects in subgroups
    // due to a glab bug in project resolution. The REST API works reliably.
    let project_path = resolve_gitlab_project_path(repo_root)?;
    let encoded_path = percent_encode_path(&project_path);

    // Write the description to a temp file and use --field with @ to pass it,
    // to avoid shell escaping issues with large markdown bodies.
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp_body = std::env::temp_dir().join(format!("comfygit-mr-create-{timestamp}.md"));
    std::fs::write(&temp_body, &body)
        .with_context(|| format!("failed to write temp body to {}", temp_body.display()))?;

    let body_file_arg = format!("@{}", temp_body.display());
    let source_branch_arg = format!("source_branch={current_branch}");
    let target_branch_arg = format!("target_branch={target_branch}");
    let title_arg = format!("title={title}");
    let remove_source_arg = "remove_source_branch=false";

    let output = cli::run_in_repo(
        repo_root,
        &[
            "api",
            "--method",
            "POST",
            &format!("projects/{}/merge_requests", encoded_path),
            "--field",
            &source_branch_arg,
            "--field",
            &target_branch_arg,
            "--field",
            &title_arg,
            "--field",
            &format!("description={}", body_file_arg),
            "--field",
            remove_source_arg,
        ],
    )?;

    let _ = std::fs::remove_file(&temp_body);

    if !output.status.success() {
        bail_cli_failure("api merge_requests create", &output)?;
    }

    // Parse the response to extract the MR web URL.
    let response: GitlabMergeRequestCreateResponse = serde_json::from_slice(&output.stdout)
        .context("failed to parse GitLab API merge_request create response")?;
    Ok(response.web_url)
}

pub fn lookup_created_merge_request(
    repo_root: &str,
    current_branch: &str,
    target_branch: &str,
) -> Result<(u64, String)> {
    // Use the GitLab REST API directly to avoid `glab mr list` 401 issues
    // with projects in subgroups.
    let project_path = resolve_gitlab_project_path(repo_root)?;
    let encoded_path = percent_encode_path(&project_path);
    let endpoint = format!(
        "projects/{}/merge_requests?source_branch={}&state=opened&per_page=20",
        encoded_path, current_branch
    );
    let output = cli::run_in_repo(repo_root, &["api", &endpoint])?;
    if !output.status.success() {
        bail_cli_failure("api merge_requests lookup", &output)?;
    }

    let listed: Vec<GlabMergeRequestLookup> = serde_json::from_slice(&output.stdout)
        .context("failed to parse GitLab API merge_requests lookup output")?;
    let matched = listed
        .into_iter()
        .find(|mr| mr.target_branch == target_branch)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "created merge request for branch '{current_branch}' targeting '{target_branch}' could not be resolved"
            )
        })?;
    Ok((matched.iid, matched.web_url))
}

fn bail_cli_failure(action: &str, output: &std::process::Output) -> Result<()> {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stderr.is_empty() {
        bail!("{CLI_NAME} {action} failed: {stderr}");
    }
    if !stdout.is_empty() {
        bail!("{CLI_NAME} {action} failed: {stdout}");
    }
    bail!(
        "{CLI_NAME} {action} failed with exit code {:?}",
        output.status.code()
    );
}

#[derive(Deserialize)]
struct GlabMergeRequest {
    iid: u64,
    title: String,
    target_branch: String,
    source_branch: String,
    created_at: String,
    author: Option<GlabAuthor>,
    merge_status: Option<String>,
    detailed_merge_status: Option<String>,
    has_conflicts: Option<bool>,
    web_url: Option<String>,
}

/// Minimal struct for parsing the GitLab REST API response for a merge request.
/// Used by `fetch_mergeability` to get the reliable `merge_status` field.
#[derive(Deserialize)]
struct GitlabMergeRequestApi {
    merge_status: Option<String>,
    detailed_merge_status: Option<String>,
}

/// Response from creating a merge request via the GitLab REST API.
#[derive(Deserialize)]
struct GitlabMergeRequestCreateResponse {
    web_url: String,
}

#[derive(Deserialize)]
struct GlabAuthor {
    username: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct GlabMergeRequestLookup {
    iid: u64,
    web_url: String,
    target_branch: String,
}

fn is_mergeable_status(status: &str) -> bool {
    matches!(
        status.to_ascii_lowercase().as_str(),
        "can_be_merged" | "mergeable"
    )
}

impl GlabMergeRequest {
    fn into_forge(self, repository_issue_root: Option<&str>) -> ForgePullRequest {
        // Prefer `merge_status` (reliable, e.g. "can_be_merged") over
        // `detailed_merge_status` (can be stale at "checking" for a long time).
        let mut mergeable_state = self
            .merge_status
            .clone()
            .or(self.detailed_merge_status.clone())
            .unwrap_or_else(|| "unknown".to_string());
        // GitLab's `merge_status` can be stuck at "checking" indefinitely.
        // If `has_conflicts` is explicitly `false`, treat it as mergeable
        // so the picker doesn't show a false "Mergeable: False".
        if mergeable_state.eq_ignore_ascii_case("checking") && self.has_conflicts == Some(false) {
            mergeable_state = "can_be_merged".to_string();
        }
        let status = mergeable_state.clone();
        let issue_url = repository_issue_root
            .filter(|_| !is_mergeable_status(&mergeable_state))
            .map(|root| format!("{root}/-/merge_requests/{}/conflicts", self.iid));
        let author = self
            .author
            .and_then(|author| {
                author
                    .username
                    .filter(|value| !value.trim().is_empty())
                    .or(author.name.filter(|name| !name.trim().is_empty()))
            })
            .unwrap_or_else(|| "-".to_string());
        ForgePullRequest {
            number: self.iid,
            title: self.title,
            target_branch: self.target_branch,
            source_branch: self.source_branch,
            created_at: self.created_at,
            author,
            status,
            mergeable_state,
            issue_url: issue_url.or(self.web_url),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MR_VIEW_JSON: &str = r#"{
        "iid": 140,
        "title": "feature",
        "target_branch": "0.35.x",
        "source_branch": "v0.35.1-dev",
        "created_at": "2026-05-30T11:00:53.045Z",
        "author": { "username": "dev-ComfyHome", "name": "Tom" },
        "detailed_merge_status": "mergeable",
        "web_url": "https://gitlab.com/comfyhome/dist/ComfyGit/-/merge_requests/140"
    }"#;

    const MR_LIST_JSON: &str = r#"[{
        "iid": 140,
        "web_url": "https://gitlab.com/comfyhome/dist/ComfyGit/-/merge_requests/140",
        "target_branch": "0.35.x",
        "source_branch": "v0.35.1-dev"
    }]"#;

    #[test]
    fn parses_glab_mr_view_snake_case_json() {
        let mr: GlabMergeRequest = serde_json::from_str(MR_VIEW_JSON).expect("parse mr view");
        let forge = mr.into_forge(Some("https://gitlab.com/comfyhome/dist/ComfyGit"));
        assert_eq!(forge.number, 140);
        assert_eq!(forge.target_branch, "0.35.x");
        assert_eq!(forge.source_branch, "v0.35.1-dev");
        assert_eq!(forge.mergeable_state, "mergeable");
        assert_eq!(forge.author, "dev-ComfyHome");
    }

    #[test]
    fn parses_glab_mr_list_lookup_snake_case_json() {
        let listed: Vec<GlabMergeRequestLookup> =
            serde_json::from_str(MR_LIST_JSON).expect("parse mr list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].iid, 140);
        assert_eq!(listed[0].target_branch, "0.35.x");
    }

    #[test]
    fn gitlab_api_response_parses_merge_status() {
        let api_json = r#"{"merge_status":"can_be_merged","detailed_merge_status":"checking"}"#;
        let api: GitlabMergeRequestApi =
            serde_json::from_str(api_json).expect("parse api response");
        assert_eq!(api.merge_status.as_deref(), Some("can_be_merged"));
        assert_eq!(api.detailed_merge_status.as_deref(), Some("checking"));
    }

    #[test]
    fn into_forge_prefers_merge_status_over_detailed_merge_status() {
        // When both fields are present, merge_status (reliable) should win
        // over detailed_merge_status (can be stale at "checking").
        let mr_json = r#"{
            "iid": 119,
            "title": "test",
            "target_branch": "main",
            "source_branch": "feature",
            "created_at": "2026-08-05T20:58:11.708Z",
            "merge_status": "can_be_merged",
            "detailed_merge_status": "checking"
        }"#;
        let mr: GlabMergeRequest = serde_json::from_str(mr_json).expect("parse");
        let forge = mr.into_forge(None);
        assert_eq!(forge.mergeable_state, "can_be_merged");
    }

    #[test]
    fn into_forge_treats_checking_with_no_conflicts_as_mergeable() {
        // GitLab's merge_status can be stuck at "checking" indefinitely.
        // If has_conflicts is explicitly false, treat it as mergeable.
        let mr_json = r#"{
            "iid": 119,
            "title": "test",
            "target_branch": "main",
            "source_branch": "feature",
            "created_at": "2026-08-05T20:58:11.708Z",
            "merge_status": "checking",
            "detailed_merge_status": "checking",
            "has_conflicts": false
        }"#;
        let mr: GlabMergeRequest = serde_json::from_str(mr_json).expect("parse");
        let forge = mr.into_forge(None);
        assert_eq!(forge.mergeable_state, "can_be_merged");
    }

    #[test]
    fn into_forge_preserves_conflict_state_when_has_conflicts_true() {
        let mr_json = r#"{
            "iid": 119,
            "title": "test",
            "target_branch": "main",
            "source_branch": "feature",
            "created_at": "2026-08-05T20:58:11.708Z",
            "merge_status": "checking",
            "detailed_merge_status": "checking",
            "has_conflicts": true
        }"#;
        let mr: GlabMergeRequest = serde_json::from_str(mr_json).expect("parse");
        let forge = mr.into_forge(None);
        assert_eq!(forge.mergeable_state, "checking");
    }

    #[test]
    fn percent_encode_path_preserves_slashes() {
        assert_eq!(
            percent_encode_path("comfyhome/x-project/my-repo"),
            "comfyhome%2Fx-project%2Fmy-repo"
        );
        assert_eq!(percent_encode_path("simple/repo"), "simple%2Frepo");
    }

    #[test]
    fn percent_encode_path_encodes_special_chars() {
        assert_eq!(percent_encode_path("group/my repo"), "group%2Fmy%20repo");
        assert_eq!(percent_encode_path("group/repo.name"), "group%2Frepo.name");
    }
}
