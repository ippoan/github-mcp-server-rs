//! Issues 読取り (list / get / list_org_issues) — ci-dashboard `src/mcp/tools/issues.ts` 移植。

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
        let mut params: Vec<(&str, String)> = vec![
            ("state", state),
            ("per_page", per_page.to_string()),
        ];
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
                                format!(
                                    "{}/{}",
                                    segs[segs.len() - 2],
                                    segs[segs.len() - 1]
                                )
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
}
