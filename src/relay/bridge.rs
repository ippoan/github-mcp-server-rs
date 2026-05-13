//! `Frame::Req` ↔ `axum::Request` / `axum::Response` ↔ `Frame::Resp` 変換 +
//! `tower::Service` (= rmcp `StreamableHttpService`) への dispatch。
//!
//! 設計判断:
//! - **path は `/`**。auth-worker bridge handler 側で `POST /u/<user>/mcp` を受けた時に
//!   `Frame::Req.path = "/"` で frame を組み立てる前提 (path = "/mcp" にすると、
//!   binary 側で nest 無しの service を直接呼ぶと 404 になる)。
//! - **body 上限 1 MiB**。MVP の MCP tool (whoami, list_repos) は数 KB なので余裕。
//!   超える tool が出たら frame v2 で chunked 対応。
//! - **`tower::ServiceExt::oneshot`** で `poll_ready` + `call` を 1 回で済ませる。
//!   service は `Clone` (rmcp `StreamableHttpService` は internal Arc を持つ)。
//! - **response body type は generic** (`RB: http_body::Body<Data = Bytes>`)。
//!   rmcp 1.6.0 の `StreamableHttpService` は `Response<BoxBody<Bytes, Infallible>>` を返すため。

use anyhow::{Context, Result};
use axum::body::Body;
use bytes::Bytes;
use http::header::{HeaderName, HeaderValue};
use http::{Method, Request, Response};
use http_body_util::{BodyExt, Limited};
use std::collections::BTreeMap;
use std::convert::Infallible;
use tower::{Service, ServiceExt};

#[cfg(test)]
use super::frame::encode_body;
use super::frame::{decode_body, Frame};

/// Frame::Resp に詰める部分情報 (frame 組み立ては caller 側で `Frame::resp(...)` に分離)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RespParts {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

/// MCP request body の hard limit (1 MiB)。
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// `Frame::Req` の中身 (caller が unpack 済み) を受け、tower service に dispatch して
/// `RespParts` を返す。
///
/// 失敗パターン:
/// - method / header の parse 失敗 → 400 で返す (frame として呼び出し側に渡す)
/// - body decode 失敗 → 400
/// - service が internal error → 500
/// - response body が `MAX_BODY_BYTES` 超過 → 500 (response truncate ではなく fail)
pub async fn dispatch_req<S, RB>(
    svc: &S,
    method: &str,
    path: &str,
    headers: &BTreeMap<String, String>,
    body_b64: &str,
) -> RespParts
where
    S: Service<Request<Body>, Response = Response<RB>, Error = Infallible>
        + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send,
    RB: http_body::Body<Data = Bytes> + Send + 'static,
    RB::Error: std::error::Error + Send + Sync + 'static,
{
    match build_axum_request(method, path, headers, body_b64) {
        Ok(req) => match svc.clone().oneshot(req).await {
            Ok(resp) => match collect_response(resp).await {
                Ok(parts) => parts,
                Err(e) => error_resp(500, format!("response collect failed: {e}")),
            },
            Err(e) => match e {}, // Infallible
        },
        Err(e) => error_resp(400, e.to_string()),
    }
}

/// Frame の値 (method/path/headers/body_b64) から axum::Request<Body> を組み立てる。
pub(crate) fn build_axum_request(
    method: &str,
    path: &str,
    headers: &BTreeMap<String, String>,
    body_b64: &str,
) -> Result<Request<Body>> {
    let body_bytes = decode_body(body_b64).context("body_b64 base64 decode")?;
    if body_bytes.len() > MAX_BODY_BYTES {
        anyhow::bail!(
            "request body exceeds {} bytes ({} given)",
            MAX_BODY_BYTES,
            body_bytes.len()
        );
    }
    let m = Method::from_bytes(method.as_bytes()).context("invalid method")?;
    let mut builder = Request::builder().method(m).uri(path);
    for (k, v) in headers {
        let hn = HeaderName::from_bytes(k.as_bytes())
            .with_context(|| format!("invalid header name: {k}"))?;
        let hv =
            HeaderValue::from_str(v).with_context(|| format!("invalid header value for {k}"))?;
        builder = builder.header(hn, hv);
    }
    let req = builder
        .body(Body::from(body_bytes))
        .context("build request body")?;
    Ok(req)
}

/// http::Response<RB> を bytes + headers + status に分解。`MAX_BODY_BYTES` で limited collect。
pub(crate) async fn collect_response<RB>(resp: Response<RB>) -> Result<RespParts>
where
    RB: http_body::Body<Data = Bytes> + Send + 'static,
    RB::Error: std::error::Error + Send + Sync + 'static,
{
    let (parts, body) = resp.into_parts();
    let limited = Limited::new(body, MAX_BODY_BYTES);
    let collected = limited
        .collect()
        .await
        .map_err(|e| anyhow::anyhow!("collect response body: {e}"))?;
    let bytes = collected.to_bytes();
    let mut headers = BTreeMap::new();
    for (k, v) in parts.headers.iter() {
        if let Ok(s) = v.to_str() {
            headers.insert(k.as_str().to_string(), s.to_string());
        }
    }
    Ok(RespParts {
        status: parts.status.as_u16(),
        headers,
        body: bytes.to_vec(),
    })
}

fn error_resp(status: u16, msg: String) -> RespParts {
    let mut headers = BTreeMap::new();
    headers.insert("content-type".into(), "application/json".into());
    let body = serde_json::json!({
        "error": "relay_bridge_error",
        "message": msg,
    });
    RespParts {
        status,
        headers,
        body: body.to_string().into_bytes(),
    }
}

