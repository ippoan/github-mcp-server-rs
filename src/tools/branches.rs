//! Branch protection tools — proxy through auth-worker `/mcp/admin/exec`.
//!
//! Phase 2: the binary no longer calls the GitHub Branches API directly.
//! Instead, each tool POSTs `{tool, args}` to auth-worker, which holds the
//! high-privilege GitHub App installation token and gates the call behind a
//! short-lived (15min) browser-issued elevate flag. Calling an admin tool
//! without an active elevate flag returns a user-facing error pointing at
//! `/mcp/elevate`.

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::admin_exec::{admin_exec_with_refresh, to_rmcp_error};
use crate::github_api::parse_and_validate_repo;
use crate::mcp_server::GithubMcp;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetBranchProtectionArgs {
    /// Repository (e.g. 'cc-relay' or 'ippoan/cc-relay').
    pub repo: String,
    /// Branch to protect (e.g. 'main', 'master').
    pub branch: String,
    /// Required status check contexts (CI job names) that must pass before
    /// merging. Empty (or omitted) → no required_status_checks block is sent,
    /// which is what repos without CI (e.g. claude-md) want.
    #[serde(default)]
    pub required_checks: Option<Vec<String>>,
    /// Require branches to be up to date before merging. Default: true.
    #[serde(default = "default_true")]
    pub strict_required_checks: bool,
    /// Also enforce protection for admins. Default: true (admins are NOT
    /// allowed to bypass — solo-dev repos rely on this to keep the gate
    /// real). Pass `false` explicitly to let admins bypass.
    #[serde(default = "default_true")]
    pub enforce_admins: bool,
    /// Block merge until all review threads are resolved. Default: true.
    #[serde(default = "default_true")]
    pub required_conversation_resolution: bool,
    /// Allow force pushes to the protected branch. Default: false.
    #[serde(default)]
    pub allow_force_pushes: bool,
    /// Allow the protected branch to be deleted. Default: false.
    #[serde(default)]
    pub allow_deletions: bool,
    /// Require linear history (no merge commits). Default: false.
    #[serde(default)]
    pub required_linear_history: bool,
    /// Require N approving reviews before merge. None / 0 → no review
    /// requirement (the `required_pull_request_reviews` block is omitted).
    #[serde(default)]
    pub required_approving_review_count: Option<u32>,
    /// Dismiss stale approvals when new commits are pushed. Only meaningful
    /// when `required_approving_review_count > 0`. Default: false.
    #[serde(default)]
    pub dismiss_stale_reviews: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetBranchProtectionArgs {
    /// Repository (e.g. 'cc-relay' or 'ippoan/cc-relay').
    pub repo: String,
    /// Branch (e.g. 'main').
    pub branch: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteBranchProtectionArgs {
    /// Repository (e.g. 'cc-relay' or 'ippoan/cc-relay').
    pub repo: String,
    /// Branch (e.g. 'main').
    pub branch: String,
}

#[tool_router(router = branches_router, vis = "pub(crate)")]
impl GithubMcp {
    /// Apply branch protection. Proxies to auth-worker `/mcp/admin/exec` which
    /// performs the actual `PUT /repos/{owner}/{repo}/branches/{branch}/protection`
    /// with a GitHub App installation token (server-side).
    #[tool(
        description = "Apply or update branch protection on a repository branch. Proxied via auth-worker /mcp/admin/exec; requires a browser-issued elevate flag (15min TTL). See SetBranchProtectionArgs for the rule knobs (required checks, conversation resolution, force-push / deletion gates, optional review requirement)."
    )]
    async fn set_branch_protection(
        &self,
        Parameters(args): Parameters<SetBranchProtectionArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;

        // Build the full args payload sent to auth-worker. The worker
        // re-validates owner/repo/branch and assembles the GitHub PUT body.
        let checks: Vec<String> = args
            .required_checks
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let payload = json!({
            "owner": r.owner,
            "repo": r.repo,
            "branch": args.branch,
            "required_checks": checks,
            "strict_required_checks": args.strict_required_checks,
            "enforce_admins": args.enforce_admins,
            "required_conversation_resolution": args.required_conversation_resolution,
            "allow_force_pushes": args.allow_force_pushes,
            "allow_deletions": args.allow_deletions,
            "required_linear_history": args.required_linear_history,
            "required_approving_review_count": args.required_approving_review_count,
            "dismiss_stale_reviews": args.dismiss_stale_reviews,
        });

        let resp: Value = admin_exec_with_refresh(self.ctx(), "set_branch_protection", payload)
            .await
            .map_err(to_rmcp_error)?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Branch protection applied on {}/{}@{}\n\n{}",
            r.owner,
            r.repo,
            args.branch,
            serde_json::to_string_pretty(&resp).unwrap_or_else(|_| resp.to_string())
        ))]))
    }

    /// Fetch the current branch protection. Proxies to auth-worker.
    #[tool(
        description = "Get the current branch protection settings for a branch. Proxied via auth-worker /mcp/admin/exec; requires a browser-issued elevate flag. Returns the raw GitHub API response."
    )]
    async fn get_branch_protection(
        &self,
        Parameters(args): Parameters<GetBranchProtectionArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let resp: Value = admin_exec_with_refresh(
            self.ctx(),
            "get_branch_protection",
            json!({
                "owner": r.owner,
                "repo": r.repo,
                "branch": args.branch,
            }),
        )
        .await
        .map_err(to_rmcp_error)?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&resp).unwrap_or_else(|_| resp.to_string()),
        )]))
    }

    /// Remove all branch protection from a branch. Proxies to auth-worker.
    #[tool(
        description = "Remove branch protection from a branch. Proxied via auth-worker /mcp/admin/exec; requires a browser-issued elevate flag (15min TTL)."
    )]
    async fn delete_branch_protection(
        &self,
        Parameters(args): Parameters<DeleteBranchProtectionArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let _: Value = admin_exec_with_refresh(
            self.ctx(),
            "delete_branch_protection",
            json!({
                "owner": r.owner,
                "repo": r.repo,
                "branch": args.branch,
            }),
        )
        .await
        .map_err(to_rmcp_error)?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Branch protection removed from {}/{}@{}",
            r.owner, r.repo, args.branch
        ))]))
    }
}
