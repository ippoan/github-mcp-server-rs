//! Issues (ci-dashboard `src/mcp/tools/issues.ts` 移植) — read + write 両方。

use reqwest::Method;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::github_api::{github_api_json, parse_and_validate_repo, validate_org};
use crate::mcp_server::GithubMcp;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListIssuesArgs {
    /// Repository (e.g. 'rust-alc-api').
    pub repo: String,
    /// Issue state filter: "open" | "closed" | "all" (default: open).
    #[serde(default)]
    pub state: Option<String>,
    /// Comma-separated label names (e.g. "bug,enhancement").
    #[serde(default)]
    pub labels: Option<String>,
    /// Results per page (1–100, default 20).
    #[serde(default)]
    pub per_page: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetIssueArgs {
    /// Repository (e.g. 'rust-alc-api').
    pub repo: String,
    /// Issue number.
    pub issue_number: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListOrgIssuesArgs {
    /// Organization names (e.g. ["ippoan", "ohishi-exp"]).
    pub orgs: Vec<String>,
    /// Issue state: "open" | "closed" | "all" (default: open).
    #[serde(default)]
    pub state: Option<String>,
    /// AND filter by label names.
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    /// GitHub username, or "@me" for the current token's user.
    #[serde(default)]
    pub assignee: Option<String>,
    /// Raw GitHub search syntax appended to q (advanced).
    /// If it contains `repo:owner/name`, the `org:` qualifier is omitted
    /// (GitHub silently drops `repo:` when `org:` is also present).
    #[serde(default)]
    pub query: Option<String>,
    /// Results per page (1–100, default 30).
    #[serde(default)]
    pub per_page: Option<u32>,
}

fn issue_summary(i: &serde_json::Value) -> serde_json::Value {
    let labels: Vec<&str> = i
        .get("labels")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l.get("name").and_then(|n| n.as_str()))
                .collect()
        })
        .unwrap_or_default();
    serde_json::json!({
        "number": i.get("number"),
        "title": i.get("title"),
        "state": i.get("state"),
        "author": i.get("user").and_then(|u| u.get("login")),
        "labels": labels,
        "created_at": i.get("created_at"),
        "updated_at": i.get("updated_at"),
        "comments": i.get("comments"),
        "url": i.get("html_url"),
    })
}

