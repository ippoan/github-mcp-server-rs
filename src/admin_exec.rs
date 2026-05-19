//! Proxy helper for the auth-worker `/mcp/admin/exec` endpoint (Phase 2).
//!
//! Admin tools (branch protection) no longer call the GitHub REST API
//! directly from the binary. Instead they POST to auth-worker, which uses a
//! GitHub App installation token server-side and gates the call behind a
//! short-lived (15min) elevate flag minted via a browser-based one-tap flow
//! (`/mcp/elevate`). This keeps the high-privilege App token out of the
//! distributed binary entirely.
//!
//! Contract:
//!   POST {auth_worker_origin}/mcp/admin/exec
//!     Authorization: Bearer <MCP JWT>
//!     Content-Type:  application/json
//!     Body: { "tool": "<tool_name>", "args": { ... } }
//!
//!   200 → { "ok": true,  "result": <github_response_or_null> }
//!   401 → { "ok": false, "error": "invalid_jwt" | "missing_authorization" }
//!   403 → { "ok": false, "error": "not_elevated", "elevate_url": "..." }
//!   400 → { "ok": false, "error": "...", "details": "..." }
//!   502 → { "ok": false, "error": "github_api_error", "status": <int>, "body": "..." }
//!
//! ## JWT refresh wrapper (`admin_exec_with_refresh`)
//!
//! The raw `admin_exec` takes an immutable JWT string and surfaces 401 as
//! `"Authentication failed ... reconnect the binary"`. In long-running
//! relay sessions the binary's JWT (TTL 1h) expires while the binary
//! itself is healthy. To avoid forcing users to reconnect the binary,
//! `admin_exec_with_refresh` pre-checks `exp - now < 60s` and refreshes
//! via `auth::refresh()` (refresh_token grant, TTL 30d) before each admin
//! tool call. On a 401 from the proxy it attempts a forced refresh and
//! retries once. If the local refresh_token is also dead, it tries to
//! pick up a freshly minted pair from auth-worker `/mcp/jwt/pickup`
//! (populated when the user just went through `/mcp/elevate`). Only when
//! both local refresh and pickup fail does it surface the device URL.

use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;

use crate::auth;
use crate::mcp_server::GithubContext;
use crate::token_cache::TokenSet;

const MAX_BODY_LEN: usize = 500;

/// Truncate `s` to at most `MAX_BODY_LEN` chars (byte-safe at char boundaries).
fn truncate(s: &str) -> String {
    if s.len() <= MAX_BODY_LEN {
        return s.to_string();
    }
    // floor to nearest char boundary <= MAX_BODY_LEN
    let mut end = MAX_BODY_LEN;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... (truncated)", &s[..end])
}

