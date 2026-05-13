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
mod token_cache;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use reqwest::Client;
use std::sync::Arc;
use std::time::Duration;

use crate::mcp_server::{GithubContext, GithubMcp};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};

use crate::config::{AuthEnv, Config};
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
    /// Run MCP server (Streamable HTTP transport) on the given bind address.
    /// Requires a cached token (run `auth` first). Loads github_token via
    /// introspect at startup and caches in-memory for the process lifetime.
    Serve {
        /// Bind address (host:port)
        #[arg(long, default_value = "127.0.0.1:8765")]
        bind: String,
        /// Allowed Host header values (comma-separated). Default: loopback only.
        /// Set explicitly for public exposure (cloudflared tunnel etc.).
        #[arg(long, value_delimiter = ',')]
        allowed_hosts: Option<Vec<String>>,
        /// Use stateless JSON response mode (no SSE framing). Recommended for
        /// simple request/response tools.
        #[arg(long, default_value_t = true)]
        json_response: bool,
    },
}

fn build_config(cli: &Cli) -> Result<Config> {
    let auth_base = cli
        .auth_base
        .clone()
        .unwrap_or_else(|| cli.env.default_base().to_string());
    let internal_shared_secret = resolve_internal_secret(cli.internal_shared_secret.as_deref());
    Ok(Config {
        env: cli.env,
        auth_base,
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

async fn run_serve(
    client: &Client,
    cfg: &Config,
    bind: &str,
    allowed_hosts: Option<Vec<String>>,
    json_response: bool,
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

    let ctx = Arc::new(GithubContext {
        github_token: active.github_token,
        github_login: active.github_login,
        scope: active.scope,
        client: client.clone(),
    });

    // rmcp StreamableHttpService の factory は session ごとに呼ばれる
    let factory_ctx = ctx.clone();
    let service: StreamableHttpService<GithubMcp, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(GithubMcp::new(factory_ctx.clone())),
        Default::default(),
        StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_json_response(json_response)
            .with_allowed_hosts(allowed_hosts.unwrap_or_else(|| {
                vec![
                    "localhost".into(),
                    "127.0.0.1".into(),
                    "::1".into(),
                    // bind した host:port もデフォで許可しておく
                    bind.to_string(),
                ]
            })),
    );

    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind {bind}"))?;
    let addr = listener.local_addr()?;
    println!(
        "⇒ MCP server listening on http://{addr}/mcp (env={})",
        cfg.env.as_str()
    );
    println!("   Register URL in Claude Code Web → MCP connector.");
    println!(
        "   ※ Ctrl-C で停止。token 期限は {} (Unix epoch)。",
        token.expires_at
    );

    axum::serve(listener, router).await.context("axum serve")?;
    Ok(())
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
        Command::Serve {
            bind,
            allowed_hosts,
            json_response,
        } => run_serve(&client, &cfg, &bind, allowed_hosts, json_response).await,
    }
}
