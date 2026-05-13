//! Frame schema for the WebSocket relay (issue #27, paired with auth-worker #117 Phase 7).
//!
//! Wire format: **JSON text frame** on the WS, one Frame per WS message.
//!
//! - `Req`   Claude Code Web → auth-worker `POST /u/<user>/mcp` → DO → binary
//! - `Resp`  binary → DO → auth-worker → Claude Code Web (HTTP response body)
//! - `Hello` binary → DO の application-level handshake (binary 起動直後に 1 回送る)
//!
//! 設計理由:
//! - JSON: msgpack より debug 容易。MCP payload 自体 JSON なので帯域差は誤差。
//! - `body_b64`: HTTP body は任意 byte sequence。base64 standard alphabet で encode して JSON に乗せる。
//! - `id` (uuid v4): 1 WS 上で複数 in-flight を許すための correlation id。auth-worker 側は
//!   `POST /u/:user/mcp` を `Frame::Req { id }` 1 件に変換し、`Frame::Resp` を `id` で待ち合わせる。
//! - `v` / `proto`: forward-compat 用。現行 v1 のみ。auth-worker は `proto != FRAME_VERSION` なら
//!   WS を `close(1002, "proto mismatch")` する想定。
//!
//! Out of scope (frame v2 以降で対応):
//! - SSE event boundary / `Last-Event-ID` / `mcp-session-id` 透過
//! - request cancellation
//! - response chunking (MVP は 1 MiB body 上限)

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Frame schema version. auth-worker Phase 7 plan と同期して bump する。
pub const FRAME_VERSION: u32 = 1;

/// 1 WS frame に乗る payload。`#[serde(tag = "kind")]` で discriminator は `kind`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum Frame {
    /// Claude Code Web → binary (HTTP request to be dispatched to MCP service).
    Req {
        v: u32,
        id: String,
        method: String,
        path: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        #[serde(default)]
        body_b64: String,
    },
    /// binary → Claude Code Web (HTTP response).
    Resp {
        v: u32,
        id: String,
        status: u16,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        #[serde(default)]
        body_b64: String,
    },
    /// binary → auth-worker, application-level handshake. WS open 直後に 1 回。
    Hello {
        v: u32,
        binary_version: String,
        proto: u32,
    },
}

impl Frame {
    /// `Hello` frame を作る (binary version は `CARGO_PKG_VERSION` を埋める想定)。
    pub fn hello(binary_version: impl Into<String>) -> Self {
        Frame::Hello {
            v: FRAME_VERSION,
            binary_version: binary_version.into(),
            proto: FRAME_VERSION,
        }
    }

    /// `Resp` frame を組み立てる。`body` は raw byte。base64 encode は内部で実施。
    pub fn resp(
        id: impl Into<String>,
        status: u16,
        headers: BTreeMap<String, String>,
        body: &[u8],
    ) -> Self {
        Frame::Resp {
            v: FRAME_VERSION,
            id: id.into(),
            status,
            headers,
            body_b64: encode_body(body),
        }
    }

    /// JSON 文字列に encode (WS Text frame に乗せる用)。
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// JSON 文字列から decode。
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// Frame の `v` field (Hello は `v == proto`、Req/Resp は `v` field を持つ) を返す。
    /// 受信側 version check 用。
    pub fn version(&self) -> u32 {
        match self {
            Frame::Req { v, .. } | Frame::Resp { v, .. } | Frame::Hello { v, .. } => *v,
        }
    }
}

/// `Frame::Req.body_b64` / `Frame::Resp.body_b64` の base64 → bytes decode。
pub fn decode_body(b64: &str) -> Result<Vec<u8>, base64::DecodeError> {
    if b64.is_empty() {
        return Ok(Vec::new());
    }
    B64.decode(b64.as_bytes())
}

/// bytes → base64 standard alphabet (no url-safe variant — frame schema は standard 固定)。
pub fn encode_body(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    B64.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_req() -> Frame {
        let mut headers = BTreeMap::new();
        headers.insert("content-type".into(), "application/json".into());
        headers.insert(
            "accept".into(),
            "application/json, text/event-stream".into(),
        );
        Frame::Req {
            v: 1,
            id: "11111111-2222-3333-4444-555555555555".into(),
            method: "POST".into(),
            path: "/".into(),
            headers,
            body_b64: encode_body(br#"{"jsonrpc":"2.0","method":"tools/list","id":1}"#),
        }
    }

    #[test]
    fn req_roundtrip_preserves_fields() {
        let f = sample_req();
        let s = f.to_json().unwrap();
        let parsed = Frame::from_json(&s).unwrap();
        assert_eq!(parsed, f);
    }

    #[test]
    fn resp_roundtrip_preserves_fields() {
        let mut headers = BTreeMap::new();
        headers.insert("content-type".into(), "application/json".into());
        let f = Frame::resp("abc", 200, headers, br#"{"ok":true}"#);
        let s = f.to_json().unwrap();
        let parsed = Frame::from_json(&s).unwrap();
        assert_eq!(parsed, f);
    }

    #[test]
    fn hello_roundtrip_preserves_fields() {
        let f = Frame::hello("0.1.0");
        let s = f.to_json().unwrap();
        let parsed = Frame::from_json(&s).unwrap();
        assert_eq!(parsed, f);
        assert_eq!(parsed.version(), FRAME_VERSION);
    }

    #[test]
    fn body_b64_round_trip() {
        let bytes: &[u8] = &[0u8, 1, 2, 3, 0xff, 0xfe, 0xfd];
        let b64 = encode_body(bytes);
        let back = decode_body(&b64).unwrap();
        assert_eq!(back, bytes);
    }

    #[test]
    fn empty_body_round_trip() {
        assert_eq!(encode_body(&[]), "");
        assert_eq!(decode_body("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn unknown_kind_is_rejected() {
        let s = r#"{"kind":"bogus","v":1}"#;
        assert!(Frame::from_json(s).is_err());
    }

    #[test]
    fn unknown_field_is_rejected_thanks_to_deny_unknown_fields() {
        // future-proofing: Phase 7 で誤って extra field を入れたら decode に気付ける
        let s = r#"{"kind":"hello","v":1,"binary_version":"0.1.0","proto":1,"extra":42}"#;
        assert!(Frame::from_json(s).is_err());
    }

    #[test]
    fn missing_id_in_req_is_rejected() {
        let s = r#"{"kind":"req","v":1,"method":"POST","path":"/","headers":{},"body_b64":""}"#;
        assert!(Frame::from_json(s).is_err());
    }

    #[test]
    fn req_with_default_headers_and_body_decodes() {
        // headers / body_b64 は #[serde(default)] なので省略可
        let s = r#"{"kind":"req","v":1,"id":"a","method":"GET","path":"/"}"#;
        let f = Frame::from_json(s).unwrap();
        if let Frame::Req {
            id,
            headers,
            body_b64,
            ..
        } = f
        {
            assert_eq!(id, "a");
            assert!(headers.is_empty());
            assert_eq!(body_b64, "");
        } else {
            panic!("expected Req");
        }
    }

    #[test]
    fn version_field_extraction() {
        assert_eq!(Frame::hello("0.1.0").version(), 1);
        assert_eq!(sample_req().version(), 1);
    }
}
