//! MCP server (Streamable HTTP transport) — github_token を使って GitHub API を叩く
//! tool 群を expose する。
//!
//! 起動フロー (main.rs::run_serve から):
//!   1. token cache から MCP JWT を読む (expired なら refresh)
//!   2. `/mcp/introspect` を 1 回叩いて github_token + github_login を取得
//!   3. それらを `GithubMcp` 構造体に格納 → `StreamableHttpService` でラップ
//!   4. axum で `POST /mcp` を listen
//!
//! Token は in-memory cache のみ (1h で expire)。expire 後は再起動必要 (MVP)。
//! 将来: refresh & introspect を background task で定期更新。

use anyhow::Result;
use reqwest::Client;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ServerHandler,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

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
    ctx: Arc<GithubContext>,
    /// rmcp の `#[tool_handler]` macro が内部で参照するが、
    /// rust-analyzer の dead code 解析からは見えないので allow を付ける。
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
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

impl GithubMcp {
    pub fn new(ctx: Arc<GithubContext>) -> Self {
        Self {
            ctx,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
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
        let visibility = args.visibility.as_deref().unwrap_or("all");
        let per_page = args.per_page.unwrap_or(30).min(100);
        let page = args.page.unwrap_or(1);
        let url = format!(
            "https://api.github.com/user/repos?visibility={}&per_page={}&page={}",
            visibility, per_page, page,
        );
        let resp = self
            .ctx
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.ctx.github_token))
            .header("User-Agent", "github-mcp-server-rs")
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(format!("GitHub request: {e}"), None))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(format!("GitHub body: {e}"), None))?;
        if !status.is_success() {
            return Err(rmcp::ErrorData::internal_error(
                format!("GitHub /user/repos: HTTP {status} — {text}"),
                None,
            ));
        }

        // GitHub repo の各 entry から代表 field だけ抜き出して返す (token 浪費抑止)。
        let repos: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            rmcp::ErrorData::internal_error(format!("GitHub /user/repos parse: {e}"), None)
        })?;
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

#[tool_handler]
impl ServerHandler for GithubMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.instructions = Some(
            "GitHub MCP server backed by auth-worker (RFC 8628 device flow + introspect). \
             Tools available: whoami, list_repos. The github_token is auto-recovered \
             from auth-worker KV via /mcp/introspect at server startup."
                .into(),
        );
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}
