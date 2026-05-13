//! github-mcp-server-rs CLI entry.
//!
//! Subcommands:
//!   - `auth`    — RFC 8628 device flow を実行して token を `~/.config/.../token-{env}.json` に保存
//!   - `whoami`  — cache から token を読み、`/mcp/introspect` で github_token を取得して `/user` を叩く
//!   - `logout`  — token cache を削除
//!
//! 共通 flag:
//!   `--env staging|prod` で auth-worker base URL を切替 (default: staging で先行検証)
//!   `--auth-base <URL>` で base を任意上書き (local dev / wt-quick URL 用)
//!   internal_shared_secret 解決順:
//!     1. `--internal-shared-secret <S>` (CLI)
//!     2. env `GITHUB_MCP_INTERNAL_SHARED_SECRET`
//!     3. build-time embed `MCP_INTERNAL_SECRET` (release binary に焼き込み — build.rs)
//!     4. dev fallback `"dev-secret-do-not-use"` (本物 auth-worker は 401 を返す)

mod auth;
mod config;
mod introspect;
mod mcp_server;
mod relay;
mod token_cache;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use reqwest::Client;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use crate::mcp_server::{GithubContext, GithubMcp};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};

use crate::config::{AuthEnv, Config};
use crate::relay::RelayContext;
use crate::token_cache::TokenSet;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "GitHub MCP server with auth-worker Device Flow client"
)]
struct Cli {
    /// Target environment (URL preset)
    #[arg(long, value_enum, default_value_t = AuthEnv::Staging, global = true)]
    env: AuthEnv,

    /// Override auth-worker base URL (e.g. https://xxx.trycloudflare.com for wt-quick)
    #[arg(long, global = true)]
    auth_base: Option<String>,

    /// Override MCP relay base URL (default: https://mcp(-staging).ippoan.org from env).
    /// 開発時に local mock auth-worker を叩く時用 (例: ws://127.0.0.1:18099)。
    #[arg(long, global = true)]
    relay_base: Option<String>,

    /// auth-worker INTERNAL_SHARED_SECRET。通常は release binary に build-time embed
    /// されているので未指定で OK。上書きしたい時のみ CLI or env で指定。
    /// 解決順: CLI → env → build-time embed → dev fallback (file-level doc 参照)。
    #[arg(long, env = "GITHUB_MCP_INTERNAL_SHARED_SECRET", global = true)]
    internal_shared_secret: Option<String>,

    /// MCP client_id sent to auth-worker (Phase 1 では validate しない)
    #[arg(
        long,
        env = "GITHUB_MCP_CLIENT_ID",
        default_value = "github-mcp-server-rs",
        global = true
    )]
    client_id: String,

    /// MCP scope (issue #91 仕様)
    #[arg(long, default_value = "mcp.read mcp.write", global = true)]
    scope: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run device authorization grant flow, save token to cache
    Auth,
    /// Use cached token (auto-refresh if expired) to fetch github_token via introspect,
    /// then call GitHub /user and print login
    Whoami,
    /// Delete the cached token for the selected env
    Logout,
    /// Show effective config (URLs, cache path) without secrets
    Doctor,
    /// Run MCP server as an outbound WebSocket relay client (issue #27).
    /// `wss://mcp(-staging).ippoan.org/u/<github_login>/connect` に接続し、
    /// auth-worker `McpSession` Durable Object に長寿命 WS を張る。
    /// Claude Code Web からは `https://mcp(-staging).ippoan.org/u/<login>/mcp` に POST
    /// するだけで、auth-worker → DO → WS frame として本 binary に届く。
    /// 旧 `serve` (cloudflared 用 axum bind) は撤廃。
    Relay {
        /// `--user` で github_login を明示。省略時は `/mcp/introspect` で resolve。
        /// install-mcp.sh は明示する (1 回 introspect する手間を省く)。
        #[arg(long)]
        user: Option<String>,
        /// State directory (install-mcp.sh `$STATE_DIR`)。設定すると
        /// `<state-dir>/url` に固定 URL を書き出す。
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Status sentinel (install-mcp.sh が grep する) を stdout に出力する。
        #[arg(long, default_value_t = true)]
        print_status: bool,
    },
}

fn build_config(cli: &Cli) -> Result<Config> {
    let auth_base = cli
        .auth_base
        .clone()
        .unwrap_or_else(|| cli.env.default_base().to_string());
    let relay_base = cli
        .relay_base
        .clone()
        .unwrap_or_else(|| cli.env.default_relay_base().to_string());
    let internal_shared_secret = resolve_internal_secret(cli.internal_shared_secret.as_deref());
    Ok(Config {
        env: cli.env,
        auth_base,
        relay_base,
        internal_shared_secret,
        client_id: cli.client_id.clone(),
        scope: cli.scope.clone(),
    })
}

