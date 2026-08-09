//! JSON-RPC 2.0 message shapes.
//!
//! Hand-written rather than reused from a framework because the set is tiny and the
//! decoding rule is unusual: an incoming message is classified by *which fields are
//! present*, not by a tag. `id` + `method` is a request, `id` alone is a response,
//! `method` alone is a notification. A single `#[serde(untagged)]` enum gets this wrong
//! in practice — a response carrying `result: null` is indistinguishable from garbage
//! to untagged matching — so the classification is explicit.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A JSON-RPC request or response id.
///
/// The specification allows strings as well as numbers. We only ever *generate*
/// numbers, but a server's own requests (`window/showMessageRequest`,
/// `client/registerCapability`) may use either, and we must echo back what we received.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    String(String),
}

impl From<i64> for RequestId {
    fn from(value: i64) -> Self {
        Self::Number(value)
    }
}

/// An outgoing request.
#[derive(Debug, Serialize)]
pub struct Request {
    pub jsonrpc: &'static str,
    pub id: RequestId,
    pub method: String,
    pub params: Value,
}

impl Request {
    pub fn new(id: RequestId, method: impl Into<String>, params: Value) -> Self {
        Self { jsonrpc: "2.0", id, method: method.into(), params }
    }
}

/// An outgoing notification: a request with no id and therefore no reply.
#[derive(Debug, Serialize)]
pub struct Notification {
    pub jsonrpc: &'static str,
    pub method: String,
    pub params: Value,
}

impl Notification {
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self { jsonrpc: "2.0", method: method.into(), params }
    }
}

/// An outgoing response to a request the *server* sent us.
#[derive(Debug, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: RequestId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

impl Response {
    pub fn success(id: RequestId, result: Value) -> Self {
        Self { jsonrpc: "2.0", id, result: Some(result), error: None }
    }

    pub fn error(id: RequestId, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(ResponseError { code, message: message.into(), data: None }),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResponseError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl std::fmt::Display for ResponseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)
    }
}

/// Error codes the specification defines and we act on.
pub mod error_codes {
    /// The server acknowledging a `$/cancelRequest`. Not a failure: the reply is the
    /// expected outcome of cancelling, and surfacing it as an error to the UI would
    /// turn ordinary fast typing into a stream of error messages.
    pub const REQUEST_CANCELLED: i64 = -32800;
    pub const CONTENT_MODIFIED: i64 = -32801;
    pub const METHOD_NOT_FOUND: i64 = -32601;
}

/// Anything the server can send us, classified by shape.
#[derive(Debug)]
pub enum Incoming {
    /// A reply to a request we sent.
    Response { id: RequestId, result: Result<Value, ResponseError> },
    /// A server-initiated request that expects a reply from us.
    Request { id: RequestId, method: String, params: Value },
    /// A server-initiated notification, such as `textDocument/publishDiagnostics`.
    Notification { method: String, params: Value },
}

/// The wire form, before classification.
#[derive(Deserialize)]
struct RawMessage {
    #[serde(default)]
    id: Option<RequestId>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<ResponseError>,
}

