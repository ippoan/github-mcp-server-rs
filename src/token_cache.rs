//! MCP access_token + refresh_token のローカル永続化。
//!
//! `~/.config/github-mcp-server-rs/token-{env}.json` に JSON で保存。
//! access_token の `exp` を見て期限切れ判定、refresh_token grant で自動更新する。

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: String,
    pub scope: String,
    /// MCP JWT の `exp` claim (Unix epoch seconds)。
    pub expires_at: i64,
    /// 取得時刻 (debug 用)。
    pub obtained_at: DateTime<Utc>,
}

impl TokenSet {
    pub fn is_expired(&self, skew_seconds: i64) -> bool {
        let now = Utc::now().timestamp();
        self.expires_at <= now + skew_seconds
    }

    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read token cache {}", path.display()))?;
        let set: Self = serde_json::from_str(&raw)
            .with_context(|| format!("parse token cache {}", path.display()))?;
        Ok(Some(set))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let raw = serde_json::to_string_pretty(self)?;
        std::fs::write(path, raw)
            .with_context(|| format!("write token cache {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // 0600 — owner read/write only (token は secret)
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(path, perms).ok();
        }
        Ok(())
    }

    pub fn delete(path: &Path) -> Result<()> {
        if path.exists() {
            std::fs::remove_file(path)
                .with_context(|| format!("delete token cache {}", path.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample() -> TokenSet {
        TokenSet {
            access_token: "jwt".into(),
            refresh_token: "refresh".into(),
            scope: "mcp.read mcp.write".into(),
            expires_at: Utc::now().timestamp() + 3600,
            obtained_at: Utc::now(),
        }
    }

    #[test]
    fn roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.json");
        let set = sample();
        set.save(&path).unwrap();
        let loaded = TokenSet::load(&path).unwrap().unwrap();
        assert_eq!(loaded.access_token, "jwt");
        assert_eq!(loaded.refresh_token, "refresh");
    }

    #[test]
    fn load_missing_returns_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.json");
        assert!(TokenSet::load(&path).unwrap().is_none());
    }

    #[test]
    fn is_expired_with_skew() {
        let now = Utc::now().timestamp();
        let set = TokenSet {
            access_token: "".into(),
            refresh_token: "".into(),
            scope: "".into(),
            expires_at: now + 30,
            obtained_at: Utc::now(),
        };
        // skew=60 → exp(now+30) <= now+60 → true
        assert!(set.is_expired(60));
        // skew=10 → exp(now+30) <= now+10 → false
        assert!(!set.is_expired(10));
    }
}
