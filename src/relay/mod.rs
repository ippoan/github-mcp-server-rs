//! Outbound WebSocket relay client (issue #27, paired with auth-worker #117).
//!
//! `wss://mcp(-staging).ippoan.org/u/<login>/connect` に `Authorization: Bearer <mcp-jwt>` で
//! 接続し、auth-worker `McpSession` Durable Object と長寿命 WS を張る。
//! Claude Code Web からの `POST /u/<login>/mcp` は auth-worker → DO → WS frame として
//! 本 binary に届き、`StreamableHttpService` (rmcp) に dispatch される。
//!
//! 設計判断:
//! - **stateless 1-req-1-resp**: WS frame は `Req`/`Resp`/`Hello` の 3 種のみ。SSE / cancellation
//!   は frame v2 で扱う (本 plan §設計判断)。
//! - **同一 user 1 接続**: auth-worker 側が新 WS upgrade で旧 WS を `close(1000, "replaced")`
//!   する。
//! - **控えめな reconnect (issue #30)**: CF が WS を idle close する度に aggressive に
//!   reconnect すると auth-worker `handleBridge` の stale-WS race を誘発する。binary は
//!   - clean close を 1 回受けたら 5s cooldown で 1 回だけ reconnect、続けて close されたら
//!     `Ok(())` で exit (install-mcp.sh が次セッション spawn 時に respawn する)。
//!   - network error は 3 回連続で `Err` exit (1s → 2s backoff)。
//! - **JWT refresh**: handshake が 401 を返したら `auth::refresh()` で更新して即時再接続。
//!   `refresh` 自体が失敗したら fail fast (caller = install-mcp.sh が `auth` 再実行を案内)。
//! - **複数 in-flight**: 受信 frame ごとに `tokio::spawn`、応答は mpsc 経由で writer task に集約。
//!   ping は同じ writer task から pong で返す。

pub mod bridge;
pub mod frame;

use anyhow::{anyhow, Context, Result};
use axum::body::Body;
use bytes::Bytes;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use http::{Request as HttpRequest, Response as HttpResponse};
use reqwest::Client as HttpClient;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, RwLock};
use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tower::Service;

use crate::auth;
use crate::config::Config;
use crate::token_cache::TokenSet;

use self::frame::{Frame, FRAME_VERSION};

/// 1 接続のセッションに必要な共有 state。
///
/// `S` は rmcp `StreamableHttpService` 等の `tower::Service` 型 (response body type は generic)。
pub struct RelayContext<S> {
    pub cfg: Arc<Config>,
    pub http: HttpClient,
    pub login: String,
    /// MCP JWT (access + refresh) を共有 state として保持。refresh は逐次化。
    pub jwt: Arc<RwLock<TokenSet>>,
    /// token cache file path (refresh 後に save する用)。
    pub jwt_cache_path: PathBuf,
    /// rmcp `StreamableHttpService` (clone-cheap、internal は Arc)。
    pub svc: S,
    /// optional state dir (e.g. install-mcp.sh `$STATE_DIR`)。設定時、最初の接続成功で
    /// `<state-dir>/url` に public URL を書く。
    pub state_dir: Option<PathBuf>,
    /// optional sentinel logging — install-mcp.sh が grep する用。
    pub print_status: bool,
}

#[derive(Debug)]
enum RelayError {
    /// WS handshake が 401 等で reject (JWT 失効 / signature mismatch)。`auth::refresh()` を試す。
    AuthRejected,
    /// network / handshake 以外の WS エラー。backoff のみ。
    Network(anyhow::Error),
    /// frame schema バージョン乖離 / プロトコル違反。fail fast (再接続しない)。
    /// auth-worker Phase 7 が Hello の `proto` mismatch で `close(1002)` を返したらここに来る想定。
    #[allow(dead_code)]
    Protocol(anyhow::Error),
    /// fatal: caller 側で再起動が必要。
    Fatal(anyhow::Error),
}

impl std::fmt::Display for RelayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RelayError::AuthRejected => write!(f, "auth rejected (401)"),
            RelayError::Network(e) => write!(f, "network: {e}"),
            RelayError::Protocol(e) => write!(f, "protocol: {e}"),
            RelayError::Fatal(e) => write!(f, "fatal: {e}"),
        }
    }
}

