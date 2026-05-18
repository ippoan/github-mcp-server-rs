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
//! ## Scope-based router factory (auth-worker#148 / consumer of `mcp.admin`)
//!
//! 1 binary 1 user 1 JWT の設計上、`ctx.scope` (= `/mcp/introspect` が返した JWT
//! claim) を single source of truth として tool surface を出し分ける。
//!
//! - `mcp.admin`        → `branches_router` (set/get/delete_branch_protection)
//! - `mcp.read|write`   → 既存の read/write router 群
//! - 両方                 → union (テスト用の superuser セッション)
//! - どちらも含まない → core のみ (defense-in-depth: whoami + list_repos)
//!
//! `mcp.admin` と read/write は **disjoint** として設計されているため、admin
//! セッションからは issue / PR / push 系の副作用が出てこない (⚠ admin JWT を
//! 誤って leak しても branch protection 以外は叩けない) — blast radius を最小化。
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
    /// MCP JWT の `scope` claim (space-separated, e.g. `"mcp.read mcp.write"` or
    /// `"mcp.admin"`)。`GithubMcp::new` の router factory がこの値を見て tool
    /// subset を出し分けるため、auth-worker `/mcp/introspect` の戻り値を
    /// そのまま詰めること (re-normalize しない)。
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

/// `scope_str` (space-separated MCP scope claim) に `target` token が含まれるか。
/// substring match ではなく、whitespace で区切った token 単位の厳密一致。
///
/// `"mcp.admin-x"` や `"xmcp.admin"` は match しない (defense-in-depth)。
fn scope_has(scope_str: &str, target: &str) -> bool {
    scope_str.split_whitespace().any(|s| s == target)
}

impl GithubMcp {
    pub fn new(ctx: Arc<GithubContext>) -> Self {
        // 3 段階 scope factory (admin と read/write は disjoint):
        //   - mcp.admin       → branches_router (set/get/delete_branch_protection)
        //   - mcp.read|write  → 既存の read/write router (actions/commits/issues/...)
        //   - core (whoami / list_repos) は常に含む
        //
        // 両方 set の JWT (superuser) は union になるが、通常は auth-worker pair flow で
        // requested_scope="mcp.admin" or "mcp.read mcp.write" のどちらかだけを mint する。
        let admin = scope_has(&ctx.scope, "mcp.admin");
        let read_or_write =
            scope_has(&ctx.scope, "mcp.read") || scope_has(&ctx.scope, "mcp.write");

        let mut tool_router = Self::core_router();
        if admin {
            tool_router = tool_router + Self::branches_router();
        }
        if read_or_write {
            tool_router = tool_router
                + Self::actions_router()
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
             (workflow runs / commits / issues / job logs / pull requests / tags / repository); \
             `mcp.admin` exposes branch protection tools \
             (set/get/delete_branch_protection) instead. \
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
        let ctx = Arc::new(GithubContext {
            github_token: "x".to_string(),
            github_login: "x".to_string(),
            scope: scope.to_string(),
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
        // branch protection tools は read+write JWT では見えないこと (defense-in-depth)
        assert!(
            !names.iter().any(|n| n == "set_branch_protection"),
            "set_branch_protection leaked into mcp.read mcp.write scope"
        );
        assert!(
            !names.iter().any(|n| n == "get_branch_protection"),
            "get_branch_protection leaked into mcp.read mcp.write scope"
        );
        assert!(
            !names.iter().any(|n| n == "delete_branch_protection"),
            "delete_branch_protection leaked into mcp.read mcp.write scope"
        );
        // read+write には core (2) + 8 カテゴリの tool が含まれるので、最低限
        // ちゃんと量があることを確認 (正確な個数は tool 追加で変わるので >= だけ使う)。
        assert!(
            names.len() >= 10,
            "expected at least 10 tools for mcp.read mcp.write, got {}: {:?}",
            names.len(),
            names
        );
    }

    #[test]
    fn admin_only_scope_exposes_branch_protection_no_other_categories() {
        let mcp = build_mcp("mcp.admin");
        let names = tool_names(&mcp);
        eprintln!(
            "TOOL_DUMP (mcp.admin) count={} names={:?}",
            names.len(),
            names
        );
        // core は常に expose
        assert!(names.iter().any(|n| n == "whoami"), "whoami missing");
        assert!(
            names.iter().any(|n| n == "list_repos"),
            "list_repos missing"
        );
        // branch protection 3 tools が expose されている
        assert!(
            names.iter().any(|n| n == "set_branch_protection"),
            "set_branch_protection missing for mcp.admin"
        );
        assert!(
            names.iter().any(|n| n == "get_branch_protection"),
            "get_branch_protection missing for mcp.admin"
        );
        assert!(
            names.iter().any(|n| n == "delete_branch_protection"),
            "delete_branch_protection missing for mcp.admin"
        );
        // それ以外のカテゴリ (actions/commits/issues/logs/projects/pulls/releases/repository) は一切見えないこと。
        // 代表例として既知の tool 1 個づつを選んで negative assert するのではなく、
        // count で一括チェック: core 2 + branches 3 = 5 のはず。
        assert_eq!(
            names.len(),
            5,
            "expected exactly 5 tools (2 core + 3 branches) for mcp.admin, got {}: {:?}",
            names.len(),
            names
        );
    }

    #[test]
    fn admin_and_write_combined_scope_exposes_union() {
        let mcp = build_mcp("mcp.read mcp.write mcp.admin");
        let names = tool_names(&mcp);
        // 両方含まれること
        assert!(
            names.iter().any(|n| n == "set_branch_protection"),
            "admin tools missing from union"
        );
        assert!(names.iter().any(|n| n == "whoami"), "core missing");
        // count は admin-only (5) + read+write categories の合計以上
        let admin_only = tool_names(&build_mcp("mcp.admin")).len();
        let rw_only = tool_names(&build_mcp("mcp.read mcp.write")).len();
        // union = admin_subset ∪ rw_subset = (admin_only + rw_only - core_overlap_2)
        assert_eq!(names.len(), admin_only + rw_only - 2);
    }

    #[test]
    fn empty_scope_exposes_core_only() {
        let mcp = build_mcp("");
        let names = tool_names(&mcp);
        // defense-in-depth: 不明な JWT は core のみ、誤って admin tool に到達しない
        assert_eq!(
            names,
            vec!["list_repos".to_string(), "whoami".to_string()],
            "empty scope should expose only core tools"
        );
    }

    #[test]
    fn unknown_scope_exposes_core_only() {
        // 以前の build_mcp() が使っていた scope="x" に相当。
        let mcp = build_mcp("x garbage");
        let names = tool_names(&mcp);
        assert_eq!(names.len(), 2);
        assert!(names.iter().any(|n| n == "whoami"));
        assert!(names.iter().any(|n| n == "list_repos"));
    }

    #[test]
    fn read_only_scope_exposes_read_write_routers() {
        // mcp.read のみでも read+write のカテゴリを見せる (この PR では read/write
        // 内部のカテゴリ分離はしない、という設計判断。将来 mcp.read だけでは write 系
        // (create_issue 等) を hide するよう tool レベルで filter を掛けると better、
        // だがこれは Track A のスコープ外。
        let mcp = build_mcp("mcp.read");
        let names = tool_names(&mcp);
        assert!(names.iter().any(|n| n == "whoami"));
        // branch protection は見えない
        assert!(!names.iter().any(|n| n == "set_branch_protection"));
        // リードカテゴリの tool は見える (>= 10)
        assert!(names.len() >= 10);
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
