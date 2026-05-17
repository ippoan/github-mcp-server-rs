//! Job logs (tail / range / grep) — ci-dashboard `src/mcp/tools/logs.ts` 移植。

use reqwest::Method;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::github_api::{github_api_raw, parse_and_validate_repo};
use crate::mcp_server::GithubMcp;

const MAX_GREP_MATCHES: usize = 50;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetJobLogsArgs {
    /// Repository (e.g. 'rust-alc-api').
    pub repo: String,
    /// Job ID (from list_workflow_run_jobs).
    pub job_id: u64,
    /// Lines from end (default 200, max 1000). Ignored if start_line set.
    #[serde(default)]
    pub tail_lines: Option<u32>,
    /// Start line (1-based) for range retrieval.
    #[serde(default)]
    pub start_line: Option<u32>,
    /// End line (inclusive) for range retrieval.
    #[serde(default)]
    pub end_line: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GrepJobLogsArgs {
    /// Repository (e.g. 'rust-alc-api').
    pub repo: String,
    /// Job ID (from list_workflow_run_jobs).
    pub job_id: u64,
    /// Regex pattern (e.g. 'error|fail|panic'). Case-insensitive.
    pub pattern: String,
    /// Context lines before/after each match (default 3, max 20).
    #[serde(default)]
    pub context_lines: Option<u32>,
}

#[tool_router(router = logs_router, vis = "pub(crate)")]
impl GithubMcp {
    /// Get logs for a workflow job. Returns tail lines by default, or a specific line range.
    #[tool(
        description = "Get logs for a workflow job. Returns tail lines by default, or a specific line range with start_line/end_line."
    )]
    async fn get_job_logs(
        &self,
        Parameters(args): Parameters<GetJobLogsArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let tail_lines = args.tail_lines.unwrap_or(200).clamp(1, 1000) as usize;
        let raw = github_api_raw(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::GET,
            &format!(
                "/repos/{}/{}/actions/jobs/{}/logs",
                r.owner, r.repo, args.job_id
            ),
        )
        .await?;
        let lines: Vec<&str> = raw.split('\n').collect();
        let total = lines.len();

        let (selected, header, start_num) = if let Some(start) = args.start_line {
            let start = start.max(1) as usize;
            let end = args
                .end_line
                .map(|e| (e as usize).min(total))
                .unwrap_or(total);
            let slice = if start <= total {
                &lines[start - 1..end]
            } else {
                &[][..]
            };
            (
                slice.to_vec(),
                format!("Lines {}-{} of {}", start, end.min(total), total),
                start,
            )
        } else if total > tail_lines {
            let slice = &lines[total - tail_lines..];
            (
                slice.to_vec(),
                format!(
                    "Last {tail_lines} of {total} lines (use start_line/end_line for specific range)"
                ),
                total - tail_lines + 1,
            )
        } else {
            (lines.clone(), format!("{total} lines (complete)"), 1)
        };

        let numbered: Vec<String> = selected
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{}: {}", start_num + i, line))
            .collect();
        let text = format!("{header}\n\n{}", numbered.join("\n"));
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Search job logs with regex pattern. Returns matching lines with context.
    #[tool(
        description = "Search job logs with regex pattern. Returns matching lines with context. Use for finding errors: pattern='error|fail|panic'"
    )]
    async fn grep_job_logs(
        &self,
        Parameters(args): Parameters<GrepJobLogsArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let context_lines = args.context_lines.unwrap_or(3).clamp(0, 20) as usize;
        let raw = github_api_raw(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::GET,
            &format!(
                "/repos/{}/{}/actions/jobs/{}/logs",
                r.owner, r.repo, args.job_id
            ),
        )
        .await?;
        let lines: Vec<&str> = raw.split('\n').collect();
        // ci-dashboard 側は `new RegExp(pattern, "i")`。Rust の regex crate は
        // `(?i)` で case-insensitive を表現する。
        let pattern_ci = format!("(?i){}", args.pattern);
        let re = regex::Regex::new(&pattern_ci)
            .map_err(|e| rmcp::ErrorData::invalid_params(format!("invalid regex: {e}"), None))?;

        let match_indices: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter_map(|(i, l)| re.is_match(l).then_some(i))
            .collect();

        if match_indices.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No matches for /{}/i in {} lines",
                args.pattern,
                lines.len()
            ))]));
        }

        let truncated = match_indices.len() > MAX_GREP_MATCHES;
        let display: Vec<usize> = match_indices
            .iter()
            .take(MAX_GREP_MATCHES)
            .copied()
            .collect();

        // Merge overlapping context ranges.
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for &idx in &display {
            let start = idx.saturating_sub(context_lines);
            let end = (idx + context_lines).min(lines.len() - 1);
            if let Some(last) = ranges.last_mut() {
                if start <= last.1 + 1 {
                    last.1 = end;
                    continue;
                }
            }
            ranges.push((start, end));
        }

        let match_set: std::collections::HashSet<usize> = display.iter().copied().collect();
        let parts: Vec<String> = ranges
            .iter()
            .map(|(s, e)| {
                let chunk: Vec<String> = (*s..=*e)
                    .map(|i| {
                        let marker = if match_set.contains(&i) { ">" } else { " " };
                        format!("{} {}: {}", marker, i + 1, lines[i])
                    })
                    .collect();
                chunk.join("\n")
            })
            .collect();

        let mut header = format!(
            "{} matches for /{}/i in {} lines",
            match_indices.len(),
            args.pattern,
            lines.len()
        );
        if truncated {
            header.push_str(&format!(
                " (showing first {}, {} more truncated)",
                MAX_GREP_MATCHES,
                match_indices.len() - MAX_GREP_MATCHES
            ));
        }
        Ok(CallToolResult::success(vec![Content::text(format!(
            "{header}\n\n{}",
            parts.join("\n---\n")
        ))]))
    }
}
