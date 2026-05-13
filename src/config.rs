//! Environment + endpoint config.
//!
//! `--env staging` / `--env prod` で auth-worker の base URL を切り替え、token cache の
//! 保存先 (`~/.config/github-mcp-server-rs/token-{env}.json`) も env 別にして staging/prod
//! の状態を独立に保つ。

use anyhow::{anyhow, Context, Result};
use clap::ValueEnum;
use directories::ProjectDirs;
use std::path::PathBuf;

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum AuthEnv {
    Staging,
    Prod,
}

impl AuthEnv {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Staging => "staging",
            Self::Prod => "prod",
        }
    }

    /// auth-worker base URL.
    pub fn default_base(&self) -> &'static str {
        match self {
            Self::Staging => "https://auth-staging.ippoan.org",
            Self::Prod => "https://auth.ippoan.org",
        }
    }

    /// MCP relay base URL (issue #27, paired with auth-worker #117).
    /// `mcp.ippoan.org` (prod) / `mcp-staging.ippoan.org` (staging) で
    /// `GET /u/<login>/connect` (WS upgrade) と `POST /u/<login>/mcp` を提供。
    pub fn default_relay_base(&self) -> &'static str {
        match self {
            Self::Staging => "https://mcp-staging.ippoan.org",
            Self::Prod => "https://mcp.ippoan.org",
        }
    }
}

/// 実行時 config — CLI 引数 + 環境変数から組み立てる。
#[derive(Debug, Clone)]
pub struct Config {
    pub env: AuthEnv,
    pub auth_base: String,
    /// MCP relay (auth-worker #117) の base URL。`https://` または `http://` で
    /// 始まる。`relay_ws_connect_url()` が `wss://` / `ws://` に置換する。
    pub relay_base: String,
    /// auth-worker の `/mcp/introspect` を叩く Bearer (auth-worker `INTERNAL_SHARED_SECRET` と同値)。
    pub internal_shared_secret: String,
    /// device flow の client_id (auth-worker は Phase 1 では validate しないので任意文字列で可)。
    pub client_id: String,
    /// MCP token scope (issue #91 仕様: `mcp.read mcp.write`)。
    pub scope: String,
}

impl Config {
    pub fn token_cache_path(&self) -> Result<PathBuf> {
        let dirs = ProjectDirs::from("org", "ippoan", "github-mcp-server-rs")
            .ok_or_else(|| anyhow!("could not determine project config directory"))?;
        let dir = dirs.config_dir();
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create config dir {}", dir.display()))?;
        Ok(dir.join(format!("token-{}.json", self.env.as_str())))
    }

    /// `<auth_base>/<path>` を組み立てる。path は先頭 `/` 必須。
    pub fn url(&self, path: &str) -> String {
        debug_assert!(path.starts_with('/'));
        format!("{}{}", self.auth_base.trim_end_matches('/'), path)
    }

    /// `wss://mcp(-staging).ippoan.org/u/<login>/connect` を組み立てる (WS upgrade endpoint)。
    /// `relay_base` が `http://` 始まりなら `ws://` に、`https://` 始まりなら `wss://` に置換。
    /// それ以外 (e.g. `ws://...` を直接渡された) はそのまま使う。
    pub fn relay_ws_connect_url(&self, login: &str) -> String {
        let trimmed = self.relay_base.trim_end_matches('/');
        let scheme_swapped = if let Some(rest) = trimmed.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = trimmed.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            trimmed.to_string()
        };
        format!("{scheme_swapped}/u/{login}/connect")
    }

    /// `https://mcp(-staging).ippoan.org/u/<login>/mcp` を組み立てる
    /// (Claude Code Web に登録する公開 URL)。`relay_base` が `ws(s)://` で渡されていた場合は
    /// 元の `http(s)://` に戻して返す。
    pub fn relay_public_url(&self, login: &str) -> String {
        let trimmed = self.relay_base.trim_end_matches('/');
        let scheme_swapped = if let Some(rest) = trimmed.strip_prefix("wss://") {
            format!("https://{rest}")
        } else if let Some(rest) = trimmed.strip_prefix("ws://") {
            format!("http://{rest}")
        } else {
            trimmed.to_string()
        };
        format!("{scheme_swapped}/u/{login}/mcp")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(env: AuthEnv, relay_base: &str) -> Config {
        Config {
            env,
            auth_base: env.default_base().to_string(),
            relay_base: relay_base.to_string(),
            internal_shared_secret: "x".into(),
            client_id: "github-mcp-server-rs".into(),
            scope: "mcp.read mcp.write".into(),
        }
    }

    #[test]
    fn relay_ws_url_https_to_wss_staging() {
        let c = cfg_with(AuthEnv::Staging, "https://mcp-staging.ippoan.org");
        assert_eq!(
            c.relay_ws_connect_url("yhonda-ohishi"),
            "wss://mcp-staging.ippoan.org/u/yhonda-ohishi/connect"
        );
    }

    #[test]
    fn relay_ws_url_https_to_wss_prod() {
        let c = cfg_with(AuthEnv::Prod, "https://mcp.ippoan.org");
        assert_eq!(
            c.relay_ws_connect_url("alice"),
            "wss://mcp.ippoan.org/u/alice/connect"
        );
    }

    #[test]
    fn relay_ws_url_http_to_ws_dev_override() {
        let c = cfg_with(AuthEnv::Staging, "http://127.0.0.1:18099");
        assert_eq!(
            c.relay_ws_connect_url("dev"),
            "ws://127.0.0.1:18099/u/dev/connect"
        );
    }

    #[test]
    fn relay_ws_url_passthrough_for_explicit_ws_scheme() {
        let c = cfg_with(AuthEnv::Staging, "ws://localhost:8080");
        assert_eq!(
            c.relay_ws_connect_url("dev"),
            "ws://localhost:8080/u/dev/connect"
        );
    }

    #[test]
    fn relay_ws_url_strips_trailing_slash() {
        let c = cfg_with(AuthEnv::Staging, "https://mcp-staging.ippoan.org/");
        assert_eq!(
            c.relay_ws_connect_url("u"),
            "wss://mcp-staging.ippoan.org/u/u/connect"
        );
    }

    #[test]
    fn relay_public_url_https_passthrough() {
        let c = cfg_with(AuthEnv::Prod, "https://mcp.ippoan.org");
        assert_eq!(
            c.relay_public_url("alice"),
            "https://mcp.ippoan.org/u/alice/mcp"
        );
    }

    #[test]
    fn relay_public_url_unwraps_wss() {
        // user が --relay-base wss://... を渡した時は public URL は https:// で出す
        let c = cfg_with(AuthEnv::Prod, "wss://mcp.ippoan.org");
        assert_eq!(
            c.relay_public_url("alice"),
            "https://mcp.ippoan.org/u/alice/mcp"
        );
    }

    #[test]
    fn default_relay_base_per_env() {
        assert_eq!(
            AuthEnv::Staging.default_relay_base(),
            "https://mcp-staging.ippoan.org"
        );
        assert_eq!(AuthEnv::Prod.default_relay_base(), "https://mcp.ippoan.org");
    }
}
