//! Commits 読取り (ci-dashboard `src/mcp/tools/commits.ts` 移植)。

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

const MAX_PATCH_LINES: usize = 500;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListCommitsArgs {
    /// Repository (e.g. 'rust-alc-api').
    pub repo: String,
    /// Branch, tag, or SHA (default: "main").
    #[serde(default)]
    pub sha: Option<String>,
    /// Filter commits touching this file path.
    #[serde(default)]
    pub path: Option<String>,
    /// Results per page (1–100, default 20).
    #[serde(default)]
    pub per_page: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetCommitArgs {
    /// Repository (e.g. 'rust-alc-api').
    pub repo: String,
    /// Commit SHA (full or short).
    pub sha: String,
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

#[tool_router(router = commits_router, vis = "pub(crate)")]
impl GithubMcp {
    /// List commits for a repository. Supports branch/tag and file path filtering.
    #[tool(
        description = "List commits for a repository. Supports branch/tag and file path filtering."
    )]
    async fn list_commits(
        &self,
        Parameters(args): Parameters<ListCommitsArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let sha = args.sha.unwrap_or_else(|| "main".to_string());
        let per_page = args.per_page.unwrap_or(20).clamp(1, 100);
        let mut params: Vec<(&str, String)> =
            vec![("sha", sha), ("per_page", per_page.to_string())];
        if let Some(p) = args.path {
            params.push(("path", p));
        }
        let path = format!("/repos/{}/{}/commits", r.owner, r.repo);
        let commits: Vec<serde_json::Value> = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::GET,
            &path,
            &params,
            None,
            &[],
        )
        .await?;
        let result: Vec<serde_json::Value> = commits
            .iter()
            .map(|c| {
                let sha = c.get("sha").and_then(|v| v.as_str()).unwrap_or("");
                let commit = c.get("commit");
                let message = commit
                    .and_then(|c| c.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let author = commit.and_then(|c| c.get("author"));
                serde_json::json!({
                    "sha": short_sha(sha),
                    "message": message.split('\n').next().unwrap_or(""),
                    "author": author.and_then(|a| a.get("name")),
                    "date": author.and_then(|a| a.get("date")),
                })
            })
            .collect();
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Get commit details including changed files and diff patches.
    #[tool(description = "Get commit details including changed files and diff patches.")]
    async fn get_commit(
        &self,
        Parameters(args): Parameters<GetCommitArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let path = format!("/repos/{}/{}/commits/{}", r.owner, r.repo, args.sha);
        let commit: serde_json::Value = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::GET,
            &path,
            &[],
            None,
            &[],
        )
        .await?;
        let files: Vec<serde_json::Value> = commit
            .get("files")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|f| {
                        let patch = f.get("patch").and_then(|v| v.as_str()).unwrap_or("");
                        let lines: Vec<&str> = patch.split('\n').collect();
                        let truncated = if lines.len() > MAX_PATCH_LINES {
                            let mut head = lines[..MAX_PATCH_LINES].join("\n");
                            head.push_str(&format!(
                                "\n... (truncated, {} total lines)",
                                lines.len()
                            ));
                            head
                        } else {
                            patch.to_string()
                        };
                        serde_json::json!({
                            "filename": f.get("filename"),
                            "status": f.get("status"),
                            "additions": f.get("additions"),
                            "deletions": f.get("deletions"),
                            "patch": truncated,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let sha = commit.get("sha").and_then(|v| v.as_str()).unwrap_or("");
        let commit_obj = commit.get("commit");
        let message = commit_obj
            .and_then(|c| c.get("message"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let author = commit_obj.and_then(|c| c.get("author"));
        let result = serde_json::json!({
            "sha": short_sha(sha),
            "message": message,
            "author": author.and_then(|a| a.get("name")),
            "date": author.and_then(|a| a.get("date")),
            "stats": commit.get("stats"),
            "files": files,
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }
}