/// Convenience: `Frame::Req` を直接受けて `Frame::Resp` を返す。
pub async fn dispatch_frame<S, RB>(svc: &S, req: Frame) -> Frame
where
    S: Service<Request<Body>, Response = Response<RB>, Error = Infallible>
        + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send,
    RB: http_body::Body<Data = Bytes> + Send + 'static,
    RB::Error: std::error::Error + Send + Sync + 'static,
{
    match req {
        Frame::Req {
            id,
            method,
            path,
            headers,
            body_b64,
            ..
        } => {
            let parts = dispatch_req(svc, &method, &path, &headers, &body_b64).await;
            Frame::resp(id, parts.status, parts.headers, &parts.body)
        }
        // Hello / Resp は binary 側 service への dispatch 対象外 — caller のミス
        other => Frame::resp(
            "00000000-0000-0000-0000-000000000000",
            500,
            BTreeMap::new(),
            format!("dispatch_frame: unexpected frame v={}", other.version()).as_bytes(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use http::{Request, Response, StatusCode};
    use std::convert::Infallible;
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context as TaskCtx, Poll};

    /// 任意の status / body を返す `tower::Service<Request<Body>>` 実装 (mock)。
    /// Response<axum::Body> を返すので bridge の RB generic は axum::Body で具体化される。
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
                let (parts, body) = req.into_parts();
                let body_bytes = to_bytes(body, MAX_BODY_BYTES).await.unwrap();
                let path = parts.uri.path().to_string();
                let mut resp = Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .header("x-echo-path", &path)
                    .body(Body::from(body_bytes))
                    .unwrap();
                if let Some(ct) = parts.headers.get("content-type") {
                    resp.headers_mut().insert("x-echo-content-type", ct.clone());
                }
                Ok(resp)
            })
        }
    }

    #[tokio::test]
    async fn dispatch_req_round_trip_body() {
        let svc = EchoSvc;
        let mut headers = BTreeMap::new();
        headers.insert("content-type".into(), "application/json".into());
        let body = br#"{"hello":"world"}"#;
        let body_b64 = encode_body(body);

        let parts = dispatch_req(&svc, "POST", "/", &headers, &body_b64).await;
        assert_eq!(parts.status, 200);
        assert_eq!(parts.body, body);
        assert_eq!(
            parts.headers.get("x-echo-path").map(|s| s.as_str()),
            Some("/")
        );
        assert_eq!(
            parts.headers.get("content-type").map(|s| s.as_str()),
            Some("application/json")
        );
    }

    #[tokio::test]
    async fn dispatch_req_invalid_method_returns_400() {
        let svc = EchoSvc;
        let parts = dispatch_req(&svc, "BAD METHOD", "/", &BTreeMap::new(), "").await;
        assert_eq!(parts.status, 400);
        assert!(String::from_utf8_lossy(&parts.body).contains("relay_bridge_error"));
    }

    #[tokio::test]
    async fn dispatch_req_invalid_header_returns_400() {
        let svc = EchoSvc;
        let mut headers = BTreeMap::new();
        // header value with embedded \n is invalid in HTTP
        headers.insert("x-bad".into(), "line1\nline2".into());
        let parts = dispatch_req(&svc, "POST", "/", &headers, "").await;
        assert_eq!(parts.status, 400);
    }

    #[tokio::test]
    async fn dispatch_req_oversized_body_rejected() {
        let svc = EchoSvc;
        // base64 encoded 2 MiB → decoded > 1 MiB → reject before service call
        let big = vec![b'x'; MAX_BODY_BYTES + 100];
        let body_b64 = encode_body(&big);
        let parts = dispatch_req(&svc, "POST", "/", &BTreeMap::new(), &body_b64).await;
        assert_eq!(parts.status, 400);
        assert!(String::from_utf8_lossy(&parts.body).contains("exceeds"));
    }

    #[tokio::test]
    async fn dispatch_req_empty_body_works() {
        let svc = EchoSvc;
        let parts = dispatch_req(&svc, "GET", "/", &BTreeMap::new(), "").await;
        assert_eq!(parts.status, 200);
        assert!(parts.body.is_empty());
    }

    #[tokio::test]
    async fn dispatch_frame_wraps_into_resp_with_same_id() {
        let svc = EchoSvc;
        let id = "abc-123";
        let mut headers = BTreeMap::new();
        headers.insert("content-type".into(), "text/plain".into());
        let req = Frame::Req {
            v: 1,
            id: id.into(),
            method: "POST".into(),
            path: "/".into(),
            headers,
            body_b64: encode_body(b"ping"),
        };
        let resp = dispatch_frame(&svc, req).await;
        match resp {
            Frame::Resp {
                id: r_id,
                status,
                body_b64,
                ..
            } => {
                assert_eq!(r_id, id);
                assert_eq!(status, 200);
                assert_eq!(
                    super::super::frame::decode_body(&body_b64).unwrap(),
                    b"ping"
                );
            }
            other => panic!("expected Resp, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_frame_unexpected_kind_returns_synthetic_resp() {
        let svc = EchoSvc;
        let resp = dispatch_frame(&svc, Frame::hello("0.1.0")).await;
        match resp {
            Frame::Resp {
                status, body_b64, ..
            } => {
                assert_eq!(status, 500);
                let body = super::super::frame::decode_body(&body_b64).unwrap();
                assert!(String::from_utf8_lossy(&body).contains("unexpected frame"));
            }
            _ => panic!("expected Resp"),
        }
    }
}
