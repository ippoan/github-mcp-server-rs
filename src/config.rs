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
}

/// 実行時 config — CLI 引数 + 環境変数から組み立てる。
#[derive(Debug, Clone)]
pub struct Config {
    pub env: AuthEnv,
    pub auth_base: String,
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
}
