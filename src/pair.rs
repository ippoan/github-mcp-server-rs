//! 1-click pair flow client (issue #42, paired with auth-worker #144).
//!
//! Device-flow (`src/auth.rs`) は CLI / local dev / offline 用途に温存しつつ、
//! Claude Code on the Web (CCoW) 等の "fresh container" コンテキストでは
//! `pair` subcommand から本 module を経由して
//!
//!   1. `POST <RELAY_BASE>/mcp/pair/new`      → `pair_code` / `pair_url`
//!   2. browser 1-click で `pair_url` を踏むと auth-worker `mcp_pair_session`
//!      cookie + GitHub OAuth で本人確認 → KV record が `status=approved`
//!      + `binding_jwt` を持つ
//!   3. binary が `Authorization: Bearer <pair_code>` で WS upgrade
//!      (`relay::run_pair_session` 側)
//!
//! を 1 連の流れで実行する。本 module は (1) の HTTP 部分のみを担当する。

use anyhow::{anyhow, bail, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::config::Config;

/// `POST /mcp/pair/new` の request body schema (auth-worker `handleMcpPairNew`)。
#[derive(Debug, Serialize)]
struct PairNewRequest<'a> {
    claim_login: &'a str,
    binary_version: &'a str,
    /// `requested_scope` は MVP では送らない (auth-worker 側 default = `mcp.read mcp.write`)。
    /// admin scope を要求したい時は別 subcommand で対応する (本 issue の out of scope)。
    #[serde(skip_serializing_if = "Option::is_none")]
    requested_scope: Option<&'a str>,
}

/// `POST /mcp/pair/new` の response schema (auth-worker `handleMcpPairNew`)。
///
/// 200 OK 時に返る。429 / 503 / 400 等は HTTP error として bail。
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct PairNewResponse {
    /// browser に表示する 1-click URL (例: `https://auth-staging.ippoan.org/mcp/pair/<code>`)。
    /// auth-worker `env.AUTH_WORKER_ORIGIN` から生成される。
    pub pair_url: String,
    /// WS upgrade で `Authorization: Bearer <pair_code>` として送る短期 token。
    /// 5 min TTL、approve 後 1 回限りで KV から削除される。
    pub pair_code: String,
    /// TTL hint (秒、auth-worker は 300 を返す)。binary 側 polling の上限に使う。
    pub expires_in: u64,
}

/// `POST <RELAY_BASE>/mcp/pair/new` を叩く。
///
/// 失敗時の典型:
///   - 400 `claim_login is required`        : `--user` / `$GITHUB_LOGIN` 未設定
///   - 429 `rate_limited`                   : 同一 source IP から 10/min 超過
///   - 503 `MCP OAuth Provider not configured` / `AUTH_WORKER_ORIGIN not configured`
pub async fn pair_new(
    client: &Client,
    cfg: &Config,
    claim_login: &str,
    binary_version: &str,
) -> Result<PairNewResponse> {
    if claim_login.is_empty() {
        bail!("pair_new: claim_login is empty (pass --user or set $GITHUB_LOGIN)");
    }
    let url = cfg.pair_new_url();
    let body = PairNewRequest {
        claim_login,
        binary_version,
        requested_scope: None,
    };
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let raw = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("pair_new: HTTP {} — {}", status, raw);
    }
    let parsed: PairNewResponse = serde_json::from_str(&raw)
        .map_err(|e| anyhow!("pair_new: parse response: {e} — body: {raw}"))?;
    if parsed.pair_code.is_empty() || parsed.pair_url.is_empty() {
        bail!("pair_new: response missing pair_code / pair_url — body: {raw}");
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_relay_base(base: &str) -> Config {
        Config {
            env: crate::config::AuthEnv::Staging,
            auth_base: "https://auth-staging.ippoan.org".into(),
            relay_base: base.into(),
            internal_shared_secret: "x".into(),
            client_id: "github-mcp-server-rs".into(),
            scope: "mcp.read mcp.write".into(),
        }
    }

    #[tokio::test]
    async fn pair_new_ok_parses_response() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/mcp/pair/new")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"pair_code":"abc123abc123abc123abc123abc123abc123","pair_url":"https://auth-x.example/mcp/pair/abc","expires_in":300}"#,
            )
            .match_header("content-type", "application/json")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "claim_login": "alice",
                "binary_version": "0.1.0-test"
            })))
            .create_async()
            .await;
        let cfg = cfg_with_relay_base(&server.url());
        let client = Client::new();
        let resp = pair_new(&client, &cfg, "alice", "0.1.0-test").await.unwrap();
        assert_eq!(resp.pair_code.len(), 36);
        assert!(resp.pair_url.starts_with("https://auth-x.example/mcp/pair/"));
        assert_eq!(resp.expires_in, 300);
        m.assert_async().await;
    }

    #[tokio::test]
    async fn pair_new_http_error_bails() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/mcp/pair/new")
            .with_status(429)
            .with_body(r#"{"error":"rate_limited","error_description":"too many"}"#)
            .create_async()
            .await;
        let cfg = cfg_with_relay_base(&server.url());
        let client = Client::new();
        let err = pair_new(&client, &cfg, "alice", "0.1.0-test")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("429"), "got: {err}");
        assert!(err.contains("rate_limited"), "got: {err}");
    }

    #[tokio::test]
    async fn pair_new_malformed_body_bails() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/mcp/pair/new")
            .with_status(200)
            .with_body(r#"{"not_what_we_expect": true}"#)
            .create_async()
            .await;
        let cfg = cfg_with_relay_base(&server.url());
        let client = Client::new();
        let err = pair_new(&client, &cfg, "alice", "0.1.0-test")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("parse response"), "got: {err}");
    }

    #[tokio::test]
    async fn pair_new_empty_claim_login_short_circuits() {
        let cfg = cfg_with_relay_base("https://mcp-staging.ippoan.org");
        let client = Client::new();
        let err = pair_new(&client, &cfg, "", "0.1.0-test")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("claim_login is empty"), "got: {err}");
    }
}
