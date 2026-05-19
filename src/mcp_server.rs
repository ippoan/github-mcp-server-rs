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
//! `+` operator (`rmcp::ToolRouter: Add`) で scope に応じて subset を足し合わせる。
//!
//! ## Router factory
//!
//! - core (whoami / list_repos) は常に expose
//! - branches_router (set/get/delete_branch_protection) も **常に** expose。
//!   authorization は auth-worker `/mcp/admin/exec` の elevate flag (15min TTL,
//!   browser-based one-tap) で server-side に行う。binary は proxy するだけ。
//! - `mcp.read|write` scope → 既存の read/write router 群 (actions/commits/...)
//!
//! Token は `Arc<RwLock<TokenSet>>` で relay と共有。admin tool 経路は
//! `admin_exec::admin_exec_with_refresh` 経由で expiry を pre-check し、必要なら
//! `auth::refresh()` で逐次更新する (issue: JWT expiry mid-session)。

use reqwest::{Client, Method};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ServerHandler,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::Config;
use crate::github_api::github_api_json;
use crate::token_cache::TokenSet;

/// MCP server で共有する state。
///
/// `token` は relay loop と同じ `Arc<RwLock<TokenSet>>` を握る — どちらの経路で
/// refresh しても他方が次回 read で最新値を見る。`cfg` と `token_cache_path` は
/// `admin_exec_with_refresh` が `auth::refresh()` を呼ぶのに必要。
#[derive(Clone)]
pub struct GithubContext {
    pub github_token: String,
    pub github_login: String,
    /// MCP JWT の `scope` claim (space-separated, e.g. `"mcp.read mcp.write"` or
    /// `"mcp.admin"`)。read/write 系 tool surface の judgement に使う。
    /// admin tool は scope に依存せず常に expose され、authorization は
    /// auth-worker `/mcp/admin/exec` 側の elevate flag で行われる。
    pub scope: String,
    /// MCP JWT + refresh_token (relay loop と共有)。admin tool は read lock を
    /// 取って access_token を `Authorization: Bearer <jwt>` として送る。expiry が
    /// 近い / 401 のとき `admin_exec_with_refresh` が write lock を取って refresh。
    pub token: Arc<RwLock<TokenSet>>,
    /// refresh 成功時に新 TokenSet を persist する先 (relay と同じ path を共有)。
    pub token_cache_path: PathBuf,
    /// `auth::refresh()` / `admin_exec` の URL 組立に使う。`cfg.auth_base` が
    /// `{auth_worker_origin}` の役割を兼ねる。
    pub cfg: Arc<Config>,
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

/// `scope_str` (space-separated MCP scope claim) に `target` token が含まれるか。
/// substring match ではなく、whitespace で区切った token 単位の厳密一致。
///
/// `"mcp.admin-x"` や `"xmcp.admin"` は match しない (defense-in-depth)。
fn scope_has(scope_str: &str, target: &str) -> bool {
    scope_str.split_whitespace().any(|s| s == target)
}

impl GithubMcp {
    pub fn new(ctx: Arc<GithubContext>) -> Self {
        // Admin tools (branch protection) are always exposed in the router.
        // Authorization is enforced server-side via the auth-worker elevate flag
        // (`POST /mcp/admin/exec`): calling an admin tool without an active
        // elevate flag returns 403 from auth-worker, which we surface as a
        // user-facing error pointing at the elevate URL. The `mcp.admin` JWT
        // scope is no longer used as a gate on the binary side.
        let read_or_write = scope_has(&ctx.scope, "mcp.read") || scope_has(&ctx.scope, "mcp.write");

        let mut tool_router = Self::core_router() + Self::branches_router();
        if read_or_write {
            tool_router += Self::actions_router()
                + Self::commits_router()
                + Self::issues_router()
                + Self::logs_router()
                + Self::projects_router()
                + Self::pulls_router()
                + Self::releases_router()
                + Self::repository_router();
        }
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
             Tools surface depends on the JWT scope claim: \
             `mcp.read|write` exposes ci-dashboard-derived read/write tools \
             (workflow runs / commits / issues / job logs / pull requests / tags / repository). \
             Admin tools (set/get/delete_branch_protection) are always available; \
             authorization is enforced server-side by auth-worker via a browser-based \
             elevate flow (`/mcp/elevate`, 15min TTL). \
             whoami and list_repos are always available. \
             The github_token is auto-recovered from auth-worker KV via /mcp/introspect at \
             server startup."
                .into(),
        );
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_mcp(scope: &str) -> GithubMcp {
        use crate::config::AuthEnv;
        use chrono::Utc;
        let cfg = Arc::new(Config {
            env: AuthEnv::Staging,
            auth_base: "https://auth.test.invalid".to_string(),
            relay_base: "https://mcp.test.invalid".to_string(),
            internal_shared_secret: "x".into(),
            client_id: "github-mcp-server-rs".into(),
            scope: scope.to_string(),
            project_name: "github-mcp-server-rs",
        });
        let token = Arc::new(RwLock::new(TokenSet {
            access_token: "test-jwt".into(),
            refresh_token: "test-refresh".into(),
            scope: scope.to_string(),
            expires_at: Utc::now().timestamp() + 3600,
            obtained_at: Utc::now(),
        }));
        let ctx = Arc::new(GithubContext {
            github_token: "x".to_string(),
            github_login: "x".to_string(),
            scope: scope.to_string(),
            token,
            token_cache_path: PathBuf::from("/tmp/test-token.json"),
            cfg,
            client: Client::new(),
        });
        GithubMcp::new(ctx)
    }