/// 1 回 clean close を受けてから cooldown するまでの間隔。
const RECONNECT_COOLDOWN: Duration = Duration::from_secs(5);
/// network error 連続発生で諦めるまでの試行回数 (本数自体は cap、間隔は 1s, 2s)。
const MAX_NETWORK_RETRIES: u32 = 3;

/// `run_relay` の状態遷移を input event だけで決める純関数 (テスト容易性のため抽出)。
#[derive(Debug, PartialEq, Eq)]
enum LoopAction {
    /// `tokio::time::sleep(d)` してから次の周回へ。
    Reconnect(Duration),
    /// 即時再接続 (sleep skip)。
    ReconnectImmediate,
    /// `Ok(())` で関数を抜ける (install-mcp.sh が次セッションで respawn する)。
    ExitOk,
    /// `Err` で関数を抜ける。
    ExitErr,
}

/// loop 内の連続イベントカウンタ。
#[derive(Default, Debug)]
struct LoopState {
    clean_close_streak: u32,
    network_error_streak: u32,
}

impl LoopState {
    fn on_clean_close(&mut self) -> LoopAction {
        self.clean_close_streak += 1;
        self.network_error_streak = 0;
        if self.clean_close_streak >= 2 {
            LoopAction::ExitOk
        } else {
            LoopAction::Reconnect(RECONNECT_COOLDOWN)
        }
    }

    fn on_auth_rejected(&mut self) -> LoopAction {
        self.clean_close_streak = 0;
        self.network_error_streak = 0;
        LoopAction::ReconnectImmediate
    }

    fn on_network_error(&mut self) -> LoopAction {
        self.network_error_streak += 1;
        self.clean_close_streak = 0;
        if self.network_error_streak >= MAX_NETWORK_RETRIES {
            LoopAction::ExitErr
        } else {
            // 1st = 1s, 2nd = 2s
            let backoff = Duration::from_secs(1u64 << (self.network_error_streak - 1));
            LoopAction::Reconnect(backoff)
        }
    }
}

/// 本 binary の relay loop entry。
///
/// 終了条件 (issue #30):
/// - clean close を 2 回連続で受けたら `Ok(())` (install-mcp.sh が respawn する)
/// - network error が 3 回連続したら `Err`
/// - protocol / fatal は即 `Err`
pub async fn run_relay<S, RB>(ctx: RelayContext<S>) -> Result<()>
where
    S: Service<HttpRequest<Body>, Response = HttpResponse<RB>, Error = Infallible>
        + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send,
    RB: http_body::Body<Data = Bytes> + Send + 'static,
    RB::Error: std::error::Error + Send + Sync + 'static,
{
    // 1 周目だけ public URL を state file に書く (install-mcp.sh が読む)
    if let Some(dir) = &ctx.state_dir {
        let public = ctx.cfg.relay_public_url(&ctx.login);
        let path = dir.join("url");
        if let Err(e) = std::fs::write(&path, &public) {
            tracing::warn!("failed to write relay url to {}: {e}", path.display());
        }
        if ctx.print_status {
            println!("⇒ MCP relay: public URL = {public}");
        }
    }

    let mut state = LoopState::default();
    let mut first_success_announced = false;

    loop {
        let connect_url = ctx.cfg.relay_ws_connect_url(&ctx.login);
        if ctx.print_status {
            println!("→ MCP relay: connecting to {connect_url} as {}", ctx.login);
        }

        let result = connect_and_serve(&ctx).await;

        // 最初の Ok (= 1 セッション完走) で sentinel を 1 度だけ出す
        if result.is_ok() && !first_success_announced && ctx.print_status {
            println!("✓ MCP relay: connected (first session)");
            first_success_announced = true;
        }

        let action = match result {
            Ok(()) => {
                let action = state.on_clean_close();
                if ctx.print_status {
                    match action {
                        LoopAction::ExitOk => println!(
                            "✓ MCP relay: clean close x{} — exiting (install-mcp.sh respawns next session)",
                            state.clean_close_streak
                        ),
                        LoopAction::Reconnect(d) => println!(
                            "✓ MCP relay: connection closed cleanly, reconnecting in {d:?}"
                        ),
                        _ => {}
                    }
                }
                action
            }
            Err(RelayError::AuthRejected) => {
                if ctx.print_status {
                    eprintln!("⚠ MCP relay: WS handshake rejected (401), refreshing JWT");
                }
                refresh_jwt(&ctx).await.context("JWT refresh")?;
                state.on_auth_rejected()
            }
            Err(RelayError::Network(e)) => {
                let action = state.on_network_error();
                match action {
                    LoopAction::ExitErr => {
                        return Err(anyhow!(
                            "relay: {MAX_NETWORK_RETRIES} consecutive network errors, last: {e}"
                        ));
                    }
                    LoopAction::Reconnect(d) => {
                        tracing::warn!(
                            "relay network error: {e}, retry in {:?} ({}/{})",
                            d,
                            state.network_error_streak,
                            MAX_NETWORK_RETRIES
                        );
                        if ctx.print_status {
                            eprintln!(
                                "⚠ MCP relay: network error ({e}), retry in {:?} ({}/{})",
                                d, state.network_error_streak, MAX_NETWORK_RETRIES
                            );
                        }
                    }
                    _ => {}
                }
                action
            }
            Err(RelayError::Protocol(e)) => {
                // proto mismatch は再接続しても直らない (binary を update する必要がある)。
                return Err(anyhow!("relay protocol error: {e}"));
            }
            Err(RelayError::Fatal(e)) => {
                return Err(anyhow!("relay fatal: {e}"));
            }
        };

        match action {
            LoopAction::Reconnect(d) => tokio::time::sleep(d).await,
            LoopAction::ReconnectImmediate => {}
            LoopAction::ExitOk => return Ok(()),
            LoopAction::ExitErr => {
                // Network error 用の ExitErr は上の Err(RelayError::Network) arm で
                // 直接 return しているのでここには来ない。defensive。
                return Err(anyhow!("relay: exit on unrecoverable error"));
            }
        }
    }
}

