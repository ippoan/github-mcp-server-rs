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

mod admin_exec;
mod github_api;
mod introspect;
mod mcp_server;
mod tools;

// Phase 2: auth / config / pair / relay / token_cache は mcp-relay crate に移動
// (ippoan/ref-files-mcp-server-rs#4)。`crate::<name>` 参照を残すための re-export。
pub use mcp_relay::{auth, config, pair, relay, token_cache};

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
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
use crate::relay::{PairRelayContext, RelayContext};
use crate::token_cache::TokenSet;

/// `--version` 出力に焼き込む文字列。
///
/// `CARGO_PKG_VERSION` (Cargo.toml の "0.1.0") は release ごとに bump されないので、
/// install-mcp.sh から「いま走っている binary が target release tag のものか」を
/// 識別できない (#39 の TAG_FILE が嘘の時に検出できない)。release.yml が tag push で
/// 走るとき `GITHUB_REF_NAME=v0.0.NN` が set されるので、build.rs がそれを
/// `BUILD_RELEASE_TAG` env で焼き込み、ここで `--version` 出力に append する。
/// dev build では空文字なので format は `0.1.0` のまま。
const VERSION: &str = if env!("BUILD_RELEASE_TAG").is_empty() {
    env!("CARGO_PKG_VERSION")
} else {
    concat!(
        env!("CARGO_PKG_VERSION"),
        " (",
        env!("BUILD_RELEASE_TAG"),
        ")"
    )
};

