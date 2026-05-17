//! Tags / releases 読取り — ci-dashboard `src/mcp/tools/releases.ts` 移植 (read のみ)。

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
pub struct ListTagsArgs {
    /// Repository (e.g. 'rust-alc-api').
    pub repo: String,
    /// Results per page (1–100, default 10).
    #[serde(default)]
    pub per_page: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetLatestReleaseArgs {
    /// Repository (e.g. 'rust-alc-api').
    pub repo: String,
}

#[tool_router(router = releases_router, vis = "pub(crate)")]
impl GithubMcp {
    /// List tags for a repository.
    #[tool(description = "List tags for a repository.")]
    async fn list_tags(
        &self,
        Parameters(args): Parameters<ListTagsArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let per_page = args.per_page.unwrap_or(10).clamp(1, 100);
        let tags: Vec<serde_json::Value> = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::GET,
            &format!("/repos/{}/{}/tags", r.owner, r.repo),
            &[("per_page", per_page.to_string())],
            None,
            &[],
        )
        .await?;
        let result: Vec<serde_json::Value> = tags
            .iter()
            .map(|t| {
                let sha = t
                    .get("commit")
                    .and_then(|c| c.get("sha"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                serde_json::json!({
                    "name": t.get("name"),
                    "sha": sha.chars().take(7).collect::<String>(),
                })
            })
            .collect();
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Get the latest release for a repository.
    #[tool(description = "Get the latest release for a repository.")]
    async fn get_latest_release(
        &self,
        Parameters(args): Parameters<GetLatestReleaseArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let release: serde_json::Value = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::GET,
            &format!("/repos/{}/{}/releases/latest", r.owner, r.repo),
            &[],
            None,
            &[],
        )
        .await?;
        let body_snippet = release
            .get("body")
            .and_then(|v| v.as_str())
            .map(|s| s.chars().take(500).collect::<String>());
        let result = serde_json::json!({
            "tag": release.get("tag_name"),
            "name": release.get("name"),
            "published_at": release.get("published_at"),
            "author": release.get("author").and_then(|a| a.get("login")),
            "url": release.get("html_url"),
            "body": body_snippet,
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }
}
