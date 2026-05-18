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

use anyhow::{anyhow, Result};
use reqwest::Client;
use serde_json::Value;

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
}
