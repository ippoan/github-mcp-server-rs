//! Pull requests (ci-dashboard `src/mcp/tools/pulls.ts` 移植) — read + write 両方。

use reqwest::Method;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::github_api::{github_api_json, parse_and_validate_repo};
use crate::mcp_server::GithubMcp;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListPullsArgs {
    /// Repository (e.g. 'rust-alc-api').
    pub repo: String,
    /// PR state filter: "open" | "closed" | "all" (default: open).
    #[serde(default)]
    pub state: Option<String>,
    /// Results per page (1–100, default 10).
    #[serde(default)]
    pub per_page: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetPullArgs {
    /// Repository (e.g. 'rust-alc-api').
    pub repo: String,
    /// PR number.
    pub pull_number: u64,
}

#[tool_router(router = pulls_router, vis = "pub(crate)")]
impl GithubMcp {
    /// List pull requests for a repository.
    #[tool(description = "List pull requests for a repository.")]
    async fn list_pull_requests(
        &self,
        Parameters(args): Parameters<ListPullsArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let state = args.state.unwrap_or_else(|| "open".to_string());
        let per_page = args.per_page.unwrap_or(10).clamp(1, 100);
        let path = format!("/repos/{}/{}/pulls", r.owner, r.repo);
        let prs: Vec<serde_json::Value> = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::GET,
            &path,
            &[("state", state), ("per_page", per_page.to_string())],
            None,
            &[],
        )
        .await?;
        let result: Vec<serde_json::Value> = prs
            .iter()
            .map(|pr| {
                serde_json::json!({
                    "number": pr.get("number"),
                    "title": pr.get("title"),
                    "state": pr.get("state"),
                    "author": pr.get("user").and_then(|u| u.get("login")),
                    "branch": pr.get("head").and_then(|h| h.get("ref")),
                    "base": pr.get("base").and_then(|b| b.get("ref")),
                    "created_at": pr.get("created_at"),
                    "updated_at": pr.get("updated_at"),
                    "url": pr.get("html_url"),
                    "draft": pr.get("draft"),
                    "mergeable_state": pr.get("mergeable_state"),
                })
            })
            .collect();
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Get PR details including CI check status.
    #[tool(description = "Get PR details including CI check status.")]
    async fn get_pull_request(
        &self,
        Parameters(args): Parameters<GetPullArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let pr_path = format!("/repos/{}/{}/pulls/{}", r.owner, r.repo, args.pull_number);
        let pr: serde_json::Value = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::GET,
            &pr_path,
            &[],
            None,
            &[],
        )
        .await?;
        let head_sha = pr
            .get("head")
            .and_then(|h| h.get("sha"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let checks: serde_json::Value = if !head_sha.is_empty() {
            github_api_json(
                &self.ctx().client,
                &self.ctx().github_token,
                Method::GET,
                &format!(
                    "/repos/{}/{}/commits/{}/check-runs",
                    r.owner, r.repo, head_sha
                ),
                &[],
                None,
                &[],
            )
            .await?
        } else {
            serde_json::json!({ "check_runs": [] })
        };
        let check_runs: Vec<serde_json::Value> = checks
            .get("check_runs")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|c| {
                        serde_json::json!({
                            "name": c.get("name"),
                            "status": c.get("status"),
                            "conclusion": c.get("conclusion"),
                            "url": c.get("html_url"),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let result = serde_json::json!({
            "number": pr.get("number"),
            "title": pr.get("title"),
            "state": pr.get("state"),
            "author": pr.get("user").and_then(|u| u.get("login")),
            "branch": pr.get("head").and_then(|h| h.get("ref")),
            "base": pr.get("base").and_then(|b| b.get("ref")),
            "mergeable": pr.get("mergeable"),
            "mergeable_state": pr.get("mergeable_state"),
            "created_at": pr.get("created_at"),
            "updated_at": pr.get("updated_at"),
            "url": pr.get("html_url"),
            "additions": pr.get("additions"),
            "deletions": pr.get("deletions"),
            "changed_files": pr.get("changed_files"),
            "checks": check_runs,
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Merge a pull request using squash merge.
    #[tool(description = "Merge a pull request using squash merge.")]
    async fn merge_pull_request(
        &self,
        Parameters(args): Parameters<MergePullArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let mut payload = serde_json::Map::new();
        payload.insert(
            "merge_method".into(),
            serde_json::Value::String("squash".into()),
        );
        if let Some(t) = args.commit_title {
            payload.insert("commit_title".into(), serde_json::Value::String(t));
        }
        let path = format!(
            "/repos/{}/{}/pulls/{}/merge",
            r.owner, r.repo, args.pull_number
        );
        let _: serde_json::Value = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::PUT,
            &path,
            &[],
            Some(&serde_json::Value::Object(payload)),
            &[],
        )
        .await?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "PR #{} merged (squash)",
            args.pull_number
        ))]))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MergePullArgs {
    /// Repository (e.g. 'rust-alc-api').
    pub repo: String,
    /// PR number.
    pub pull_number: u64,
    /// Custom commit title (optional).
    #[serde(default)]
    pub commit_title: Option<String>,
}
