// `#[path]` include で src/ を test crate に mount する都合、test crate からは
// 大半の関数 / struct が未使用扱いになる (relay 以外は test code path に出てこないため)。
// 本来 src/ コードは bin から使われていて生き物なので、test crate scope では抑制。
//
// `clippy::result_large_err` は tokio-tungstenite の `accept_hdr_async` callback の
// 戻り値型 `Result<Response, ErrorResponse>` (Err variant が大きい) で発火。external API
// 都合で box 化できないので test ではこれも抑制。
#![allow(dead_code, unused_imports, clippy::result_large_err)]

//! Integration smoke test for the WS relay (issue #27).
//!
//! - mock auth-worker WS server (`tokio-tungstenite::accept_async`) を `127.0.0.1:0` で立てる
//! - 受信した WS upgrade Request の `Authorization: Bearer test-jwt` を assert
//! - mock server から `Frame::Req` を送り、 `Frame::Resp` (`id` 一致 + body echo) を受信
//! - mock server が `close(1000)` で切断 → binary 側 backoff loop に入ることを timeout で確認
//!
//! 本テストは binary 側 frame schema の self-consistency 確認用。auth-worker Phase 7 の
//! TypeScript 実装と契約が合っているかは、PR review で frame.rs を diff レビュー。
//!
//! NOTE: github-mcp-server-rs は bin crate のため、test 側から module を import するために
//! `#[path]` で src/ を直接 include する。include 側の `use crate::*` を満たすため、mod 名は
//! src/ の構造と一致させてある。

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskCtx, Poll};
use std::time::Duration;

use axum::body::Body;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use http::{Request, Response, StatusCode};
use tempfile::tempdir;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, RwLock};
use tokio_tungstenite::{accept_hdr_async, tungstenite::Message};
use tower::Service;

// Phase 2: auth / config / pair / relay / token_cache は mcp-relay crate に移動した。
// 整数テストは crate 直接 import に切替。残った binary-only modules (admin_exec /
// mcp_server / github_api / introspect / tools) はこのテストでは未使用。
use mcp_relay::config::{AuthEnv, Config};
use mcp_relay::relay::{run_relay, RelayContext};
use mcp_relay::token_cache::TokenSet;

#[derive(Clone)]
struct EchoSvc;

impl Service<Request<Body>> for EchoSvc {
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut TaskCtx<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        Box::pin(async move {
            use http_body_util::BodyExt;
            let (parts, body) = req.into_parts();
            let bytes = body.collect().await.unwrap().to_bytes();
            let mut resp = Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .header("x-echo-method", parts.method.as_str())
                .header("x-echo-path", parts.uri.path())
                .body(Body::from(bytes))
                .unwrap();
            if let Some(ct) = parts.headers.get("content-type") {
                resp.headers_mut().insert("x-echo-content-type", ct.clone());
            }
            Ok(resp)
        })
    }
}