/// 1 回の WS 接続 〜 切断までを処理。
async fn connect_and_serve<S, RB>(ctx: &RelayContext<S>) -> Result<(), RelayError>
where
    S: Service<HttpRequest<Body>, Response = HttpResponse<RB>, Error = Infallible>
        + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send,
    RB: http_body::Body<Data = Bytes> + Send + 'static,
    RB::Error: std::error::Error + Send + Sync + 'static,
{
    // 1. 期限切れなら refresh してから handshake (handshake 401 を回避)
    {
        let needs_refresh = ctx.jwt.read().await.is_expired(60);
        if needs_refresh {
            refresh_jwt(ctx).await.map_err(RelayError::Fatal)?;
        }
    }
    let access_token = ctx.jwt.read().await.access_token.clone();

    // 2. WS upgrade Request 構築 (Authorization: Bearer)
    let url = ctx.cfg.relay_ws_connect_url(&ctx.login);
    let req = HttpRequest::builder()
        .method("GET")
        .uri(&url)
        .header("Host", host_from_url(&url))
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", generate_key())
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header(
            "User-Agent",
            concat!("github-mcp-server-rs/", env!("CARGO_PKG_VERSION")),
        )
        .body(())
        .map_err(|e| RelayError::Network(anyhow!("build WS request: {e}")))?;

    // 3. handshake
    let (ws_stream, _resp) = match connect_async(req).await {
        Ok(pair) => pair,
        Err(e) => return Err(map_handshake_err(e)),
    };

    // 4. split + writer task setup
    let (sink, mut stream) = ws_stream.split();
    let (out_tx, out_rx) = mpsc::channel::<OutMsg>(64);
    let writer = tokio::spawn(writer_task(sink, out_rx));

    // 5. Hello 送信 (application-level handshake)
    let hello = Frame::hello(env!("CARGO_PKG_VERSION"));
    if out_tx.send(OutMsg::Frame(hello)).await.is_err() {
        // writer がもう死んでる
        writer.abort();
        return Err(RelayError::Network(anyhow!(
            "writer task closed before hello"
        )));
    }

    // 6. reader loop — frame を受けて dispatch
    let mut close_reason: Option<RelayError> = None;
    while let Some(msg) = stream.next().await {
        match msg {
            Ok(Message::Text(txt)) => {
                handle_incoming_text(ctx, &out_tx, &txt).await;
            }
            Ok(Message::Binary(_)) => {
                // 仕様上は Text のみ。Binary は無視。
                tracing::warn!("relay: ignoring unexpected Binary frame");
            }
            Ok(Message::Ping(p)) => {
                let _ = out_tx.send(OutMsg::Pong(p.to_vec())).await;
            }
            Ok(Message::Pong(_)) => { /* keepalive */ }
            Ok(Message::Close(c)) => {
                close_reason = classify_close(c.as_ref());
                break;
            }
            Ok(Message::Frame(_)) => {
                // raw frame — tokio-tungstenite では通常 surface しない
            }
            Err(e) => {
                close_reason = Some(RelayError::Network(anyhow!("ws read: {e}")));
                break;
            }
        }
    }

    drop(out_tx); // writer task drain → exit
    let _ = writer.await;

    match close_reason {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Text frame 受信時の処理。Frame::Req は dispatch して Resp で返す。
async fn handle_incoming_text<S, RB>(ctx: &RelayContext<S>, out: &mpsc::Sender<OutMsg>, text: &str)
where
    S: Service<HttpRequest<Body>, Response = HttpResponse<RB>, Error = Infallible>
        + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send,
    RB: http_body::Body<Data = Bytes> + Send + 'static,
    RB::Error: std::error::Error + Send + Sync + 'static,
{
    let parsed = match Frame::from_json(text) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("relay: malformed frame ({e}): {}", truncate(text, 256));
            return;
        }
    };
    if parsed.version() != FRAME_VERSION {
        tracing::warn!(
            "relay: dropped frame v={} (binary supports v={})",
            parsed.version(),
            FRAME_VERSION
        );
        return;
    }
    match parsed {
        Frame::Req { .. } => {
            let svc = ctx.svc.clone();
            let out = out.clone();
            tokio::spawn(async move {
                let resp = bridge::dispatch_frame(&svc, parsed).await;
                let _ = out.send(OutMsg::Frame(resp)).await;
            });
        }
        Frame::Resp { .. } => {
            // binary 側は Resp を受けない。debug log のみ。
            tracing::debug!("relay: ignoring Resp frame from peer");
        }
        Frame::Hello { .. } => {
            // auth-worker からの Hello は仕様上は無いが、forward-compat で無視。
            tracing::debug!("relay: ignoring Hello frame from peer");
        }
    }
}

