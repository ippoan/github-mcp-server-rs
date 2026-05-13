//! RFC 8628 Device Authorization Grant client (auth-worker への問い合わせ)。
//!
//! Flow:
//!   1. `POST /mcp/device_authorization` で device_code / user_code / verification_uri 取得
//!   2. user に `verification_uri_complete` を表示して承認を待つ
//!   3. `POST /mcp/token` を `interval` 秒間隔で polling
//!        - `authorization_pending` → 再試行
//!        - `slow_down` → interval を 5s 増やして再試行 (RFC 8628 §3.5)
//!        - `access_denied` / `expired_token` → エラー終了
//!        - 200 OK → access_token + refresh_token を返却
//!   4. refresh_token grant: `grant_type=refresh_token` で新 token を取得

use anyhow::{anyhow, bail, Result};
use chrono::Utc;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

use crate::config::Config;
use crate::token_cache::TokenSet;

const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

#[derive(Debug, Deserialize)]
pub struct DeviceAuthorizationResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Debug, Deserialize)]
struct TokenSuccess {
    access_token: String,
    refresh_token: String,
    scope: String,
    #[allow(dead_code)]
    expires_in: i64,
}

#[derive(Debug, Deserialize)]
struct OauthError {
    error: String,
    #[serde(default)]
    #[allow(dead_code)]
    error_description: Option<String>,
}

/// Step 1: `POST /mcp/device_authorization`
pub async fn start_device_authorization(
    client: &Client,
    cfg: &Config,
) -> Result<DeviceAuthorizationResponse> {
    let url = cfg.url("/mcp/device_authorization");
    let resp = client
        .post(&url)
        .form(&[
            ("client_id", cfg.client_id.as_str()),
            ("scope", cfg.scope.as_str()),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("device_authorization failed: HTTP {} — {}", status, body);
    }
    Ok(resp.json::<DeviceAuthorizationResponse>().await?)
}

/// Step 3: poll `POST /mcp/token` until approved / denied / expired.
pub async fn poll_token(
    client: &Client,
    cfg: &Config,
    device: &DeviceAuthorizationResponse,
) -> Result<TokenSet> {
    let url = cfg.url("/mcp/token");
    let mut interval = device.interval;
    let deadline = std::time::Instant::now() + Duration::from_secs(device.expires_in);

    loop {
        if std::time::Instant::now() >= deadline {
            bail!("device_code expired before user approval");
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;

        let resp = client
            .post(&url)
            .form(&[
                ("grant_type", DEVICE_CODE_GRANT),
                ("device_code", device.device_code.as_str()),
                ("client_id", cfg.client_id.as_str()),
            ])
            .send()
            .await?;

        let status = resp.status();
        let body = resp.text().await?;

        if status.is_success() {
            let tok: TokenSuccess = serde_json::from_str(&body)
                .map_err(|e| anyhow!("parse token success: {} — body: {}", e, body))?;
            return Ok(TokenSet {
                access_token: tok.access_token.clone(),
                refresh_token: tok.refresh_token,
                scope: tok.scope,
                expires_at: parse_jwt_exp(&tok.access_token)
                    .unwrap_or_else(|| Utc::now().timestamp() + 3600),
                obtained_at: Utc::now(),
            });
        }

        let err: OauthError = serde_json::from_str(&body)
            .map_err(|e| anyhow!("parse oauth error: {} — body: {}", e, body))?;
        match err.error.as_str() {
            "authorization_pending" => continue,
            "slow_down" => {
                interval += 5;
                continue;
            }
            "access_denied" => bail!("user denied the authorization"),
            "expired_token" => bail!("device_code expired"),
            other => bail!("token endpoint returned error: {}", other),
        }
    }
}

/// `grant_type=refresh_token` で新 token を取得する (rotation: 旧 refresh は再使用不可)。
pub async fn refresh(client: &Client, cfg: &Config, refresh_token: &str) -> Result<TokenSet> {
    let url = cfg.url("/mcp/token");
    let resp = client
        .post(&url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        bail!("refresh_token failed: HTTP {} — {}", status, body);
    }
    let tok: TokenSuccess = serde_json::from_str(&body)
        .map_err(|e| anyhow!("parse refresh success: {} — body: {}", e, body))?;
    Ok(TokenSet {
        access_token: tok.access_token.clone(),
        refresh_token: tok.refresh_token,
        scope: tok.scope,
        expires_at: parse_jwt_exp(&tok.access_token)
            .unwrap_or_else(|| Utc::now().timestamp() + 3600),
        obtained_at: Utc::now(),
    })
}

/// JWT の middle segment を base64url decode して `exp` (秒) を抜き取る。
/// 失敗時は None (caller が default 1h を使う)。
fn parse_jwt_exp(jwt: &str) -> Option<i64> {
    let mut parts = jwt.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let pad = (4 - payload.len() % 4) % 4;
    let padded = format!(
        "{}{}",
        payload.replace('-', "+").replace('_', "/"),
        "=".repeat(pad)
    );
    let decoded = base64_decode(&padded)?;
    let v: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    v.get("exp").and_then(|x| x.as_i64())
}

/// minimal base64 (standard alphabet) decoder — std にない代わりに 22 行で済ます。
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lut = [255u8; 256];
    for (i, &c) in TABLE.iter().enumerate() {
        lut[c as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0;
    for &c in s.as_bytes() {
        if c == b'=' {
            break;
        }
        let v = lut[c as usize];
        if v == 255 {
            return None;
        }
        buf = (buf << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_jwt_exp_works_on_real_token_shape() {
        // header.payload.sig where payload = base64url({"exp":1234567890,"sub":"x"})
        let payload = b"{\"exp\":1234567890,\"sub\":\"x\"}";
        let b64 = base64_url_encode(payload);
        let token = format!("hdr.{}.sig", b64);
        assert_eq!(parse_jwt_exp(&token), Some(1234567890));
    }

    #[test]
    fn parse_jwt_exp_returns_none_for_malformed() {
        assert_eq!(parse_jwt_exp("only-one-part"), None);
        assert_eq!(parse_jwt_exp("a.b"), None);
        assert_eq!(parse_jwt_exp("a.b.c"), None); // b is not base64 of JSON
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
