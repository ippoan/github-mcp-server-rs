//! Workflow runs / jobs (ci-dashboard `src/mcp/tools/actions.ts` 移植)。read + write 両方。

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
pub struct ListWorkflowRunsArgs {
    /// Repository (e.g. 'rust-alc-api' or 'ippoan/rust-alc-api').
    pub repo: String,
    /// Filter by status: "queued" | "in_progress" | "completed".
    #[serde(default)]
    pub status: Option<String>,
    /// Results per page (1–100, default 10).
    #[serde(default)]
    pub per_page: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkflowRunIdArgs {
    /// Repository (e.g. 'rust-alc-api').
    pub repo: String,
    /// Workflow run ID.
    pub run_id: u64,
}

fn workflow_run_summary(r: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "id": r.get("id"),
        "name": r.get("name"),
        "status": r.get("status"),
        "conclusion": r.get("conclusion"),
        "branch": r.get("head_branch"),
        "actor": r.get("actor").and_then(|a| a.get("login")),
        "created_at": r.get("created_at"),
        "updated_at": r.get("updated_at"),
        "url": r.get("html_url"),
    })
}

#[tool_router(router = actions_router, vis = "pub(crate)")]
impl GithubMcp {
    /// List recent workflow runs for a repository.
    #[tool(
        description = "List recent workflow runs for a repository. Use ci-dashboard UI for real-time monitoring instead of polling."
    )]
    async fn list_workflow_runs(
        &self,
        Parameters(args): Parameters<ListWorkflowRunsArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let per_page = args.per_page.unwrap_or(10).clamp(1, 100);
        let mut params: Vec<(&str, String)> = vec![("per_page", per_page.to_string())];
        if let Some(status) = args.status.as_deref() {
            params.push(("status", status.to_string()));
        }
        let path = format!("/repos/{}/{}/actions/runs", r.owner, r.repo);
        let data: serde_json::Value = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::GET,
            &path,
            &params,
            None,
            &[],
        )
        .await?;
        let runs: Vec<serde_json::Value> = data
            .get("workflow_runs")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().map(workflow_run_summary).collect())
            .unwrap_or_default();
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&runs).unwrap_or_default(),
        )]))
    }

    /// Get details of a specific workflow run.
    #[tool(description = "Get details of a specific workflow run.")]
    async fn get_workflow_run(
        &self,
        Parameters(args): Parameters<WorkflowRunIdArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let path = format!("/repos/{}/{}/actions/runs/{}", r.owner, r.repo, args.run_id);
        let run: serde_json::Value = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::GET,
            &path,
            &[],
            None,
            &[],
        )
        .await?;
        let mut summary = workflow_run_summary(&run);
        if let Some(obj) = summary.as_object_mut() {
            obj.insert(
                "run_attempt".into(),
                run.get("run_attempt")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            );
        }
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&summary).unwrap_or_default(),
        )]))
    }

    /// Re-run all jobs in a workflow run.
    #[tool(description = "Re-run all jobs in a workflow run.")]
    async fn rerun_workflow_run(
        &self,
        Parameters(args): Parameters<WorkflowRunIdArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let path = format!(
            "/repos/{}/{}/actions/runs/{}/rerun",
            r.owner, r.repo, args.run_id
        );
        let _: serde_json::Value = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::POST,
            &path,
            &[],
            None,
            &[],
        )
        .await?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Rerun triggered for run {}",
            args.run_id
        ))]))
    }

    /// Re-run only failed jobs in a workflow run.
    #[tool(description = "Re-run only failed jobs in a workflow run.")]
    async fn rerun_failed_jobs(
        &self,
        Parameters(args): Parameters<WorkflowRunIdArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let path = format!(
            "/repos/{}/{}/actions/runs/{}/rerun-failed-jobs",
            r.owner, r.repo, args.run_id
        );
        let _: serde_json::Value = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::POST,
            &path,
            &[],
            None,
            &[],
        )
        .await?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Rerun of failed jobs triggered for run {}",
            args.run_id
        ))]))
    }

    /// Cancel an in-progress workflow run.
    #[tool(description = "Cancel an in-progress workflow run.")]
    async fn cancel_workflow_run(
        &self,
        Parameters(args): Parameters<WorkflowRunIdArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let path = format!(
            "/repos/{}/{}/actions/runs/{}/cancel",
            r.owner, r.repo, args.run_id
        );
        let _: serde_json::Value = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::POST,
            &path,
            &[],
            None,
            &[],
        )
        .await?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Cancelled run {}",
            args.run_id
        ))]))
    }

    /// List jobs for a workflow run.
    #[tool(description = "List jobs for a workflow run.")]
    async fn list_workflow_run_jobs(
        &self,
        Parameters(args): Parameters<WorkflowRunIdArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let path = format!(
            "/repos/{}/{}/actions/runs/{}/jobs",
            r.owner, r.repo, args.run_id
        );
        let data: serde_json::Value = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::GET,
            &path,
            &[],
            None,
            &[],
        )
        .await?;
        let jobs: Vec<serde_json::Value> = data
            .get("jobs")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|j| {
                        serde_json::json!({
                            "id": j.get("id"),
                            "name": j.get("name"),
                            "status": j.get("status"),
                            "conclusion": j.get("conclusion"),
                            "started_at": j.get("started_at"),
                            "completed_at": j.get("completed_at"),
                            "url": j.get("html_url"),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&jobs).unwrap_or_default(),
        )]))
    }
}