impl Incoming {
    /// Classifies a decoded message body.
    ///
    /// Returns `None` for anything unrecognisable. A malformed message from a
    /// misbehaving server must not take the client down (§24) — the caller logs and
    /// keeps reading, because the alternative is that one bad frame ends LSP for the
    /// session.
    pub fn parse(body: &[u8]) -> Option<Self> {
        let raw: RawMessage = serde_json::from_slice(body).ok()?;

        match (raw.id, raw.method) {
            (Some(id), Some(method)) => {
                Some(Self::Request { id, method, params: raw.params.unwrap_or(Value::Null) })
            }
            (Some(id), None) => {
                // A response is `result` xor `error`. `result` may legitimately be
                // `null` (a hover with nothing to show), so absence of both fields
                // still means a successful null result.
                let result = match raw.error {
                    Some(error) => Err(error),
                    None => Ok(raw.result.unwrap_or(Value::Null)),
                };
                Some(Self::Response { id, result })
            }
            (None, Some(method)) => {
                Some(Self::Notification { method, params: raw.params.unwrap_or(Value::Null) })
            }
            (None, None) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Option<Incoming> {
        Incoming::parse(json.as_bytes())
    }

    #[test]
    fn classifies_a_response() {
        let message = parse(r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#).unwrap();
        match message {
            Incoming::Response { id, result } => {
                assert_eq!(id, RequestId::Number(1));
                assert_eq!(result.unwrap()["ok"], true);
            }
            other => panic!("expected a response, got {other:?}"),
        }
    }

    #[test]
    fn a_null_result_is_success_not_failure() {
        // `textDocument/hover` with nothing to report replies `result: null`. Treating
        // that as an error would show a spurious failure every time the cursor sits on
        // whitespace.
        let message = parse(r#"{"jsonrpc":"2.0","id":7,"result":null}"#).unwrap();
        match message {
            Incoming::Response { result, .. } => assert_eq!(result.unwrap(), Value::Null),
            other => panic!("expected a response, got {other:?}"),
        }
    }

    #[test]
    fn classifies_an_error_response() {
        let message =
            parse(r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"nope"}}"#).unwrap();
        match message {
            Incoming::Response { result, .. } => {
                let error = result.unwrap_err();
                assert_eq!(error.code, error_codes::METHOD_NOT_FOUND);
                assert_eq!(error.message, "nope");
            }
            other => panic!("expected a response, got {other:?}"),
        }
    }

    #[test]
    fn classifies_a_notification() {
        let message =
            parse(r#"{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{}}"#)
                .unwrap();
        match message {
            Incoming::Notification { method, .. } => {
                assert_eq!(method, "textDocument/publishDiagnostics");
            }
            other => panic!("expected a notification, got {other:?}"),
        }
    }

    #[test]
    fn classifies_a_server_initiated_request() {
        // Distinguished from a notification only by the presence of `id`.
        let message = parse(
            r#"{"jsonrpc":"2.0","id":"abc","method":"window/showMessageRequest","params":{}}"#,
        )
        .unwrap();
        match message {
            Incoming::Request { id, method, .. } => {
                assert_eq!(id, RequestId::String("abc".into()));
                assert_eq!(method, "window/showMessageRequest");
            }
            other => panic!("expected a request, got {other:?}"),
        }
    }

    #[test]
    fn string_ids_round_trip() {
        let id = RequestId::String("weird-id".into());
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, r#""weird-id""#);
        assert_eq!(serde_json::from_str::<RequestId>(&json).unwrap(), id);
    }

    #[test]
    fn garbage_is_rejected_without_panicking() {
        assert!(parse("not json at all").is_none());
        assert!(parse(r#"{"jsonrpc":"2.0"}"#).is_none());
        assert!(Incoming::parse(&[0xff, 0xfe]).is_none());
    }

    #[test]
    fn requests_serialise_to_the_wire_format() {
        let request =
            Request::new(RequestId::Number(3), "textDocument/hover", serde_json::json!({}));
        let json: Value = serde_json::from_str(&serde_json::to_string(&request).unwrap()).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 3);
        assert_eq!(json["method"], "textDocument/hover");
    }

    #[test]
    fn notifications_carry_no_id() {
        let notification = Notification::new("initialized", serde_json::json!({}));
        let json: Value =
            serde_json::from_str(&serde_json::to_string(&notification).unwrap()).unwrap();
        assert!(json.get("id").is_none(), "a notification must not carry an id");
    }

    #[test]
    fn responses_omit_the_unused_half() {
        let ok = serde_json::to_string(&Response::success(1.into(), Value::Null)).unwrap();
        assert!(!ok.contains("error"), "{ok}");

        let bad = serde_json::to_string(&Response::error(1.into(), -1, "boom")).unwrap();
        assert!(!bad.contains("result"), "{bad}");
        assert!(bad.contains("boom"));
    }
}