    fn tool_names(mcp: &GithubMcp) -> Vec<String> {
        let mut names: Vec<String> = mcp
            .tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn dump_registered_tool_names_for_read_write_scope() {
        let mcp = build_mcp("mcp.read mcp.write");
        let names = tool_names(&mcp);
        eprintln!(
            "TOOL_DUMP (mcp.read mcp.write) count={} names={:?}",
            names.len(),
            names
        );
        assert!(names.contains(&"whoami".to_string()), "whoami missing");
        assert!(
            names.contains(&"list_repos".to_string()),
            "list_repos missing"
        );
        // Admin tools are now always exposed (server-side authz via auth-worker
        // `/mcp/admin/exec` elevate flag). Calling them without elevation
        // returns a 403 surfaced as a user-friendly error.
        assert!(
            names.iter().any(|n| n == "set_branch_protection"),
            "set_branch_protection should always be exposed"
        );
        assert!(
            names.iter().any(|n| n == "get_branch_protection"),
            "get_branch_protection should always be exposed"
        );
        assert!(
            names.iter().any(|n| n == "delete_branch_protection"),
            "delete_branch_protection should always be exposed"
        );
        // read+write には core (2) + branches (3) + 8 カテゴリの tool が含まれる。
        assert!(
            names.len() >= 13,
            "expected at least 13 tools for mcp.read mcp.write, got {}: {:?}",
            names.len(),
            names
        );
    }

    #[test]
    fn branches_always_exposed_regardless_of_scope() {
        // Admin tools are always in the router; authorization is enforced
        // server-side by auth-worker. Verify across multiple scope strings.
        for scope in [
            "",
            "x garbage",
            "mcp.read",
            "mcp.read mcp.write",
            "mcp.admin",
            "mcp.read mcp.write mcp.admin",
        ] {
            let mcp = build_mcp(scope);
            let names = tool_names(&mcp);
            assert!(
                names.iter().any(|n| n == "set_branch_protection"),
                "set_branch_protection missing for scope={scope:?}"
            );
            assert!(
                names.iter().any(|n| n == "get_branch_protection"),
                "get_branch_protection missing for scope={scope:?}"
            );
            assert!(
                names.iter().any(|n| n == "delete_branch_protection"),
                "delete_branch_protection missing for scope={scope:?}"
            );
            assert!(names.iter().any(|n| n == "whoami"));
            assert!(names.iter().any(|n| n == "list_repos"));
        }
    }

    #[test]
    fn empty_scope_exposes_core_plus_branches_only() {
        let mcp = build_mcp("");
        let names = tool_names(&mcp);
        // No read/write surface, but core + branches always expose.
        // core (2) + branches (3) = 5
        assert_eq!(
            names.len(),
            5,
            "expected exactly 5 tools (2 core + 3 branches) for empty scope, got {}: {:?}",
            names.len(),
            names
        );
        assert!(names.iter().any(|n| n == "whoami"));
        assert!(names.iter().any(|n| n == "list_repos"));
        assert!(names.iter().any(|n| n == "set_branch_protection"));
    }

    #[test]
    fn unknown_scope_exposes_core_plus_branches_only() {
        let mcp = build_mcp("x garbage");
        let names = tool_names(&mcp);
        assert_eq!(names.len(), 5);
        assert!(names.iter().any(|n| n == "whoami"));
        assert!(names.iter().any(|n| n == "list_repos"));
        assert!(names.iter().any(|n| n == "set_branch_protection"));
    }

    #[test]
    fn read_only_scope_exposes_read_write_routers() {
        // mcp.read のみでも read+write のカテゴリを見せる (この PR では read/write
        // 内部のカテゴリ分離はしない、という設計判断)。
        let mcp = build_mcp("mcp.read");
        let names = tool_names(&mcp);
        assert!(names.iter().any(|n| n == "whoami"));
        // admin tools are always exposed
        assert!(names.iter().any(|n| n == "set_branch_protection"));
        // リードカテゴリの tool は見える (>= 13 = 2 core + 3 branches + 8 categories)
        assert!(names.len() >= 13);
    }

    #[test]
    fn scope_has_exact_token_match() {
        assert!(scope_has("mcp.read mcp.write", "mcp.read"));
        assert!(scope_has("mcp.read mcp.write", "mcp.write"));
        assert!(scope_has("mcp.admin", "mcp.admin"));
        assert!(scope_has("  mcp.admin  ", "mcp.admin")); // leading/trailing ws スキップ
        assert!(scope_has("a b mcp.admin c", "mcp.admin"));
    }

    #[test]
    fn scope_has_rejects_substring_match() {
        assert!(!scope_has("mcp.admin-x", "mcp.admin"));
        assert!(!scope_has("xmcp.admin", "mcp.admin"));
        assert!(!scope_has("mcp.admins", "mcp.admin"));
        assert!(!scope_has("", "mcp.admin"));
        assert!(!scope_has("mcp.read", "mcp.admin"));
    }
}
