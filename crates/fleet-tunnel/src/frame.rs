//! The reverse-tunnel wire protocol: JSON text frames, bodies base64. Locked with v-task-board;
//! the board side (task-board /tunnel/ws) is built to match these byte-for-byte.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// hello-frame protocol version.
pub const PROTOCOL_VERSION: u32 = 1;

/// A single wire frame. Internally tagged by `t`, snake_case (hello / hello_ok / req / resp / err /
/// ping / pong). Covers both directions: the daemon sends hello/resp/err/ping/pong and receives
/// hello_ok/req/ping/pong.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Frame {
    /// client -> server, first frame: identify host + served agents, authenticate.
    Hello {
        v: u32,
        host: String,
        agents: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
    /// server -> client: handshake accepted, with the keepalive interval (seconds).
    HelloOk {
        #[serde(default)]
        keepalive: Option<u64>,
    },
    /// server -> client: an HTTP request to forward to the local upstream.
    Req {
        id: i64,
        #[serde(default)]
        method: Option<String>,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        #[serde(default)]
        body: Option<String>,
    },
    /// client -> server: the upstream's response for `id`.
    Resp {
        id: i64,
        status: u16,
        headers: BTreeMap<String, String>,
        body: String,
    },
    /// client -> server: the daemon could not fulfil `id` (upstream down / bad body).
    Err {
        id: i64,
        code: String,
        msg: String,
    },
    /// app-level keepalive.
    Ping,
    Pong,
}

impl Frame {
    /// Serialize to a JSON text frame.
    pub fn to_json(&self) -> String {
        // Frame is a closed enum of plain data; serialization cannot fail.
        serde_json::to_string(self).expect("frame serialize")
    }

    /// Parse a JSON text frame.
    pub fn from_json(s: &str) -> Result<Frame, serde_json::Error> {
        serde_json::from_str(s)
    }
}

/// base64-encode a body (empty stays empty, matching the board's convention).
pub fn encode_body(raw: &[u8]) -> String {
    if raw.is_empty() {
        String::new()
    } else {
        BASE64.encode(raw)
    }
}

/// base64-decode a frame body (absent/empty -> empty bytes).
pub fn decode_body(body: &Option<String>) -> Result<Vec<u8>, base64::DecodeError> {
    match body {
        Some(s) if !s.is_empty() => BASE64.decode(s),
        _ => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_serializes_with_tag_and_omits_absent_token() {
        let f = Frame::Hello {
            v: PROTOCOL_VERSION,
            host: "h".into(),
            agents: vec!["a".into()],
            token: None,
        };
        let j = f.to_json();
        assert!(j.contains(r#""t":"hello""#), "{j}");
        assert!(j.contains(r#""v":1"#), "{j}");
        assert!(!j.contains("token"), "absent token must be omitted: {j}");
        assert_eq!(Frame::from_json(&j).unwrap(), f);
    }

    #[test]
    fn hello_includes_token_when_present() {
        let j = Frame::Hello {
            v: 1,
            host: "h".into(),
            agents: vec![],
            token: Some("t".into()),
        }
        .to_json();
        assert!(j.contains(r#""token":"t""#), "{j}");
    }

    #[test]
    fn hello_ok_parses() {
        let f = Frame::from_json(r#"{"t":"hello_ok","keepalive":30}"#).unwrap();
        assert_eq!(
            f,
            Frame::HelloOk {
                keepalive: Some(30)
            }
        );
    }

    #[test]
    fn req_parses_with_body_and_defaults() {
        let f = Frame::from_json(
            r#"{"t":"req","id":7,"method":"POST","path":"/wake","headers":{"content-type":"application/json"},"body":"aGk="}"#,
        )
        .unwrap();
        match f {
            Frame::Req {
                id,
                method,
                path,
                headers,
                body,
            } => {
                assert_eq!(id, 7);
                assert_eq!(method.as_deref(), Some("POST"));
                assert_eq!(path.as_deref(), Some("/wake"));
                assert_eq!(
                    headers.get("content-type").map(String::as_str),
                    Some("application/json")
                );
                assert_eq!(decode_body(&body).unwrap(), b"hi");
            }
            other => panic!("expected req, got {other:?}"),
        }
    }

    #[test]
    fn req_tolerates_missing_optional_fields() {
        let f = Frame::from_json(r#"{"t":"req","id":1}"#).unwrap();
        assert!(matches!(f, Frame::Req { id: 1, .. }));
    }

    #[test]
    fn resp_round_trips() {
        let mut headers = BTreeMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());
        let f = Frame::Resp {
            id: 3,
            status: 200,
            headers,
            body: encode_body(b"ok"),
        };
        assert_eq!(Frame::from_json(&f.to_json()).unwrap(), f);
    }

    #[test]
    fn ping_pong_round_trip() {
        assert_eq!(Frame::Ping.to_json(), r#"{"t":"ping"}"#);
        assert_eq!(Frame::from_json(r#"{"t":"pong"}"#).unwrap(), Frame::Pong);
    }

    #[test]
    fn body_codec_empty_and_roundtrip() {
        assert_eq!(encode_body(b""), "");
        assert_eq!(decode_body(&None).unwrap(), Vec::<u8>::new());
        assert_eq!(decode_body(&Some(String::new())).unwrap(), Vec::<u8>::new());
        let enc = encode_body(b"\x00\x01\xff payload");
        assert_eq!(decode_body(&Some(enc)).unwrap(), b"\x00\x01\xff payload");
    }
}