/// POST `{tool, args}` to auth-worker `/mcp/admin/exec` and unwrap `result`.
///
/// Error messages are intentionally user-facing — they surface to the MCP
/// client (Claude Code, etc.) verbatim via `rmcp::ErrorData::internal_error`.
pub async fn admin_exec(
    client: &Client,
    auth_worker_origin: &str,
    jwt: &str,
    tool: &str,
    args: Value,
) -> Result<Value> {
    let url = format!(
        "{}/mcp/admin/exec",
        auth_worker_origin.trim_end_matches('/')
    );
    let body = serde_json::json!({ "tool": tool, "args": args });
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {jwt}"))
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| anyhow!("auth-worker /mcp/admin/exec: request failed: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let parsed: Option<Value> = serde_json::from_str(&text).ok();

    if status.is_success() {
        let v =
            parsed.ok_or_else(|| anyhow!("auth-worker /mcp/admin/exec: invalid JSON response"))?;
        if v.get("ok").and_then(|x| x.as_bool()) == Some(true) {
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
        return Err(anyhow!(
            "auth-worker /mcp/admin/exec: unexpected 2xx without ok:true — body={}",
            truncate(&text)
        ));
    }

    // Error paths — extract `error` / `details` / `elevate_url` if present.
    let err_code = parsed
        .as_ref()
        .and_then(|v| v.get("error"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let details = parsed
        .as_ref()
        .and_then(|v| v.get("details"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    match status.as_u16() {
        401 => Err(anyhow!(
            "Authentication failed. The MCP JWT is invalid or expired; reconnect the binary."
        )),
        403 => {
            let elevate_url = parsed
                .as_ref()
                .and_then(|v| v.get("elevate_url"))
                .and_then(|v| v.as_str())
                .unwrap_or("https://auth.ippoan.org/mcp/elevate");
            if err_code == "not_elevated" {
                Err(anyhow!(
                    "Admin elevation required. Visit {elevate_url} in your browser to grant 15-minute admin access."
                ))
            } else {
                Err(anyhow!(
                    "auth-worker /mcp/admin/exec: 403 ({err_code}) — {details}"
                ))
            }
        }
        400 => {
            let msg = if !details.is_empty() {
                details.to_string()
            } else if !err_code.is_empty() {
                err_code.to_string()
            } else {
                truncate(&text)
            };
            Err(anyhow!("auth-worker /mcp/admin/exec: bad request — {msg}"))
        }
        502 => {
            let gh_status = parsed
                .as_ref()
                .and_then(|v| v.get("status"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let gh_body = parsed
                .as_ref()
                .and_then(|v| v.get("body"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            Err(anyhow!(
                "auth-worker /mcp/admin/exec: GitHub API error (status={gh_status}) — {}",
                truncate(gh_body)
            ))
        }
        _ => Err(anyhow!(
            "auth-worker /mcp/admin/exec: unexpected HTTP {status} — {}",
            truncate(&text)
        )),
    }
}

/// Convert anyhow error → rmcp::ErrorData (admin tools use anyhow internally).
pub fn to_rmcp_error(e: anyhow::Error) -> rmcp::ErrorData {
    rmcp::ErrorData::internal_error(e.to_string(), None)
}

/// `admin_exec` wrapper that auto-refreshes the MCP JWT around the call.
///
/// Flow:
///  1. If `exp - now < 60s` → refresh via local refresh_token grant
///     (`auth::refresh`). Persist to `token_cache_path`, update `ctx.token`.
///  2. Call `admin_exec` with the current access_token.
///  3. On 401 (`"Authentication failed"` from `admin_exec`): try one forced
///     refresh + retry. Surface a clearer error if even that fails.
///  4. If refresh fails (e.g. refresh_token rotated by a parallel session
///     or expired): try `try_jwt_pickup()` — auth-worker may have stashed a
///     fresh pair via `/mcp/elevate` completion. If pickup succeeds we
///     retry the admin call once more.
///  5. Fall through error message includes a clickable elevate URL so
///     the user can re-authorize in one step.
///
/// Non-auth errors (403 not_elevated, 502 github_api_error, etc.) flow
/// straight through — refresh only fires on auth-shaped failures.
pub async fn admin_exec_with_refresh(
    ctx: &GithubContext,
    tool: &str,
    args: Value,
) -> Result<Value> {
    // ── 1. Pre-check expiry, refresh if needed ──────────────────────────
    let needs_refresh = ctx.token.read().await.is_expired(60);
    if needs_refresh && !ctx.token.read().await.refresh_token.is_empty() {
        // best-effort; if it fails, admin_exec below will return 401 and we
        // re-attempt refresh + pickup in the 401 branch
        let _ = refresh_in_place(ctx).await;
    }

    let jwt = ctx.token.read().await.access_token.clone();
    let auth_origin = ctx.cfg.auth_base.clone();

    // ── 2. First attempt ────────────────────────────────────────────────
    let first = admin_exec(&ctx.client, &auth_origin, &jwt, tool, args.clone()).await;
    let first_err = match first {
        Ok(v) => return Ok(v),
        Err(e) => e,
    };

    if !is_auth_failure(&first_err) {
        return Err(first_err);
    }

    // ── 3. Forced refresh + retry once ──────────────────────────────────
    let refresh_result = refresh_in_place(ctx).await;
    if refresh_result.is_ok() {
        let jwt = ctx.token.read().await.access_token.clone();
        match admin_exec(&ctx.client, &auth_origin, &jwt, tool, args.clone()).await {
            Ok(v) => return Ok(v),
            Err(e) if !is_auth_failure(&e) => return Err(e),
            Err(_) => { /* still 401 — fall through to pickup */ }
        }
    }

    // ── 4. Pickup fallback (auth-worker `/mcp/elevate` stash) ───────────
    let pickup = try_jwt_pickup(ctx).await;
    if let Ok(Some(new_set)) = pickup {
        // overwrite cache + shared lock
        if let Err(e) = new_set.save(&ctx.token_cache_path) {
            tracing::warn!("admin_exec_with_refresh: failed to persist picked-up token: {e}");
        }
        let jwt = new_set.access_token.clone();
        *ctx.token.write().await = new_set;
        match admin_exec(&ctx.client, &auth_origin, &jwt, tool, args).await {
            Ok(v) => return Ok(v),
            Err(e) => return Err(e), // surface verbatim — pickup was fresh
        }
    }

    // ── 5. Give up — surface actionable error ───────────────────────────
    let elevate_url = format!("{}/mcp/elevate", auth_origin.trim_end_matches('/'));
    let refresh_hint = match refresh_result {
        Ok(()) => String::new(),
        Err(e) => format!(" (local refresh also failed: {e})"),
    };
    Err(anyhow!(
        "Admin tool {tool} failed: MCP JWT is invalid or expired and could not be refreshed{refresh_hint}. \
         Open {elevate_url} in your browser to re-authorize, then retry the tool."
    ))
}

/// `auth::refresh()` を呼んで `ctx.token` (RwLock) と cache file を更新する。
///
/// `refresh_in_place` 自体は `Result<()>` を返すので、caller は失敗時に pickup
/// fallback / device URL に分岐できる。
async fn refresh_in_place(ctx: &GithubContext) -> Result<()> {
    let refresh_token = { ctx.token.read().await.refresh_token.clone() };
    if refresh_token.is_empty() {
        return Err(anyhow!("no refresh_token available (pair-only session?)"));
    }
    let new_token = auth::refresh(&ctx.client, &ctx.cfg, &refresh_token).await?;
    new_token.save(&ctx.token_cache_path)?;
    *ctx.token.write().await = new_token;
    Ok(())
}

/// anyhow error が `admin_exec` 由来の 401 ("Authentication failed ... reconnect"
/// 文言) か判定。下層の `admin_exec` は anyhow::Error として返すので message で
/// matching する (downcast 不要 / variant に依存しない)。
fn is_auth_failure(e: &anyhow::Error) -> bool {
    let s = e.to_string();
    s.contains("Authentication failed")
}

/// Auth-worker `/mcp/jwt/pickup` from the elevate-side refresh stash.
///
/// Endpoint contract (auth-worker `handleMcpJwtPickup`):
///   POST {auth_base}/mcp/jwt/pickup
///     Authorization: Bearer <MCP JWT, possibly expired>
///     200 → { access_token, refresh_token, scope, expires_in }
///     404 → no pickup available for this user
///     401 → JWT signature invalid / missing — fail hard
///
/// auth-worker verifies the JWT **signature only** (not `exp`), so an
/// expired-but-genuine binary can recover. The KV entry is one-shot (deleted
/// after read) and bound to the `sub` claim of the presented JWT.
async fn try_jwt_pickup(ctx: &GithubContext) -> Result<Option<TokenSet>> {
    let url = format!("{}/mcp/jwt/pickup", ctx.cfg.auth_base.trim_end_matches('/'));
    let jwt = ctx.token.read().await.access_token.clone();
    if jwt.is_empty() {
        return Ok(None);
    }
    let resp = ctx
        .client
        .post(&url)
        .header("Authorization", format!("Bearer {jwt}"))
        .send()
        .await
        .map_err(|e| anyhow!("/mcp/jwt/pickup: request failed: {e}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "/mcp/jwt/pickup: HTTP {status} — {}",
            truncate(&body)
        ));
    }
    let body: PickupResponse = resp
        .json()
        .await
        .map_err(|e| anyhow!("/mcp/jwt/pickup: parse JSON: {e}"))?;
    let expires_at = chrono::Utc::now().timestamp() + body.expires_in.max(0);
    Ok(Some(TokenSet {
        access_token: body.access_token,
        refresh_token: body.refresh_token,
        scope: body.scope,
        expires_at,
        obtained_at: chrono::Utc::now(),
    }))
}

#[derive(Debug, Deserialize)]
struct PickupResponse {
    access_token: String,
    refresh_token: String,
    scope: String,
    expires_in: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_result_on_200_ok_true() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/mcp/admin/exec")
            .match_header("authorization", "Bearer test-jwt")
            .match_header("content-type", "application/json")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"ok":true,"result":{"url":"https://api.github.com/...","enabled":true}}"#,
            )
            .create_async()
            .await;

        let client = Client::new();
        let result = admin_exec(
            &client,
            &server.url(),
            "test-jwt",
            "get_branch_protection",
            serde_json::json!({"owner":"ippoan","repo":"x","branch":"main"}),
        )
        .await
        .expect("should succeed");

        mock.assert_async().await;
        assert_eq!(result.get("enabled").and_then(|v| v.as_bool()), Some(true));
    }

    #[tokio::test]
    async fn returns_null_when_result_omitted() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/mcp/admin/exec")
            .with_status(200)
            .with_body(r#"{"ok":true}"#)
            .create_async()
            .await;

        let client = Client::new();
        let result = admin_exec(
            &client,
            &server.url(),
            "j",
            "delete_branch_protection",
            serde_json::json!({}),
        )
        .await
        .unwrap();
        assert!(result.is_null());
    }

    #[tokio::test]
    async fn surfaces_elevate_url_on_403_not_elevated() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/mcp/admin/exec")
            .with_status(403)
            .with_body(
                r#"{"ok":false,"error":"not_elevated","elevate_url":"https://auth.ippoan.org/mcp/elevate?return=x"}"#,
            )
            .create_async()
            .await;

        let client = Client::new();
        let err = admin_exec(
            &client,
            &server.url(),
            "j",
            "set_branch_protection",
            serde_json::json!({}),
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Admin elevation required"), "msg={msg}");
        assert!(
            msg.contains("https://auth.ippoan.org/mcp/elevate?return=x"),
            "msg={msg}"
        );
        assert!(msg.contains("15-minute"), "msg={msg}");
    }

    #[tokio::test]
    async fn surfaces_invalid_jwt_on_401() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/mcp/admin/exec")
            .with_status(401)
            .with_body(r#"{"ok":false,"error":"invalid_jwt"}"#)
            .create_async()
            .await;

        let client = Client::new();
        let err = admin_exec(
            &client,
            &server.url(),
            "bad",
            "set_branch_protection",
            serde_json::json!({}),
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Authentication failed"), "msg={msg}");
        assert!(msg.contains("reconnect"), "msg={msg}");
    }

    #[tokio::test]
    async fn surfaces_details_on_400() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/mcp/admin/exec")
            .with_status(400)
            .with_body(
                r#"{"ok":false,"error":"forbidden_owner","details":"owner 'evil-corp' not in allowlist"}"#,
            )
            .create_async()
            .await;

        let client = Client::new();
        let err = admin_exec(
            &client,
            &server.url(),
            "j",
            "set_branch_protection",
            serde_json::json!({"owner":"evil-corp"}),
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("evil-corp"), "msg={msg}");
        assert!(msg.contains("not in allowlist"), "msg={msg}");
    }

    #[tokio::test]
    async fn surfaces_github_status_and_body_on_502() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/mcp/admin/exec")
            .with_status(502)
            .with_body(
                r#"{"ok":false,"error":"github_api_error","status":404,"body":"{\"message\":\"Branch not protected\"}"}"#,
            )
            .create_async()
            .await;

        let client = Client::new();
        let err = admin_exec(
            &client,
            &server.url(),
            "j",
            "get_branch_protection",
            serde_json::json!({}),
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("status=404"), "msg={msg}");
        assert!(msg.contains("Branch not protected"), "msg={msg}");
    }

    #[tokio::test]
    async fn propagates_network_failure() {
        // Point at an unreachable port; reqwest should fail to connect.
        let client = Client::builder()
            .timeout(std::time::Duration::from_millis(500))
            .build()
            .unwrap();
        let err = admin_exec(
            &client,
            "http://127.0.0.1:1", // port 1 is reserved & not in use
            "j",
            "get_branch_protection",
            serde_json::json!({}),
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("request failed"), "msg={msg}");
    }

    #[test]
    fn truncate_respects_utf8_boundary() {
        let s = "a".repeat(600);
        let t = truncate(&s);
        assert!(t.len() <= MAX_BODY_LEN + 20);
        assert!(t.ends_with("(truncated)"));

        // multibyte string
        let m = "あ".repeat(300); // 3 bytes each → 900 bytes
        let t = truncate(&m);
        assert!(t.is_char_boundary(0));
        assert!(t.ends_with("(truncated)"));
    }

    // ─── admin_exec_with_refresh + helpers ──────────────────────────────

    use crate::config::{AuthEnv, Config};
    use chrono::Utc;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::sync::RwLock;

    /// Build a `GithubContext` pointing at `auth_base` (a mockito server URL).
    /// `expires_offset_sec` < 0 → token is already expired.
    fn test_ctx(
        auth_base: &str,
        access: &str,
        refresh: &str,
        expires_offset_sec: i64,
    ) -> (GithubContext, PathBuf, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("token.json");
        let cfg = Arc::new(Config {
            env: AuthEnv::Staging,
            auth_base: auth_base.to_string(),
            relay_base: "https://mcp.test.invalid".into(),
            internal_shared_secret: "x".into(),
            client_id: "github-mcp-server-rs".into(),
            scope: "mcp.read mcp.write".into(),
            project_name: "github-mcp-server-rs",
        });
        let token = TokenSet {
            access_token: access.into(),
            refresh_token: refresh.into(),
            scope: "mcp.read mcp.write".into(),
            expires_at: Utc::now().timestamp() + expires_offset_sec,
            obtained_at: Utc::now(),
        };
        let ctx = GithubContext {
            github_token: "gh-token".into(),
            github_login: "alice".into(),
            scope: "mcp.read mcp.write".into(),
            token: Arc::new(RwLock::new(token)),
            token_cache_path: path.clone(),
            cfg,
            client: Client::new(),
        };
        (ctx, path, dir)
    }

    #[test]
    fn is_auth_failure_matches_admin_exec_401_string() {
        let e = anyhow!("Authentication failed. The MCP JWT is invalid or expired");
        assert!(is_auth_failure(&e));
        let e2 = anyhow!("auth-worker /mcp/admin/exec: GitHub API error (status=404)");
        assert!(!is_auth_failure(&e2));
    }

    #[tokio::test]
    async fn with_refresh_happy_path_no_refresh_when_fresh() {
        let mut server = mockito::Server::new_async().await;
        let exec_mock = server
            .mock("POST", "/mcp/admin/exec")
            .match_header("authorization", "Bearer fresh-jwt")
            .with_status(200)
            .with_body(r#"{"ok":true,"result":{"enabled":true}}"#)
            .expect(1)
            .create_async()
            .await;
        // No /mcp/token call expected
        let refresh_guard = server
            .mock("POST", "/mcp/token")
            .expect(0)
            .with_status(200)
            .with_body("should not be called")
            .create_async()
            .await;

        let (ctx, _path, _dir) = test_ctx(&server.url(), "fresh-jwt", "fresh-refresh", 3600);
        let v = admin_exec_with_refresh(
            &ctx,
            "get_branch_protection",
            serde_json::json!({"owner":"ippoan","repo":"x","branch":"main"}),
        )
        .await
        .expect("should succeed");
        assert_eq!(v.get("enabled").and_then(|x| x.as_bool()), Some(true));
        exec_mock.assert_async().await;
        refresh_guard.assert_async().await;
    }

    #[tokio::test]
    async fn with_refresh_pre_refreshes_when_expired() {
        let mut server = mockito::Server::new_async().await;
        // Refresh endpoint mints `new-jwt`. exp encoded so parse_jwt_exp returns far-future.
        // jwt with header.payload.sig; payload base64url({"exp":<far>,"sub":"x"})
        let far = Utc::now().timestamp() + 3600;
        let payload_json = format!(r#"{{"exp":{far},"sub":"github:alice"}}"#);
        let payload_b64 = base64_url_encode(payload_json.as_bytes());
        let new_jwt = format!("hdr.{payload_b64}.sig");
        let body = serde_json::json!({
            "access_token": new_jwt,
            "refresh_token": "next-refresh",
            "scope": "mcp.read mcp.write",
            "expires_in": 3600,
        })
        .to_string();
        let refresh_mock = server
            .mock("POST", "/mcp/token")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex("grant_type=refresh_token".to_string()),
                mockito::Matcher::Regex("refresh_token=stale-refresh".to_string()),
            ]))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body)
            .expect(1)
            .create_async()
            .await;
        let exec_mock = server
            .mock("POST", "/mcp/admin/exec")
            .match_header("authorization", format!("Bearer {new_jwt}").as_str())
            .with_status(200)
            .with_body(r#"{"ok":true,"result":{"refreshed":true}}"#)
            .expect(1)
            .create_async()
            .await;

        // Token already expired (-300 sec)
        let (ctx, path, _dir) = test_ctx(&server.url(), "stale-jwt", "stale-refresh", -300);
        let v = admin_exec_with_refresh(&ctx, "get_branch_protection", serde_json::json!({}))
            .await
            .expect("should succeed");
        assert_eq!(v.get("refreshed").and_then(|x| x.as_bool()), Some(true));
        refresh_mock.assert_async().await;
        exec_mock.assert_async().await;
        // cache file should now contain new-jwt
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains(&new_jwt));
    }

    #[tokio::test]
    async fn with_refresh_retries_on_401_then_succeeds() {
        let mut server = mockito::Server::new_async().await;
        // First admin_exec call returns 401, then 200 after refresh
        let exec_401 = server
            .mock("POST", "/mcp/admin/exec")
            .match_header("authorization", "Bearer old-jwt")
            .with_status(401)
            .with_body(r#"{"ok":false,"error":"invalid_jwt"}"#)
            .expect(1)
            .create_async()
            .await;
        let far = Utc::now().timestamp() + 3600;
        let payload = format!(r#"{{"exp":{far},"sub":"github:alice"}}"#);
        let new_jwt = format!("hdr.{}.sig", base64_url_encode(payload.as_bytes()));
        let body = serde_json::json!({
            "access_token": new_jwt,
            "refresh_token": "next-refresh",
            "scope": "mcp.read mcp.write",
            "expires_in": 3600,
        })
        .to_string();
        let refresh_mock = server
            .mock("POST", "/mcp/token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body)
            .expect(1)
            .create_async()
            .await;
        let exec_200 = server
            .mock("POST", "/mcp/admin/exec")
            .match_header("authorization", format!("Bearer {new_jwt}").as_str())
            .with_status(200)
            .with_body(r#"{"ok":true,"result":{"retried":true}}"#)
            .expect(1)
            .create_async()
            .await;

        // Token still has 3600s left so pre-check refresh does NOT fire
        let (ctx, _, _dir) = test_ctx(&server.url(), "old-jwt", "good-refresh", 3600);
        let v = admin_exec_with_refresh(&ctx, "set_branch_protection", serde_json::json!({}))
            .await
            .expect("should succeed");
        assert_eq!(v.get("retried").and_then(|x| x.as_bool()), Some(true));
        exec_401.assert_async().await;
        refresh_mock.assert_async().await;
        exec_200.assert_async().await;
    }

    #[tokio::test]
    async fn with_refresh_falls_back_to_pickup_when_local_refresh_fails() {
        let mut server = mockito::Server::new_async().await;
        let exec_401 = server
            .mock("POST", "/mcp/admin/exec")
            .match_header("authorization", "Bearer old-jwt")
            .with_status(401)
            .with_body(r#"{"ok":false,"error":"invalid_jwt"}"#)
            .expect(1)
            .create_async()
            .await;
        // refresh_token grant fails with 400 invalid_grant (refresh expired)
        let refresh_fail = server
            .mock("POST", "/mcp/token")
            .with_status(400)
            .with_body(r#"{"error":"invalid_grant"}"#)
            .expect(1)
            .create_async()
            .await;
        // pickup returns a fresh pair
        let far = Utc::now().timestamp() + 3600;
        let payload = format!(r#"{{"exp":{far},"sub":"github:alice"}}"#);
        let pickup_jwt = format!("hdr.{}.sig", base64_url_encode(payload.as_bytes()));
        let pickup_body = serde_json::json!({
            "access_token": pickup_jwt,
            "refresh_token": "elevated-refresh",
            "scope": "mcp.read mcp.write",
            "expires_in": 3600,
        })
        .to_string();
        let pickup_mock = server
            .mock("POST", "/mcp/jwt/pickup")
            .match_header("authorization", "Bearer old-jwt")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(pickup_body)
            .expect(1)
            .create_async()
            .await;
        let exec_200 = server
            .mock("POST", "/mcp/admin/exec")
            .match_header("authorization", format!("Bearer {pickup_jwt}").as_str())
            .with_status(200)
            .with_body(r#"{"ok":true,"result":{"via_pickup":true}}"#)
            .expect(1)
            .create_async()
            .await;

        let (ctx, _, _dir) = test_ctx(&server.url(), "old-jwt", "dead-refresh", 3600);
        let v = admin_exec_with_refresh(&ctx, "set_branch_protection", serde_json::json!({}))
            .await
            .expect("should succeed via pickup");
        assert_eq!(v.get("via_pickup").and_then(|x| x.as_bool()), Some(true));
        exec_401.assert_async().await;
        refresh_fail.assert_async().await;
        pickup_mock.assert_async().await;
        exec_200.assert_async().await;
    }

    #[tokio::test]
    async fn with_refresh_surfaces_elevate_url_when_everything_fails() {
        let mut server = mockito::Server::new_async().await;
        let _exec_401 = server
            .mock("POST", "/mcp/admin/exec")
            .with_status(401)
            .with_body(r#"{"ok":false,"error":"invalid_jwt"}"#)
            .expect_at_least(1)
            .create_async()
            .await;
        let _refresh_fail = server
            .mock("POST", "/mcp/token")
            .with_status(400)
            .with_body(r#"{"error":"invalid_grant"}"#)
            .create_async()
            .await;
        // no /mcp/jwt/pickup mock → mockito returns 501 default; we want 404 to
        // get the clean "no pickup" path. Add explicit 404.
        let _pickup_404 = server
            .mock("POST", "/mcp/jwt/pickup")
            .with_status(404)
            .with_body(r#"{"error":"no_pickup"}"#)
            .create_async()
            .await;

        let (ctx, _, _dir) = test_ctx(&server.url(), "old-jwt", "dead-refresh", 3600);
        let err = admin_exec_with_refresh(&ctx, "set_branch_protection", serde_json::json!({}))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Admin tool set_branch_protection failed"),
            "msg={msg}"
        );
        assert!(msg.contains("/mcp/elevate"), "msg={msg}");
        assert!(msg.contains("local refresh also failed"), "msg={msg}");
    }

    #[tokio::test]
    async fn with_refresh_passes_through_non_auth_errors() {
        // 502 github_api_error should not trigger refresh or retry.
        let mut server = mockito::Server::new_async().await;
        let exec_502 = server
            .mock("POST", "/mcp/admin/exec")
            .with_status(502)
            .with_body(r#"{"ok":false,"error":"github_api_error","status":404,"body":"{\"message\":\"Not Found\"}"}"#)
            .expect(1)
            .create_async()
            .await;
        let refresh_guard = server
            .mock("POST", "/mcp/token")
            .expect(0)
            .with_status(200)
            .with_body("should not be called")
            .create_async()
            .await;

        let (ctx, _, _dir) = test_ctx(&server.url(), "old-jwt", "refresh", 3600);
        let err = admin_exec_with_refresh(&ctx, "set_branch_protection", serde_json::json!({}))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("status=404"), "msg={msg}");
        exec_502.assert_async().await;
        refresh_guard.assert_async().await;
    }

    fn base64_url_encode(bytes: &[u8]) -> String {
        const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = Vec::new();
        for chunk in bytes.chunks(3) {
            let mut buf = 0u32;
            for (i, &b) in chunk.iter().enumerate() {
                buf |= u32::from(b) << (16 - 8 * i);
            }
            let n_out = chunk.len() * 4 / 3 + if chunk.len() % 3 == 0 { 0 } else { 1 };
            for i in 0..n_out {
                out.push(TABLE[((buf >> (18 - 6 * i)) & 0x3F) as usize]);
            }
        }
        String::from_utf8(out).unwrap()
    }
}