#[tool_router(router = issues_router, vis = "pub(crate)")]
impl GithubMcp {
    /// List issues for a repository. Supports state and label filtering.
    #[tool(description = "List issues for a repository. Supports state and label filtering.")]
    async fn list_issues(
        &self,
        Parameters(args): Parameters<ListIssuesArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let state = args.state.unwrap_or_else(|| "open".to_string());
        let per_page = args.per_page.unwrap_or(20).clamp(1, 100);
        let mut params: Vec<(&str, String)> =
            vec![("state", state), ("per_page", per_page.to_string())];
        if let Some(l) = args.labels {
            params.push(("labels", l));
        }
        let path = format!("/repos/{}/{}/issues", r.owner, r.repo);
        let issues: Vec<serde_json::Value> = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::GET,
            &path,
            &params,
            None,
            &[],
        )
        .await?;
        // PRs are returned by `/issues` too — filter them out.
        let result: Vec<serde_json::Value> = issues
            .iter()
            .filter(|i| i.get("pull_request").is_none())
            .map(issue_summary)
            .collect();
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Get issue details including body and comments.
    #[tool(description = "Get issue details including body and comments.")]
    async fn get_issue(
        &self,
        Parameters(args): Parameters<GetIssueArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let issue_path = format!("/repos/{}/{}/issues/{}", r.owner, r.repo, args.issue_number);
        let comments_path = format!(
            "/repos/{}/{}/issues/{}/comments",
            r.owner, r.repo, args.issue_number
        );
        let (issue, comments) = tokio::join!(
            github_api_json::<serde_json::Value>(
                &self.ctx().client,
                &self.ctx().github_token,
                Method::GET,
                &issue_path,
                &[],
                None,
                &[],
            ),
            github_api_json::<Vec<serde_json::Value>>(
                &self.ctx().client,
                &self.ctx().github_token,
                Method::GET,
                &comments_path,
                &[],
                None,
                &[],
            ),
        );
        let issue = issue?;
        let comments = comments?;

        let labels: Vec<&str> = issue
            .get("labels")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|l| l.get("name").and_then(|n| n.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        let comments_out: Vec<serde_json::Value> = comments
            .iter()
            .map(|c| {
                serde_json::json!({
                    "author": c.get("user").and_then(|u| u.get("login")),
                    "created_at": c.get("created_at"),
                    "body": c.get("body"),
                })
            })
            .collect();
        let result = serde_json::json!({
            "number": issue.get("number"),
            "title": issue.get("title"),
            "state": issue.get("state"),
            "author": issue.get("user").and_then(|u| u.get("login")),
            "labels": labels,
            "created_at": issue.get("created_at"),
            "updated_at": issue.get("updated_at"),
            "body": issue.get("body"),
            "url": issue.get("html_url"),
            "comments": comments_out,
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// List issues across multiple orgs in one call (search-backed, PRs excluded).
    #[tool(
        description = "List issues across multiple orgs in one call (uses GitHub search). Filters by state/labels/assignee. PRs are excluded. If `query` contains `repo:owner/name`, the `orgs` allowlist is still validated but `org:` is omitted from the search (GitHub silently drops `repo:` when combined with `org:`)."
    )]
    async fn list_org_issues(
        &self,
        Parameters(args): Parameters<ListOrgIssuesArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if args.orgs.is_empty() {
            return Err(rmcp::ErrorData::invalid_params(
                "orgs must be a non-empty array",
                None,
            ));
        }
        for o in &args.orgs {
            validate_org(o)?;
        }
        let state = args.state.unwrap_or_else(|| "open".to_string());
        let per_page = args.per_page.unwrap_or(30).clamp(1, 100);

        let query_has_repo = args
            .query
            .as_deref()
            .map(|q| q.split_whitespace().any(|tok| tok.starts_with("repo:")))
            .unwrap_or(false);

        let mut parts: Vec<String> = vec!["is:issue".to_string()];
        if state != "all" {
            parts.push(format!("state:{state}"));
        }
        if !query_has_repo {
            for o in &args.orgs {
                parts.push(format!("org:{o}"));
            }
        }
        if let Some(labels) = &args.labels {
            for l in labels {
                parts.push(format!("label:\"{l}\""));
            }
        }
        if let Some(assignee) = &args.assignee {
            parts.push(format!("assignee:{assignee}"));
        }
        if let Some(q) = &args.query {
            parts.push(q.clone());
        }
        let q = parts.join(" ");

        let data: serde_json::Value = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::GET,
            "/search/issues",
            &[("q", q), ("per_page", per_page.to_string())],
            None,
            &[],
        )
        .await?;
        let items: Vec<serde_json::Value> = data
            .get("items")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter(|i| i.get("pull_request").is_none())
                    .map(|i| {
                        let repo_url = i
                            .get("repository_url")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let repo = {
                            let segs: Vec<&str> = repo_url.split('/').collect();
                            if segs.len() >= 2 {
                                format!("{}/{}", segs[segs.len() - 2], segs[segs.len() - 1])
                            } else {
                                String::new()
                            }
                        };
                        let labels: Vec<&str> = i
                            .get("labels")
                            .and_then(|v| v.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|l| l.get("name").and_then(|n| n.as_str()))
                                    .collect()
                            })
                            .unwrap_or_default();
                        let assignees: Vec<&str> = i
                            .get("assignees")
                            .and_then(|v| v.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|a| a.get("login").and_then(|n| n.as_str()))
                                    .collect()
                            })
                            .unwrap_or_default();
                        serde_json::json!({
                            "repo": repo,
                            "number": i.get("number"),
                            "title": i.get("title"),
                            "state": i.get("state"),
                            "author": i
                                .get("user")
                                .and_then(|u| u.get("login"))
                                .and_then(|v| v.as_str())
                                .unwrap_or(""),
                            "labels": labels,
                            "assignees": assignees,
                            "comments": i.get("comments"),
                            "created_at": i.get("created_at"),
                            "updated_at": i.get("updated_at"),
                            "url": i.get("html_url"),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let result = serde_json::json!({
            "total_count": data.get("total_count"),
            "incomplete": data.get("incomplete_results"),
            "items": items,
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Create a new issue in a repository.
    #[tool(description = "Create a new issue in a repository.")]
    async fn create_issue(
        &self,
        Parameters(args): Parameters<CreateIssueArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let mut payload = serde_json::Map::new();
        payload.insert("title".into(), serde_json::Value::String(args.title));
        if let Some(b) = args.body {
            payload.insert("body".into(), serde_json::Value::String(b));
        }
        if let Some(labels) = args.labels {
            payload.insert("labels".into(), serde_json::json!(labels));
        }
        if let Some(assignees) = args.assignees {
            payload.insert("assignees".into(), serde_json::json!(assignees));
        }
        let path = format!("/repos/{}/{}/issues", r.owner, r.repo);
        let created: serde_json::Value = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::POST,
            &path,
            &[],
            Some(&serde_json::Value::Object(payload)),
            &[],
        )
        .await?;
        let result = serde_json::json!({
            "number": created.get("number"),
            "title": created.get("title"),
            "state": created.get("state"),
            "url": created.get("html_url"),
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Update an existing issue's title / body / labels / assignees / milestone.
    /// State changes are intentionally not supported — use `close_issue` / `reopen_issue`.
    #[tool(
        description = "Update an existing issue's title / body / labels / assignees / milestone. State changes are intentionally not supported here — use close_issue / reopen_issue."
    )]
    async fn update_issue(
        &self,
        Parameters(args): Parameters<UpdateIssueArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let mut payload = serde_json::Map::new();
        if let Some(t) = args.title {
            payload.insert("title".into(), serde_json::Value::String(t));
        }
        if let Some(b) = args.body {
            payload.insert("body".into(), serde_json::Value::String(b));
        }
        if let Some(labels) = args.labels {
            payload.insert("labels".into(), serde_json::json!(labels));
        }
        if let Some(assignees) = args.assignees {
            payload.insert("assignees".into(), serde_json::json!(assignees));
        }
        // `milestone: null` を明示するため Option<Option<u64>> ではなく
        // serde_json::Value で `Null` を受け取れる UpdateIssueArgs::milestone を
        // 使う (null も値 detach として有効)。
        if let Some(m) = args.milestone {
            payload.insert("milestone".into(), m);
        }
        if payload.is_empty() {
            return Err(rmcp::ErrorData::invalid_params(
                "update_issue: at least one of title/body/labels/assignees/milestone must be provided",
                None,
            ));
        }
        let path = format!("/repos/{}/{}/issues/{}", r.owner, r.repo, args.issue_number);
        let updated: serde_json::Value = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::PATCH,
            &path,
            &[],
            Some(&serde_json::Value::Object(payload)),
            &[],
        )
        .await?;
        let labels: Vec<&str> = updated
            .get("labels")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|l| l.get("name").and_then(|n| n.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        let result = serde_json::json!({
            "number": updated.get("number"),
            "title": updated.get("title"),
            "state": updated.get("state"),
            "labels": labels,
            "url": updated.get("html_url"),
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Add a comment to an existing issue or pull request.
    #[tool(description = "Add a comment to an existing issue or pull request.")]
    async fn add_issue_comment(
        &self,
        Parameters(args): Parameters<AddIssueCommentArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let path = format!(
            "/repos/{}/{}/issues/{}/comments",
            r.owner, r.repo, args.issue_number
        );
        let body = serde_json::json!({ "body": args.body });
        let created: serde_json::Value = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::POST,
            &path,
            &[],
            Some(&body),
            &[],
        )
        .await?;
        let result = serde_json::json!({
            "id": created.get("id"),
            "url": created.get("html_url"),
            "created_at": created.get("created_at"),
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Add labels to an issue or pull request. Returns the current label list.
    #[tool(description = "Add labels to an issue or pull request. Returns the current label list.")]
    async fn add_labels(
        &self,
        Parameters(args): Parameters<AddLabelsArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if args.labels.is_empty() {
            return Err(rmcp::ErrorData::invalid_params(
                "labels must be a non-empty array",
                None,
            ));
        }
        let r = parse_and_validate_repo(&args.repo)?;
        let path = format!(
            "/repos/{}/{}/issues/{}/labels",
            r.owner, r.repo, args.issue_number
        );
        let payload = serde_json::json!({ "labels": args.labels });
        let updated: serde_json::Value = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::POST,
            &path,
            &[],
            Some(&payload),
            &[],
        )
        .await?;
        let names: Vec<&str> = updated
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|l| l.get("name").and_then(|n| n.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&names).unwrap_or_default(),
        )]))
    }

    /// Remove a single label from an issue or pull request. Returns the remaining label list.
    #[tool(
        description = "Remove a single label from an issue or pull request. Returns the remaining label list."
    )]
    async fn remove_label(
        &self,
        Parameters(args): Parameters<RemoveLabelArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        // `encodeURIComponent` 相当: utf8_percent_encode を使わず GitHub API が
        // 受け付ける範囲だけ手動で escape。RFC 3986 unreserved + sub-delims のうち
        // path segment で問題になる `/`, `?`, `#`, ` ` のみ最低限置換。
        let label = url_path_encode(&args.label);
        let path = format!(
            "/repos/{}/{}/issues/{}/labels/{}",
            r.owner, r.repo, args.issue_number, label
        );
        let remaining: serde_json::Value = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::DELETE,
            &path,
            &[],
            None,
            &[],
        )
        .await?;
        let names: Vec<&str> = remaining
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|l| l.get("name").and_then(|n| n.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&names).unwrap_or_default(),
        )]))
    }

    /// Close an issue. Optionally set state_reason to "completed" or "not_planned".
    #[tool(
        description = "Close an issue. Optionally set state_reason to 'completed' or 'not_planned'."
    )]
    async fn close_issue(
        &self,
        Parameters(args): Parameters<CloseIssueArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let state_reason = args.state_reason.unwrap_or_else(|| "completed".to_string());
        let payload = serde_json::json!({
            "state": "closed",
            "state_reason": state_reason,
        });
        let path = format!("/repos/{}/{}/issues/{}", r.owner, r.repo, args.issue_number);
        let updated: serde_json::Value = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::PATCH,
            &path,
            &[],
            Some(&payload),
            &[],
        )
        .await?;
        let result = serde_json::json!({
            "number": updated.get("number"),
            "state": updated.get("state"),
            "state_reason": updated.get("state_reason"),
            "url": updated.get("html_url"),
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Reopen a closed issue.
    #[tool(description = "Reopen a closed issue.")]
    async fn reopen_issue(
        &self,
        Parameters(args): Parameters<GetIssueArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let payload = serde_json::json!({ "state": "open" });
        let path = format!("/repos/{}/{}/issues/{}", r.owner, r.repo, args.issue_number);
        let updated: serde_json::Value = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::PATCH,
            &path,
            &[],
            Some(&payload),
            &[],
        )
        .await?;
        let result = serde_json::json!({
            "number": updated.get("number"),
            "state": updated.get("state"),
            "url": updated.get("html_url"),
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateIssueArgs {
    /// Repository (e.g. 'rust-alc-api').
    pub repo: String,
    /// Issue title.
    pub title: String,
    /// Issue body (markdown).
    #[serde(default)]
    pub body: Option<String>,
    /// Label names to attach.
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    /// GitHub usernames to assign.
    #[serde(default)]
    pub assignees: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateIssueArgs {
    /// Repository (e.g. 'rust-alc-api').
    pub repo: String,
    /// Issue number.
    pub issue_number: u64,
    /// New title.
    #[serde(default)]
    pub title: Option<String>,
    /// New body (markdown). Pass "" to clear.
    #[serde(default)]
    pub body: Option<String>,
    /// Replace labels with this list (pass [] to remove all).
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    /// Replace assignees with this list (pass [] to clear).
    #[serde(default)]
    pub assignees: Option<Vec<String>>,
    /// Milestone number, or null to detach.
    /// `serde_json::Value` で受け取って `null` 明示と未指定を区別する。
    #[serde(default)]
    pub milestone: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddIssueCommentArgs {
    /// Repository (e.g. 'rust-alc-api').
    pub repo: String,
    /// Issue or PR number.
    pub issue_number: u64,
    /// Comment body (markdown).
    pub body: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddLabelsArgs {
    /// Repository (e.g. 'rust-alc-api').
    pub repo: String,
    /// Issue or PR number.
    pub issue_number: u64,
    /// Label names to add (non-empty).
    pub labels: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RemoveLabelArgs {
    /// Repository (e.g. 'rust-alc-api').
    pub repo: String,
    /// Issue or PR number.
    pub issue_number: u64,
    /// Label name to remove.
    pub label: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CloseIssueArgs {
    /// Repository (e.g. 'rust-alc-api').
    pub repo: String,
    /// Issue number.
    pub issue_number: u64,
    /// Reason for closing: "completed" | "not_planned" (default: "completed").
    #[serde(default)]
    pub state_reason: Option<String>,
}

/// URL path segment encoding for label names. GitHub `DELETE /labels/{name}` は
/// reserved char (space / `/` / `?` / `#`) を含むラベル名で 404 になるので、
/// 該当 char を percent-encode する。`%` 自体も escape する。
fn url_path_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            // unreserved: A-Z a-z 0-9 - . _ ~ + その他 ASCII printable は素通し
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::url_path_encode;

    #[test]
    fn url_path_encode_passthrough_alphanum() {
        assert_eq!(url_path_encode("bug"), "bug");
        assert_eq!(url_path_encode("good-first-issue"), "good-first-issue");
    }

    #[test]
    fn url_path_encode_escapes_space_and_slash() {
        assert_eq!(url_path_encode("type: bug"), "type%3A%20bug");
        assert_eq!(url_path_encode("priority/high"), "priority%2Fhigh");
    }

    #[test]
    fn url_path_encode_escapes_utf8() {
        // 日本語ラベル (ci-dashboard で使用例あり)
        let out = url_path_encode("バグ");
        // バ = E3 83 90, グ = E3 82 B0
        assert_eq!(out, "%E3%83%90%E3%82%B0");
    }
}