enum OutMsg {
    Frame(Frame),
    Pong(Vec<u8>),
}

async fn writer_task(
    mut sink: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    mut rx: mpsc::Receiver<OutMsg>,
) {
    while let Some(out) = rx.recv().await {
        let msg = match out {
            OutMsg::Frame(f) => match f.to_json() {
                Ok(s) => Message::Text(s),
                Err(e) => {
                    tracing::warn!("relay: frame encode failed: {e}");
                    continue;
                }
            },
            OutMsg::Pong(payload) => Message::Pong(payload),
        };
        if let Err(e) = sink.send(msg).await {
            tracing::warn!("relay: ws send failed: {e}");
            break;
        }
    }
    let _ = sink.close().await;
}

/// `auth::refresh()` を呼んで TokenSet を更新、cache file にも save。
async fn refresh_jwt<S>(ctx: &RelayContext<S>) -> Result<()> {
    let refresh_token = { ctx.jwt.read().await.refresh_token.clone() };
    let new_token = auth::refresh(&ctx.http, &ctx.cfg, &refresh_token)
        .await
        .context("MCP /mcp/token refresh_token grant")?;
    new_token
        .save(&ctx.jwt_cache_path)
        .context("save refreshed token")?;
    *ctx.jwt.write().await = new_token;
    Ok(())
}

fn map_handshake_err(err: tokio_tungstenite::tungstenite::Error) -> RelayError {
    use tokio_tungstenite::tungstenite::Error as WsErr;
    match &err {
        WsErr::Http(resp) if resp.status().as_u16() == 401 => RelayError::AuthRejected,
        WsErr::Http(resp) if resp.status().as_u16() == 403 => RelayError::AuthRejected,
        _ => RelayError::Network(anyhow!("ws connect: {err}")),
    }
}

fn classify_close(c: Option<&CloseFrame>) -> Option<RelayError> {
    match c {
        Some(cf) => {
            // Policy (1008) / 401 系 application code (4001) → JWT を疑う
            if cf.code == CloseCode::Policy || u16::from(cf.code) == 4001 {
                Some(RelayError::AuthRejected)
            } else if cf.code == CloseCode::Normal || cf.code == CloseCode::Away {
                None
            } else {
                Some(RelayError::Network(anyhow!(
                    "ws close: code={} reason={}",
                    u16::from(cf.code),
                    cf.reason
                )))
            }
        }
        None => None,
    }
}

