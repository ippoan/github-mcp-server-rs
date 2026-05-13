//! `/mcp/introspect` への問い合わせ。
//!
//! 認証: `Authorization: <INTERNAL_SHARED_SECRET>` (Bearer prefix なし、生の値)。
//! Request: `{ "token": "<MCP JWT>" }`
//! Response (success): `{ active: true, scope, sub, github_login, github_token, exp }`
//! Response (failure): `{ active: false }`

use anyhow::{bail, Result};
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;

use crate::config::Config;

#[derive(Debug, Deserialize)]
pub struct IntrospectionActive {
    pub scope: String,
    pub sub: String,
    pub github_login: String,
    pub github_token: String,
    pub exp: i64,
}

/// MCP JWT を introspect して、active なら GitHub token + claims を返す。
/// active=false → None。HTTP エラー (401 / 503 / その他) → Err。
pub async fn introspect(
    client: &Client,
    cfg: &Config,
    token: &str,
) -> Result<Option<IntrospectionActive>> {
    let url = cfg.url("/mcp/introspect");
    let resp = client
        .post(&url)
        .header("Authorization", cfg.internal_shared_secret.as_str())
        .header("Content-Type", "application/json")
        .body(json!({ "token": token }).to_string())
        .send()
        .await?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        bail!("introspect: 401 — check INTERNAL_SHARED_SECRET");
    }
    if status.as_u16() == 503 {
        bail!("introspect: 503 — auth-worker missing required env (MCP_OAUTH_KV / MCP_JWT_SECRET / SSO_ENCRYPTION_KEY)");
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("introspect: HTTP {} — {}", status, body);
    }

    let body = resp.text().await?;
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let active = v.get("active").and_then(|x| x.as_bool()).unwrap_or(false);
    if !active {
        return Ok(None);
    }
    let details: IntrospectionActive = serde_json::from_value(v)?;
    Ok(Some(details))
}
