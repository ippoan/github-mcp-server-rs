//! GitHub REST/Search API ヘルパー。`ci-dashboard` (TypeScript) の
//! `src/github-api.ts` を Rust 側に移植したもの。
//!
//! - `parse_repo("owner/name" | "name")` → `name` 単独なら `ippoan` を補完
//! - `validate_org(owner)` → `ippoan` / `ohishi-exp` / `yhonda-ohishi` 以外は 403
//! - `github_api_json` / `github_api_raw` で `reqwest::Client` ラップ
//! - エラーは `GitHubApiError` (status + body) で表現し、`From<GitHubApiError>
//!   for rmcp::ErrorData` で MCP 層に流せる

use reqwest::{Client, Method, StatusCode};
use serde::de::DeserializeOwned;
use thiserror::Error;

const GITHUB_API: &str = "https://api.github.com";
const ALLOWED_ORGS: &[&str] = &["ippoan", "ohishi-exp", "yhonda-ohishi"];
const DEFAULT_ORG: &str = "ippoan";

#[derive(Debug, Error)]
pub enum GitHubApiError {
    #[error("Org not allowed: {0}")]
    OrgNotAllowed(String),
    #[error("GitHub API {status}: {body}")]
    Http { status: u16, body: String },
    #[error("request: {0}")]
    Request(#[from] reqwest::Error),
    #[error("parse: {0}")]
    Parse(#[from] serde_json::Error),
}

impl From<GitHubApiError> for rmcp::ErrorData {
    fn from(err: GitHubApiError) -> Self {
        rmcp::ErrorData::internal_error(err.to_string(), None)
    }
}

#[derive(Debug, Clone)]
pub struct RepoRef {
    pub owner: String,
    pub repo: String,
}

pub fn parse_repo(repo: &str) -> RepoRef {
    if let Some((owner, name)) = repo.split_once('/') {
        RepoRef {
            owner: owner.to_string(),
            repo: name.to_string(),
        }
    } else {
        RepoRef {
            owner: DEFAULT_ORG.to_string(),
            repo: repo.to_string(),
        }
    }
}

pub fn validate_org(owner: &str) -> Result<(), GitHubApiError> {
    if ALLOWED_ORGS.contains(&owner) {
        Ok(())
    } else {
        Err(GitHubApiError::OrgNotAllowed(owner.to_string()))
    }
}

/// `parse_repo` + `validate_org` の合成。tool 実装の冒頭で呼ぶ。
pub fn parse_and_validate_repo(repo: &str) -> Result<RepoRef, GitHubApiError> {
    let r = parse_repo(repo);
    validate_org(&r.owner)?;
    Ok(r)
}

fn apply_common_headers(req: reqwest::RequestBuilder, token: &str) -> reqwest::RequestBuilder {
    req.header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", "github-mcp-server-rs")
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
}

/// REST API を JSON で叩く。`path` は `/repos/...` のような API 相対パス。
///
/// - `params`: querystring (key, value)。空配列なら付与しない。
/// - `body`: POST/PATCH/PUT 時の JSON body。`None` なら付与しない。
/// - `extra_headers`: `Accept` を上書きしたい場合 (例: `search/code` の
///   text-match preview) に使う。
pub async fn github_api_json<T: DeserializeOwned>(
    client: &Client,
    token: &str,
    method: Method,
    path: &str,
    params: &[(&str, String)],
    body: Option<&serde_json::Value>,
    extra_headers: &[(&str, &str)],
) -> Result<T, GitHubApiError> {
    let url = format!("{GITHUB_API}{path}");
    let mut req = apply_common_headers(client.request(method, &url), token);
    if !params.is_empty() {
        req = req.query(params);
    }
    for (k, v) in extra_headers {
        req = req.header(*k, *v);
    }
    if let Some(body) = body {
        req = req.json(body);
    }
    let resp = req.send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(GitHubApiError::Http {
            status: status.as_u16(),
            body: text,
        });
    }
    if status == StatusCode::NO_CONTENT || text.is_empty() {
        // 204 や空 body のときは null を deserialize させる。
        // `T = ()` には serde_json::from_str("null") が通る。
        return Ok(serde_json::from_str("null")?);
    }
    Ok(serde_json::from_str(&text)?)
}

/// GitHub GraphQL API caller. Projects v2 が REST に surface を持たないため必須。
/// レスポンス JSON の `errors[]` を含んだら、メッセージを `;` 区切りに concat して
/// `GitHubApiError::Http { status: 400, ... }` として丸める (ci-dashboard 同等)。
pub async fn github_graphql<T: DeserializeOwned>(
    client: &Client,
    token: &str,
    query: &str,
    variables: serde_json::Value,
) -> Result<T, GitHubApiError> {
    let body = serde_json::json!({
        "query": query,
        "variables": variables,
    });
    let resp = apply_common_headers(client.post(format!("{GITHUB_API}/graphql")), token)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(GitHubApiError::Http {
            status: status.as_u16(),
            body: text,
        });
    }
    let parsed: serde_json::Value = serde_json::from_str(&text)?;
    if let Some(errs) = parsed.get("errors").and_then(|v| v.as_array()) {
        if !errs.is_empty() {
            let msgs: Vec<&str> = errs
                .iter()
                .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                .collect();
            return Err(GitHubApiError::Http {
                status: 400,
                body: format!("GitHub GraphQL error: {}", msgs.join("; ")),
            });
        }
    }
    let Some(data) = parsed.get("data") else {
        return Err(GitHubApiError::Http {
            status: 500,
            body: "GitHub GraphQL: empty data".to_string(),
        });
    };
    Ok(serde_json::from_value(data.clone())?)
}

/// Job log のように plain text を返す endpoint 用 (redirect は reqwest が自動追従)。
pub async fn github_api_raw(
    client: &Client,
    token: &str,
    method: Method,
    path: &str,
) -> Result<String, GitHubApiError> {
    let url = format!("{GITHUB_API}{path}");
    let resp = apply_common_headers(client.request(method, &url), token)
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(GitHubApiError::Http {
            status: status.as_u16(),
            body: text,
        });
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_repo_with_owner() {
        let r = parse_repo("ohishi-exp/foo");
        assert_eq!(r.owner, "ohishi-exp");
        assert_eq!(r.repo, "foo");
    }

    #[test]
    fn parse_repo_defaults_to_ippoan() {
        let r = parse_repo("bar");
        assert_eq!(r.owner, "ippoan");
        assert_eq!(r.repo, "bar");
    }

    #[test]
    fn validate_org_allowed() {
        assert!(validate_org("ippoan").is_ok());
        assert!(validate_org("ohishi-exp").is_ok());
        assert!(validate_org("yhonda-ohishi").is_ok());
    }

    #[test]
    fn validate_org_rejected() {
        let err = validate_org("evil-corp").unwrap_err();
        assert!(matches!(err, GitHubApiError::OrgNotAllowed(_)));
    }

    #[test]
    fn parse_and_validate_repo_rejects_unknown_owner() {
        let err = parse_and_validate_repo("evil/foo").unwrap_err();
        assert!(matches!(err, GitHubApiError::OrgNotAllowed(_)));
    }
}