fn host_from_url(url: &str) -> String {
    // wss://host/path or ws://host/path から host[:port] を取り出す
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    after_scheme.split('/').next().unwrap_or("").to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…(truncated {} chars)", &s[..max], s.len() - max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_from_url_extracts_host_port() {
        assert_eq!(
            host_from_url("wss://mcp.ippoan.org/u/yhonda-ohishi/connect"),
            "mcp.ippoan.org"
        );
        assert_eq!(
            host_from_url("ws://127.0.0.1:18099/u/dev/connect"),
            "127.0.0.1:18099"
        );
    }

    #[test]
    fn truncate_short_passthrough() {
        assert_eq!(truncate("abc", 10), "abc");
    }

    #[test]
    fn truncate_long_marks_truncation() {
        let s = "a".repeat(20);
        let t = truncate(&s, 10);
        assert!(t.starts_with(&"a".repeat(10)));
        assert!(t.contains("truncated"));
    }

    #[test]
    fn classify_close_normal_returns_none() {
        let cf = CloseFrame {
            code: CloseCode::Normal,
            reason: "bye".into(),
        };
        assert!(classify_close(Some(&cf)).is_none());
    }

    #[test]
    fn classify_close_policy_is_auth_rejected() {
        let cf = CloseFrame {
            code: CloseCode::Policy,
            reason: "JWT mismatch".into(),
        };
        assert!(matches!(
            classify_close(Some(&cf)),
            Some(RelayError::AuthRejected)
        ));
    }

    #[test]
    fn classify_close_4001_is_auth_rejected() {
        let cf = CloseFrame {
            code: CloseCode::Library(4001),
            reason: "expired".into(),
        };
        assert!(matches!(
            classify_close(Some(&cf)),
            Some(RelayError::AuthRejected)
        ));
    }

    #[test]
    fn classify_close_other_is_network() {
        let cf = CloseFrame {
            code: CloseCode::Library(4500),
            reason: "boom".into(),
        };
        assert!(matches!(
            classify_close(Some(&cf)),
            Some(RelayError::Network(_))
        ));
    }

    #[test]
    fn classify_close_none_is_none() {
        assert!(classify_close(None).is_none());
    }

    // ─── LoopState transitions (issue #30) ───────────────────────────────

    #[test]
    fn loop_state_first_clean_close_reconnects_with_cooldown() {
        let mut s = LoopState::default();
        assert_eq!(s.on_clean_close(), LoopAction::Reconnect(RECONNECT_COOLDOWN));
        assert_eq!(s.clean_close_streak, 1);
    }

    #[test]
    fn loop_state_second_clean_close_exits_ok() {
        let mut s = LoopState::default();
        let _ = s.on_clean_close();
        assert_eq!(s.on_clean_close(), LoopAction::ExitOk);
    }

    #[test]
    fn loop_state_auth_rejected_resets_streaks_and_reconnects_immediately() {
        let mut s = LoopState::default();
        let _ = s.on_clean_close();
        let _ = s.on_network_error();
        assert_eq!(s.on_auth_rejected(), LoopAction::ReconnectImmediate);
        assert_eq!(s.clean_close_streak, 0);
        assert_eq!(s.network_error_streak, 0);
    }

    #[test]
    fn loop_state_network_error_backoff_then_exit() {
        let mut s = LoopState::default();
        assert_eq!(
            s.on_network_error(),
            LoopAction::Reconnect(Duration::from_secs(1))
        );
        assert_eq!(
            s.on_network_error(),
            LoopAction::Reconnect(Duration::from_secs(2))
        );
        assert_eq!(s.on_network_error(), LoopAction::ExitErr);
    }

    #[test]
    fn loop_state_clean_close_clears_network_streak() {
        let mut s = LoopState::default();
        let _ = s.on_network_error();
        let _ = s.on_network_error();
        let _ = s.on_clean_close();
        assert_eq!(s.network_error_streak, 0);
        // Subsequent network error は streak=1 から再カウント
        assert_eq!(
            s.on_network_error(),
            LoopAction::Reconnect(Duration::from_secs(1))
        );
    }

    #[test]
    fn loop_state_network_error_clears_clean_close_streak() {
        let mut s = LoopState::default();
        let _ = s.on_clean_close();
        let _ = s.on_network_error();
        assert_eq!(s.clean_close_streak, 0);
        // Next clean close は streak=1 から、即 ExitOk にはならない
        assert_eq!(s.on_clean_close(), LoopAction::Reconnect(RECONNECT_COOLDOWN));
    }
}
