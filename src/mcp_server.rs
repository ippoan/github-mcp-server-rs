//! MCP server (Streamable HTTP transport) — github_token を使って GitHub API を叩く
//! tool 群を expose する。
//!
//! このファイルでは以下を担う:
//!   - `GithubContext` / `GithubMcp` 構造体 (state + Clone factory)
//!   - "core" router: `whoami` (ctx 即返し) と `list_repos` (`/user/repos`)
//!   - `ServerHandler` 実装 (`get_info`)
//!
//! ci-dashboard 由来の category 別 tool は `crate::tools::{actions, commits,
//! issues, logs, pulls, releases, repository}` にあり、`GithubMcp::new` で
//! `+` operator (`rmcp::ToolRouter: Add`) で全部足し合わせている。
//!
//! Token は in-memory cache のみ (1h で expire)。expire 後は再起動必要 (MVP)。
//! 将来: refresh & introspect を background task で定期更新。

use reqwest::{Client, Method};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ServerHandler,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::github_api::github_api_json;

/// MCP server で共有する不変 state (起動時に固定)。
#[derive(Clone)]
pub struct GithubContext {
    pub github_token: String,
    pub github_login: String,
    pub scope: String,
    pub client: Client,
}

/// rmcp は service factory を呼んで新インスタンスを作る前提なので Context を Arc
/// で持ち、`Clone` で安く複製できるようにする。
#[derive(Clone)]
pub struct GithubMcp {
    pub(crate) ctx: Arc<GithubContext>,
    /// rmcp の `#[tool_handler]` macro が内部で参照するが、
    /// rust-analyzer の dead code 解析からは見えないので allow を付ける。
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl GithubMcp {
    pub fn new(ctx: Arc<GithubContext>) -> Self {
        // `core_router` (このファイル) + category routers を ToolRouter::add で合成。
        // どの module も `#[tool_router(router = X_router, vis = "pub(crate)")]` で
        // `Self::X_router()` 形式の inherent fn を生やしている。
        let tool_router = Self::core_router()
            + Self::actions_router()
            + Self::commits_router()
            + Self::issues_router()
            + Self::logs_router()
            + Self::projects_router()
            + Self::pulls_router()
            + Self::releases_router()
            + Self::repository_router();
        Self { ctx, tool_router }
    }

    /// `crate::tools::*` から `ctx` を読むための共通アクセサ。
    pub(crate) fn ctx(&self) -> &GithubContext {
        &self.ctx
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ListReposArgs {
    /// "all" | "public" | "private" (GitHub `/user/repos` の visibility)。default "all".
    #[serde(default)]
    pub visibility: Option<String>,
    /// 1〜100 (GitHub default 30, max 100)。
    #[serde(default)]
    pub per_page: Option<u32>,
    /// 1-indexed page number。
    #[serde(default)]
    pub page: Option<u32>,
}

#[tool_router(router = core_router, vis = "pub(crate)")]
impl GithubMcp {
    /// Return the GitHub user associated with the cached MCP JWT.
    /// Useful as a sanity check that the token is valid and which account is being used.
    #[tool(
        description = "Return the authenticated GitHub user (login + scope) for this MCP session."
    )]
    async fn whoami(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let body = serde_json::json!({
            "github_login": &self.ctx.github_login,
            "scope": &self.ctx.scope,
        });
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }

    /// List repositories the authenticated user has explicit access to.
    /// Calls GitHub `GET /user/repos`.
    #[tool(
        description = "List GitHub repositories accessible to the authenticated user (paginated)."
    )]
    async fn list_repos(
        &self,
        Parameters(args): Parameters<ListReposArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let visibility = args.visibility.unwrap_or_else(|| "all".to_string());
        let per_page = args.per_page.unwrap_or(30).min(100);
        let page = args.page.unwrap_or(1);
        let repos: serde_json::Value = github_api_json(
            &self.ctx.client,
            &self.ctx.github_token,
            Method::GET,
            "/user/repos",
            &[
                ("visibility", visibility),
                ("per_page", per_page.to_string()),
                ("page", page.to_string()),
            ],
            None,
            &[],
        )
        .await?;
        let mut summary: Vec<serde_json::Value> = Vec::new();
        if let Some(arr) = repos.as_array() {
            for r in arr {
                summary.push(serde_json::json!({
                    "full_name": r.get("full_name"),
                    "private": r.get("private"),
                    "description": r.get("description"),
                    "html_url": r.get("html_url"),
                    "default_branch": r.get("default_branch"),
                    "language": r.get("language"),
                    "stargazers_count": r.get("stargazers_count"),
                    "pushed_at": r.get("pushed_at"),
                }));
            }
        }
        let body = serde_json::json!({
            "page": page,
            "per_page": per_page,
            "count": summary.len(),
            "repos": summary,
        });
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for GithubMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.instructions = Some(
            "GitHub MCP server backed by auth-worker (RFC 8628 device flow + introspect). \
             Tools: whoami, list_repos plus ci-dashboard-derived read tools \
             (workflow runs / commits / issues / job logs / pull requests / tags / repository). \
             The github_token is auto-recovered from auth-worker KV via /mcp/introspect at \
             server startup."
                .into(),
        );
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}