/// CLI/env → build-time embed → dev fallback の順に解決。空文字列は "未設定" 扱い。
///
/// release binary は CI で `MCP_INTERNAL_SECRET` 環境変数下に build され、
/// `build.rs` 経由で `option_env!()` の対象として焼き付けられる (#25)。
/// 当該 secret は intentionally quasi-public (#20)。本物の認可境界は
/// auth-worker `/mcp/introspect` 内の JWT 署名検証側にある。
fn resolve_internal_secret(cli: Option<&str>) -> String {
    if let Some(s) = cli {
        if !s.is_empty() {
            return s.to_string();
        }
    }
    let embedded = option_env!("MCP_INTERNAL_SECRET").unwrap_or("");
    if !embedded.is_empty() {
        return embedded.to_string();
    }
    "dev-secret-do-not-use".to_string()
}

async fn run_auth(client: &Client, cfg: &Config) -> Result<()> {
    println!("→ Requesting device code from {} ...", cfg.auth_base);
    let device = auth::start_device_authorization(client, cfg).await?;

    println!();
    println!("┌────────────────────────────────────────────────────");
    println!("│ Open this URL in your browser:");
    println!("│   {}", device.verification_uri_complete);
    println!("│");
    println!("│ Or visit {} and enter:", device.verification_uri);
    println!("│   {}", device.user_code);
    println!("│");
    println!(
        "│ Expires in {} seconds. Polling every {} s ...",
        device.expires_in, device.interval
    );
    println!("└────────────────────────────────────────────────────");
    println!();

    let token = auth::poll_token(client, cfg, &device).await?;
    let path = cfg.token_cache_path()?;
    token.save(&path)?;

    println!("✓ Token saved to {}", path.display());
    println!("  scope:      {}", token.scope);
    println!("  expires_at: {} (Unix epoch)", token.expires_at);
    Ok(())
}

async fn run_whoami(client: &Client, cfg: &Config) -> Result<()> {
    let path = cfg.token_cache_path()?;
    let mut token = TokenSet::load(&path)?.ok_or_else(|| {
        anyhow!(
            "no cached token for env={} — run `auth` first",
            cfg.env.as_str()
        )
    })?;

    // 60s skew で余裕を持って refresh
    if token.is_expired(60) {
        println!("→ Access token expired, refreshing ...");
        token = auth::refresh(client, cfg, &token.refresh_token).await?;
        token.save(&path)?;
    }

    println!("→ Calling /mcp/introspect ...");
    let active = introspect::introspect(client, cfg, &token.access_token)
        .await?
        .ok_or_else(|| anyhow!("introspect returned active:false — token may have been revoked"))?;
    println!("✓ Introspect OK:");
    println!("  sub:          {}", active.sub);
    println!("  github_login: {}", active.github_login);
    println!("  scope:        {}", active.scope);
    println!("  exp:          {} (Unix epoch)", active.exp);

    println!("→ Calling GitHub /user with recovered github_token ...");
    let resp = client
        .get("https://api.github.com/user")
        .header("Authorization", format!("Bearer {}", active.github_token))
        .header("User-Agent", "github-mcp-server-rs/0.1.0")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(anyhow!("GitHub /user: HTTP {} — {}", status, body));
    }
    let user: serde_json::Value = serde_json::from_str(&body)?;
    let login = user
        .get("login")
        .and_then(|v| v.as_str())
        .unwrap_or("(unknown)");
    let id = user.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
    println!("✓ GitHub /user OK:");
    println!("  login: {}", login);
    println!("  id:    {}", id);
    Ok(())
}

/// `relay` subcommand: outbound WS で auth-worker `mcp(-staging).ippoan.org` に接続して
/// MCP server を提供する (issue #27)。
async fn run_relay(
    client: &Client,
    cfg: &Config,
    user: Option<String>,
    state_dir: Option<PathBuf>,
    print_status: bool,
) -> Result<()> {
    let path = cfg.token_cache_path()?;
    let mut token = TokenSet::load(&path)?.ok_or_else(|| {
        anyhow!(
            "no cached token for env={} — run `auth` first",
            cfg.env.as_str()
        )
    })?;
    if token.is_expired(60) {
        println!("→ Access token expired, refreshing ...");
        token = auth::refresh(client, cfg, &token.refresh_token).await?;
        token.save(&path)?;
    }

    println!("→ Calling /mcp/introspect to recover github_token ...");
    let active = introspect::introspect(client, cfg, &token.access_token)
        .await?
        .ok_or_else(|| anyhow!("introspect returned active:false — token may have been revoked"))?;
    println!(
        "✓ Introspect OK: github_login={} scope={}",
        active.github_login, active.scope
    );

    // --user 明示が introspect 結果と矛盾していたら fail fast (path mismatch で WS 401 確定)
    let login = match user {
        Some(u) if u != active.github_login => {
            return Err(anyhow!(
                "--user {} does not match introspected github_login={}",
                u,
                active.github_login
            ));
        }
        Some(u) => u,
        None => active.github_login.clone(),
    };

    let ctx = Arc::new(GithubContext {
        github_token: active.github_token,
        github_login: active.github_login,
        scope: active.scope,
        client: client.clone(),
    });

    // rmcp StreamableHttpService — relay では axum router に nest せず、
    // bridge.rs から直接 tower::Service として呼ぶ。
    //
    // allowed_hosts (issue #29): auth-worker が forward する Host header は
    // `mcp(-staging).ippoan.org` (or --relay-base override 時の任意 host) なので、
    // default の loopback only だと 403 "Host header is not allowed" で reject される。
    // relay_base から host を derive して許可リストに追加する。
    let mut allowed_hosts: Vec<String> = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
    if let Some(host) = relay_host_from_base(&cfg.relay_base) {
        allowed_hosts.push(host);
    }

    let factory_ctx = ctx.clone();
    let svc: StreamableHttpService<GithubMcp, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(GithubMcp::new(factory_ctx.clone())),
        Default::default(),
        StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_json_response(true)
            .with_allowed_hosts(allowed_hosts),
    );

    if print_status {
        println!(
            "⇒ MCP relay starting (env={}, user={})",
            cfg.env.as_str(),
            login
        );
    }

    let relay_ctx = RelayContext {
        cfg: Arc::new(cfg.clone()),
        http: client.clone(),
        login,
        jwt: Arc::new(RwLock::new(token)),
        jwt_cache_path: path,
        svc,
        state_dir,
        print_status,
    };

    relay::run_relay(relay_ctx).await
}

