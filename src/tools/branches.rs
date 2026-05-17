//! Branch protection tools — wraps the GitHub Branches API
//! (`/repos/{owner}/{repo}/branches/{branch}/protection`).
//!
//! All three tools require the caller token to have `administration:write`
//! on the target repo. `parse_and_validate_repo` enforces the org allowlist
//! before any API call goes out.

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
    /// Also enforce protection for admins. Default: false (admins can bypass).
    #[serde(default)]
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
    /// Apply branch protection.  Calls `PUT /repos/{owner}/{repo}/branches/{branch}/protection`.
    /// Requires `administration:write` on the caller token.
    #[tool(
        description = "Apply or update branch protection on a repository branch. Requires administration:write scope. See SetBranchProtectionArgs for the rule knobs (required checks, conversation resolution, force-push / deletion gates, optional review requirement)."
    )]
    async fn set_branch_protection(
        &self,
        Parameters(args): Parameters<SetBranchProtectionArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;

        let mut payload = serde_json::Map::new();

        let checks: Vec<String> = args
            .required_checks
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if checks.is_empty() {
            payload.insert("required_status_checks".into(), serde_json::Value::Null);
        } else {
            payload.insert(
                "required_status_checks".into(),
                serde_json::json!({
                    "strict": args.strict_required_checks,
                    "contexts": checks,
                }),
            );
        }

        match args.required_approving_review_count {
            Some(n) if n > 0 => {
                payload.insert(
                    "required_pull_request_reviews".into(),
                    serde_json::json!({
                        "required_approving_review_count": n,
                        "dismiss_stale_reviews": args.dismiss_stale_reviews,
                    }),
                );
            }
            _ => {
                payload.insert(
                    "required_pull_request_reviews".into(),
                    serde_json::Value::Null,
                );
            }
        }

        payload.insert(
            "enforce_admins".into(),
            serde_json::Value::Bool(args.enforce_admins),
        );
        payload.insert("restrictions".into(), serde_json::Value::Null);
        payload.insert(
            "required_linear_history".into(),
            serde_json::Value::Bool(args.required_linear_history),
        );
        payload.insert(
            "allow_force_pushes".into(),
            serde_json::Value::Bool(args.allow_force_pushes),
        );
        payload.insert(
            "allow_deletions".into(),
            serde_json::Value::Bool(args.allow_deletions),
        );
        payload.insert(
            "required_conversation_resolution".into(),
            serde_json::Value::Bool(args.required_conversation_resolution),
        );

        let path = format!(
            "/repos/{}/{}/branches/{}/protection",
            r.owner, r.repo, args.branch
        );
        let resp: serde_json::Value = github_api_json(
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
            "Branch protection applied on {}/{}@{}\n\n{}",
            r.owner,
            r.repo,
            args.branch,
            serde_json::to_string_pretty(&resp).unwrap_or_else(|_| resp.to_string())
        ))]))
    }

    /// Fetch the current branch protection.
    /// Returns the raw GitHub response so callers can diff before re-applying.
    #[tool(
        description = "Get the current branch protection settings for a branch. Returns the raw GitHub API response."
    )]
    async fn get_branch_protection(
        &self,
        Parameters(args): Parameters<GetBranchProtectionArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let path = format!(
            "/repos/{}/{}/branches/{}/protection",
            r.owner, r.repo, args.branch
        );
        let resp: serde_json::Value = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::GET,
            &path,
            &[],
            None,
            &[],
        )
        .await?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&resp).unwrap_or_else(|_| resp.to_string()),
        )]))
    }

    /// Remove all branch protection from a branch.
    #[tool(
        description = "Remove branch protection from a branch. Requires administration:write scope."
    )]
    async fn delete_branch_protection(
        &self,
        Parameters(args): Parameters<DeleteBranchProtectionArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let path = format!(
            "/repos/{}/{}/branches/{}/protection",
            r.owner, r.repo, args.branch
        );
        let _: serde_json::Value = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::DELETE,
            &path,
            &[],
            None,
            &[],
        )
        .await?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Branch protection removed from {}/{}@{}",
            r.owner, r.repo, args.branch
        ))]))
    }
}