async fn spawn_relay_against(
    host_port: &str,
    login: &str,
    state_dir: Option<std::path::PathBuf>,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    let cache_dir = tempdir().unwrap().keep();
    let cache = cache_dir.join("token-staging.json");
    let token = TokenSet {
        access_token: "test-jwt".to_string(),
        refresh_token: "test-refresh".to_string(),
        scope: "mcp.read mcp.write".to_string(),
        expires_at: Utc::now().timestamp() + 3600,
        obtained_at: Utc::now(),
    };
    token.save(&cache).unwrap();

    let cfg = Config {
        env: AuthEnv::Staging,
        auth_base: format!("http://{host_port}"),
        relay_base: format!("ws://{host_port}"),
        internal_shared_secret: "x".into(),
        client_id: "test".into(),
        scope: "mcp.read mcp.write".into(),
        project_name: "github-mcp-server-rs",
    };

    let svc = EchoSvc;
    let ctx = RelayContext {
        cfg: Arc::new(cfg),
        http: reqwest::Client::new(),
        login: login.to_string(),
        jwt: Arc::new(RwLock::new(token)),
        jwt_cache_path: cache,
        svc,
        state_dir,
        print_status: false,
        service: "github-mcp-server-rs",
        binary_version: env!("CARGO_PKG_VERSION"),
    };

    tokio::spawn(async move { run_relay(ctx).await })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_round_trip_with_mock_ws_server() {
    // 1. mock WS server を 127.0.0.1:0 で立てる
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let host_port = format!("{}:{}", addr.ip(), addr.port());

    let (assert_tx, mut assert_rx) = mpsc::channel::<Result<(), String>>(8);

    let server_task = tokio::spawn(async move {
        let (stream, _peer) = listener.accept().await.unwrap();
        let mut auth_seen: Option<String> = None;
        let ws = accept_hdr_async(stream, |req: &http::Request<()>, resp| {
            if let Some(v) = req.headers().get("authorization") {
                auth_seen = v.to_str().ok().map(|s| s.to_string());
            }
            Ok(resp)
        })
        .await
        .unwrap();

        let auth_ok = matches!(auth_seen.as_deref(), Some("Bearer test-jwt"));
        let _ = assert_tx
            .send(if auth_ok {
                Ok(())
            } else {
                Err(format!("authorization header was {:?}", auth_seen))
            })
            .await;

        let (mut sink, mut stream) = ws.split();

        // Hello を受信するはず
        let hello = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("timeout waiting for hello")
            .expect("ws stream ended");
        let hello_text = match hello.unwrap() {
            Message::Text(t) => t.to_string(),
            other => panic!("expected text frame, got {other:?}"),
        };
        let hello_v: serde_json::Value = serde_json::from_str(&hello_text).unwrap();
        let _ = assert_tx
            .send(
                if hello_v.get("kind").and_then(|x| x.as_str()) == Some("hello") {
                    Ok(())
                } else {
                    Err(format!("first frame was not hello: {hello_text}"))
                },
            )
            .await;

        // Req を 1 件送る
        let req_id = "test-correlation-id";
        let body_bytes = b"hello-from-test";
        let body_b64 = <base64::engine::general_purpose::GeneralPurpose as base64::Engine>::encode(
            &base64::engine::general_purpose::STANDARD,
            body_bytes,
        );
        let req_json = serde_json::json!({
            "kind": "req",
            "v": 1,
            "id": req_id,
            "method": "POST",
            "path": "/",
            "headers": { "content-type": "application/json" },
            "body_b64": body_b64,
        });
        sink.send(Message::Text(req_json.to_string()))
            .await
            .unwrap();

        // Resp を待つ
        let resp = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("timeout waiting for resp")
            .expect("ws stream ended");
        let resp_text = match resp.unwrap() {
            Message::Text(t) => t.to_string(),
            other => panic!("expected text frame, got {other:?}"),
        };
        let resp_v: serde_json::Value = serde_json::from_str(&resp_text).unwrap();

        let mut errs: Vec<String> = vec![];
        if resp_v.get("kind").and_then(|x| x.as_str()) != Some("resp") {
            errs.push(format!("kind != resp ({resp_text})"));
        }
        if resp_v.get("id").and_then(|x| x.as_str()) != Some(req_id) {
            errs.push(format!("id mismatch ({resp_text})"));
        }
        if resp_v.get("status").and_then(|x| x.as_u64()) != Some(200) {
            errs.push(format!("status != 200 ({resp_text})"));
        }
        if let Some(b64) = resp_v.get("body_b64").and_then(|x| x.as_str()) {
            let decoded =
                <base64::engine::general_purpose::GeneralPurpose as base64::Engine>::decode(
                    &base64::engine::general_purpose::STANDARD,
                    b64.as_bytes(),
                )
                .unwrap();
            if decoded != body_bytes {
                errs.push(format!(
                    "body mismatch: got {:?}",
                    String::from_utf8_lossy(&decoded)
                ));
            }
        } else {
            errs.push("no body_b64".into());
        }
        let _ = assert_tx
            .send(if errs.is_empty() {
                Ok(())
            } else {
                Err(errs.join("; "))
            })
            .await;

        // close で binary 側を切る (binary は backoff して reconnect しに来るが、test 側は accept しない)
        let _ = sink.send(Message::Close(None)).await;
    });

    // 2. binary の relay loop を spawn
    let state = tempdir().unwrap();
    let relay_handle = spawn_relay_against(
        &host_port,
        "yhonda-ohishi",
        Some(state.path().to_path_buf()),
    )
    .await;

    // 3. server task が完走するのを待つ
    tokio::time::timeout(Duration::from_secs(15), server_task)
        .await
        .expect("server_task timed out")
        .unwrap();

    // 4. 全 assertion を確認
    drain_asserts(&mut assert_rx).await;

    // 5. state_dir/url が固定 URL で書かれていること
    let url_path = state.path().join("url");
    let url_content = std::fs::read_to_string(&url_path)
        .unwrap_or_else(|_| panic!("state_dir/url not written: {}", url_path.display()));
    assert_eq!(
        url_content,
        format!("http://{host_port}/u/yhonda-ohishi/mcp"),
        "public URL in state file did not match expected"
    );

    // 6. binary 側 task は再接続 loop に入っている → abort
    relay_handle.abort();
    let _ = tokio::time::timeout(Duration::from_secs(2), relay_handle).await;
}

async fn drain_asserts(rx: &mut mpsc::Receiver<Result<(), String>>) {
    while let Ok(item) = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
        match item {
            Some(Ok(())) => {}
            Some(Err(e)) => panic!("server-side assertion failed: {e}"),
            None => break,
        }
    }
}