/// `https://mcp-staging.ippoan.org` / `wss://mcp.ippoan.org` / `http://127.0.0.1:18099` 等から
/// `host[:port]` を抽出する (rmcp `with_allowed_hosts` に渡す用)。scheme prefix が
/// 認識できなければ None。trailing `/path` も削除する。
fn relay_host_from_base(base: &str) -> Option<String> {
    let trimmed = base.trim();
    let after_scheme = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .or_else(|| trimmed.strip_prefix("wss://"))
        .or_else(|| trimmed.strip_prefix("ws://"))?;
    let host = after_scheme.split('/').next().unwrap_or("");
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

fn run_logout(cfg: &Config) -> Result<()> {
    let path = cfg.token_cache_path()?;
    TokenSet::delete(&path)?;
    println!("✓ Token cache deleted: {}", path.display());
    Ok(())
}

fn run_doctor(cfg: &Config) -> Result<()> {
    let cache = cfg.token_cache_path()?;
    let cached = TokenSet::load(&cache)?;
    println!("env:              {}", cfg.env.as_str());
    println!("auth_base:        {}", cfg.auth_base);
    println!("relay_base:       {}", cfg.relay_base);
    println!("client_id:        {}", cfg.client_id);
    println!("scope:            {}", cfg.scope);
    println!(
        "internal_secret:  {}",
        if cfg.internal_shared_secret.is_empty() {
            "(not set)".to_string()
        } else {
            format!("(set, {} chars)", cfg.internal_shared_secret.len())
        }
    );
    println!("token_cache:      {}", cache.display());
    match cached {
        Some(t) => {
            println!("  scope:        {}", t.scope);
            println!("  expires_at:   {}", t.expires_at);
            println!("  expired:      {}", t.is_expired(0));
            println!("  obtained_at:  {}", t.obtained_at);
        }
        None => println!("  (no token cached)"),
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let cfg = build_config(&cli)?;
    let client = Client::builder()
        .user_agent(concat!("github-mcp-server-rs/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .context("build reqwest client")?;

    match cli.command {
        Command::Auth => run_auth(&client, &cfg).await,
        Command::Whoami => run_whoami(&client, &cfg).await,
        Command::Logout => run_logout(&cfg),
        Command::Doctor => run_doctor(&cfg),
        Command::Relay {
            user,
            state_dir,
            print_status,
        } => run_relay(&client, &cfg, user, state_dir, print_status).await,
    }
}

#[cfg(test)]
mod tests {
    use super::relay_host_from_base;

    #[test]
    fn relay_host_https_prod() {
        assert_eq!(
            relay_host_from_base("https://mcp.ippoan.org"),
            Some("mcp.ippoan.org".into())
        );
    }

    #[test]
    fn relay_host_https_staging_with_trailing_slash() {
        assert_eq!(
            relay_host_from_base("https://mcp-staging.ippoan.org/"),
            Some("mcp-staging.ippoan.org".into())
        );
    }

    #[test]
    fn relay_host_wss_passthrough() {
        assert_eq!(
            relay_host_from_base("wss://mcp.ippoan.org/u/x/connect"),
            Some("mcp.ippoan.org".into())
        );
    }

    #[test]
    fn relay_host_http_with_port() {
        assert_eq!(
            relay_host_from_base("http://127.0.0.1:18099"),
            Some("127.0.0.1:18099".into())
        );
    }

    #[test]
    fn relay_host_ws_with_port_and_path() {
        assert_eq!(
            relay_host_from_base("ws://localhost:8080/u/dev/connect"),
            Some("localhost:8080".into())
        );
    }

    #[test]
    fn relay_host_unknown_scheme_returns_none() {
        assert_eq!(relay_host_from_base("ftp://nope"), None);
        assert_eq!(relay_host_from_base("mcp.ippoan.org"), None);
        assert_eq!(relay_host_from_base(""), None);
    }

    #[test]
    fn relay_host_empty_after_scheme_returns_none() {
        assert_eq!(relay_host_from_base("https://"), None);
        assert_eq!(relay_host_from_base("https:///path"), None);
    }
}