#[derive(Parser, Debug)]
#[command(
    version = VERSION,
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
    /// Run the 1-click pair flow (issue #42, paired with auth-worker #144).
    ///
    /// Self-contained: `POST /mcp/pair/new` → pair_url を **stdout** に 1 行印字
    /// → `Authorization: Bearer <pair_code>` で WS upgrade を polling
    /// (401 + `Pair-Status: pending` → 2s sleep retry, 最大 pair_code TTL = 5min)
    /// → 101 で frame bridge loop へ。WS が close したら `Ok(())` で抜ける
    /// (pair_code は 1 回限り消費されるので reconnect しない)。
    ///
    /// Claude Code on the Web (CCoW) container では install-mcp.sh が本 subcommand を
    /// nohup で background 起動し、stderr に出る pair_url を user に見せる。
    /// device-flow (`auth` subcommand) は CLI / local dev / offline 用途に温存し、
    /// `$GITHUB_MCP_AUTO_DEVICE_FLOW=1` で従来挙動に opt-in できる。
    Pair {
        /// github_login を明示。省略時は `$GITHUB_LOGIN` env を読む。両方未設定なら error。
        #[arg(long, env = "GITHUB_LOGIN")]
        user: Option<String>,
        /// State directory (install-mcp.sh `$STATE_DIR`)。handshake 成功後に
        /// `<state-dir>/url` に固定 public URL を書き出す。
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Status sentinel (install-mcp.sh が grep する) を stderr に出力する。
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
        project_name: "github-mcp-server-rs",
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

    // Share the same Arc<RwLock<TokenSet>> between the relay loop (which
    // refreshes on WS 401) and admin_exec_with_refresh (which refreshes on
    // expiry / proxy 401). Either path's refresh is visible to the other.
    let token_lock = Arc::new(RwLock::new(token));
    let cfg_arc = Arc::new(cfg.clone());
    let ctx = Arc::new(GithubContext {
        github_token: active.github_token,
        github_login: active.github_login,
        scope: active.scope,
        token: token_lock.clone(),
        token_cache_path: path.clone(),
        cfg: cfg_arc.clone(),
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
        cfg: cfg_arc,
        http: client.clone(),
        login,
        jwt: token_lock,
        jwt_cache_path: path,
        svc,
        state_dir,
        print_status,
        service: "github-mcp-server-rs",
        binary_version: env!("CARGO_PKG_VERSION"),
    };

    relay::run_relay(relay_ctx).await
}

/// `pair` subcommand: 1-click pair flow を全部 in-process で実行する (issue #42)。
///
/// install-mcp.sh から `nohup` で background 起動される前提。前に流れている
/// stderr は install-mcp.sh が grep して user に見せる:
///   1. `POST /mcp/pair/new` で pair_code / pair_url を取得
///   2. pair_url を **stdout** に 1 行だけ印字 (install-mcp.sh が `grep -oE`)
///   3. WS upgrade を polling (`Pair-Status: pending` → 2s sleep)
///   4. 101 で frame bridge loop に合流 → WS close で exit
async fn run_pair(
    client: &Client,
    cfg: &Config,
    user: Option<String>,
    state_dir: Option<PathBuf>,
    print_status: bool,
) -> Result<()> {
    // ── 1. resolve login ────────────────────────────────────────────────
    let login = match user.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => {
            return Err(anyhow!(
                "pair: github login not provided.\n\
                 \n\
                 hint: pass `--user <github_login>` or set `$GITHUB_LOGIN` in the\n\
                       environment (Claude Code on the Web: Settings → Environment\n\
                       variables → add `GITHUB_LOGIN=<your-github-username>`).\n\
                 \n\
                 The username is needed so the auth-worker can match your browser\n\
                 cookie session against the pair_code your binary just minted."
            ));
        }
    };

    // ── 2. POST /mcp/pair/new ───────────────────────────────────────────
    let binary_version = VERSION; // `0.1.0` or `0.1.0 (v0.0.x)`
    if print_status {
        eprintln!(
            "→ pair: POST {} (claim_login={login}, binary_version=\"{binary_version}\")",
            cfg.pair_new_url()
        );
    }
    let resp = pair::pair_new(client, cfg, &login, binary_version).await?;

    // ── 3. surface pair_url on stdout — install-mcp.sh grep target ──────
    // **stdout** (not stderr): install-mcp.sh redirects stdout+stderr into the
    // same log file via `>$STATE_DIR/pair.log 2>&1` and greps the URL out, but
    // emitting via println! keeps the URL also reachable if the hook ever
    // separates the two streams (e.g. piping stdout into a notifier).
    println!("{}", resp.pair_url);
    if print_status {
        eprintln!(
            "⇒ pair_url surfaced (expires in {}s, pair_code len={})",
            resp.expires_in,
            resp.pair_code.len()
        );
        eprintln!("   {}", resp.pair_url);
    }

    // ── 4. build degraded GithubContext + StreamableHttpService ─────────
    // Pair flow は WS 接続専用 (`mcp-pair-callback.ts` の docstring に明記)。
    // `whoami` 以上の tool は github_token / jwt を必要とし pair mode では失敗するが、
    // `tools/list` は context state に依存せず 40 tools を返す。
    // 完全な tool 動作には device-flow (`auth` subcommand) か pre-staged
    // `$GITHUB_MCP_TOKEN_JSON` が引き続き必要 — 本 issue の out of scope。
    // pair mode は JWT を持たないので空 TokenSet を入れる。admin tool は
    // refresh_token が空で auth::refresh が即 fail → admin_exec_with_refresh が
    // 「local refresh failed」付きの actionable error を返す挙動になる。
    let empty_token = Arc::new(RwLock::new(TokenSet {
        access_token: String::new(),
        refresh_token: String::new(),
        scope: cfg.scope.clone(),
        expires_at: 0,
        obtained_at: Utc::now(),
    }));
    let ctx = Arc::new(GithubContext {
        github_token: String::new(),
        github_login: login.clone(),
        scope: cfg.scope.clone(),
        token: empty_token,
        token_cache_path: cfg
            .token_cache_path()
            .unwrap_or_else(|_| PathBuf::from("/tmp/pair-no-cache")),
        cfg: Arc::new(cfg.clone()),
        client: client.clone(),
    });
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

    // ── 5. WS upgrade + frame loop ──────────────────────────────────────
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(resp.expires_in.clamp(60, 600));
    let pair_ctx = PairRelayContext {
        cfg: Arc::new(cfg.clone()),
        login,
        svc,
        state_dir,
        print_status,
        service: "github-mcp-server-rs",
        binary_version: env!("CARGO_PKG_VERSION"),
    };
    relay::run_pair_session(pair_ctx, resp.pair_code, deadline).await
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
    // `--scope mcp.admin` is now a no-op as far as tool surface is concerned:
    // admin tools are always exposed and authorized server-side via auth-worker
    // `/mcp/elevate`. The scope value still flows into the device flow request
    // for backward compat.
    if cli.scope.split_whitespace().any(|s| s == "mcp.admin") {
        eprintln!(
            "Note: --scope=mcp.admin is now ignored; admin tools are always available \
             and authorized server-side via auth-worker /mcp/elevate"
        );
    }
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
        Command::Pair {
            user,
            state_dir,
            print_status,
        } => run_pair(&client, &cfg, user, state_dir, print_status).await,
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
