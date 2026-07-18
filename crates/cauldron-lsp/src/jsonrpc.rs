//! Minimal JSON-RPC 2.0 wire types — helix-lsp's `jsonrpc.rs` minus batch support (LSP servers
//! never batch).
//!
//! Two field-hardened deviations from a by-the-book implementation:
//! - NO `#[serde(deny_unknown_fields)]` anywhere: real servers decorate messages with extra
//!   fields (clangd stamps `requestMethod` on responses, tracing proxies inject `traceparent`)
//!   and a strict parser would kill the session on the first one.
//! - [`Id`] accepts float ids like `4.0`: JavaScript-hosted servers push integer ids through the
//!   JS number type. Integers still round-trip as integers on the way back out.
//!
//! Every enum here is `#[serde(untagged)]`, so VARIANT ORDER is load-bearing (serde tries
//! variants top to bottom): [`Output`] lists `Failure` before `Success` so a body carrying both
//! `error` and `result` resolves as the failure it is, and [`Call`] lists `MethodCall` before
//! `Notification` so a request's `id` is never silently dropped as an unknown field.

use serde::{de, Deserialize, Serialize};
use serde_json::{json, Value};

/// A request id. `Null` only appears in error replies to requests nobody could parse.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Null,
    Num(#[serde(deserialize_with = "deserialize_id_num")] i64),
    Str(String),
}

/// Accept integers AND floats with a zero fractional part (`4.0` → 4). The spec says ids
/// "SHOULD NOT contain fractional parts", and JavaScript servers take that as permission.
fn deserialize_id_num<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let num = serde_json::Number::deserialize(deserializer)?;
    if let Some(val) = num.as_i64() {
        return Ok(val);
    }
    if let Some(val) = num.as_f64().filter(|f| f.fract() == 0.0 && f.abs() <= i64::MAX as f64) {
        return Ok(val as i64);
    }
    Err(de::Error::custom("jsonrpc id must be a whole number"))
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Id::Null => f.write_str("null"),
            Id::Num(n) => write!(f, "{n}"),
            Id::Str(s) => f.write_str(s),
        }
    }
}

/// The protocol version tag. Only "2.0" exists; anything else is a parse error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Version {
    V2,
}

impl Serialize for Version {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("2.0")
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct VersionVisitor;
        impl de::Visitor<'_> for VersionVisitor {
            type Value = Version;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("the string \"2.0\"")
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Version, E> {
                match value {
                    "2.0" => Ok(Version::V2),
                    _ => Err(de::Error::custom("unsupported jsonrpc version")),
                }
            }
        }
        deserializer.deserialize_str(VersionVisitor)
    }
}

/// Request/notification parameters. LSP always sends a map (or nothing), but the spec also
/// allows positional arrays, so we keep all three shapes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Params {
    #[default]
    None,
    Array(Vec<Value>),
    Map(serde_json::Map<String, Value>),
}

impl Params {
    pub fn is_none(&self) -> bool {
        matches!(self, Params::None)
    }

    /// Decode into a concrete params type (e.g. `lsp_types::PublishDiagnosticsParams`).
    pub fn parse<D: de::DeserializeOwned>(self) -> Result<D, Error> {
        let value: Value = self.into();
        serde_json::from_value(value).map_err(|err| Error {
            code: Error::INVALID_PARAMS,
            message: format!("invalid params: {err}"),
            data: None,
        })
    }
}

impl From<Params> for Value {
    fn from(params: Params) -> Value {
        match params {
            Params::None => Value::Null,
            Params::Array(vec) => Value::Array(vec),
            Params::Map(map) => Value::Object(map),
        }
    }
}

/// A request — carries an `id` and therefore expects an answer. On our wire this is only ever
/// the SERVER→client direction (workspace/configuration etc.); our own requests are built by
/// [`request`] as plain JSON for the writer thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MethodCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jsonrpc: Option<Version>,
    pub id: Id,
    pub method: String,
    #[serde(default, skip_serializing_if = "Params::is_none")]
    pub params: Params,
}

/// A notification — fire-and-forget, no id, never answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jsonrpc: Option<Version>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Params::is_none")]
    pub params: Params,
}

/// A JSON-RPC error object. `code` stays a raw i64 because we only ever COMPARE codes (see the
/// associated constants), never exhaustively match them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Error {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl Error {
    /// Our params echo in [`Params::parse`] failures.
    pub const INVALID_PARAMS: i64 = -32602;
    /// Our reply to server→client requests we don't implement.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// LSP: the request raced a didChange (rust-analyzer sends these; swallowed silently —
    /// the next debounced pull covers it).
    pub const CONTENT_MODIFIED: i64 = -32801;
    /// LSP: request arrived before the initialize handshake finished (retried after quiescent).
    pub const SERVER_NOT_INITIALIZED: i64 = -32002;
    /// The client cancelled via `$/cancelRequest`.
    pub const REQUEST_CANCELLED: i64 = -32800;
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "jsonrpc error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

/// A successful response to one of our requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Success {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jsonrpc: Option<Version>,
    pub id: Id,
    pub result: Value,
}

/// A failed response to one of our requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jsonrpc: Option<Version>,
    pub id: Id,
    pub error: Error,
}

/// A response, either way. `Failure` MUST stay listed before `Success`: without
/// `deny_unknown_fields` a body containing BOTH `error` and `result` would otherwise match
/// `Success` first and the error would vanish (the helix-documented untagged-order safeguard).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Output {
    Failure(Failure),
    Success(Success),
}

impl Output {
    /// The id this response answers, regardless of outcome — what the dispatcher keys the
    /// pending-request map on.
    pub fn id(&self) -> &Id {
        match self {
            Output::Failure(f) => &f.id,
            Output::Success(s) => &s.id,
        }
    }
}

impl From<Output> for Result<Value, Error> {
    fn from(output: Output) -> Self {
        match output {
            Output::Success(s) => Ok(s.result),
            Output::Failure(f) => Err(f.error),
        }
    }
}

/// Server-initiated traffic. `MethodCall` MUST stay listed before `Notification`: untagged
/// deserialization ignores unknown fields, so the reverse order would swallow every request's
/// `id` and misparse it as a notification we'd never answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Call {
    MethodCall(MethodCall),
    Notification(Notification),
}

/// Anything the server can put on its stdout: a response to us, or a call to us. `Output` is
/// tried first, but the variants can't actually overlap — a response carries `result`/`error`
/// (which calls lack) and a call carries `method` (which neither response shape requires).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerMessage {
    Output(Output),
    Call(Call),
}

/// Build a request body for the writer thread, `jsonrpc: "2.0"` included.
pub fn request(id: i64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// Build a notification body (no id — never answered).
pub fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

/// Build a success response to a server→client request, echoing the server's id verbatim
/// (float ids were already normalized to integers at parse time).
pub fn response(id: &Id, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// Build an error response (e.g. [`Error::METHOD_NOT_FOUND`] for requests we don't implement).
pub fn error_response(id: &Id, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clangd_initialize_response() {
        // Shape captured from a live clangd 22.1.6 probe on this box.
        let raw = r#"{"id":1,"jsonrpc":"2.0","result":{"capabilities":{"positionEncoding":"utf-8","textDocumentSync":{"change":2,"openClose":true,"save":true},"completionProvider":{"resolveProvider":false,"triggerCharacters":[".","<",">",":","\"","/","*"]},"hoverProvider":true},"serverInfo":{"name":"clangd","version":"clangd version 22.1.6"}}}"#;
        let msg: ServerMessage = serde_json::from_str(raw).unwrap();
        match msg {
            ServerMessage::Output(Output::Success(s)) => {
                assert_eq!(s.id, Id::Num(1));
                assert_eq!(s.result["capabilities"]["positionEncoding"], "utf-8");
                assert_eq!(s.result["capabilities"]["textDocumentSync"]["change"], 2);
            }
            other => panic!("expected a Success output, got {other:?}"),
        }
    }

    #[test]
    fn parses_rust_analyzer_server_status_notification() {
        let raw = r#"{"jsonrpc":"2.0","method":"experimental/serverStatus","params":{"health":"ok","quiescent":true,"message":null}}"#;
        let msg: ServerMessage = serde_json::from_str(raw).unwrap();
        match msg {
            ServerMessage::Call(Call::Notification(n)) => {
                assert_eq!(n.method, "experimental/serverStatus");
                let params: Value = n.params.parse().unwrap();
                assert_eq!(params["quiescent"], true);
            }
            other => panic!("expected a Notification, got {other:?}"),
        }
    }

    #[test]
    fn float_ids_parse_and_integers_round_trip() {
        assert_eq!(serde_json::from_str::<Id>("8").unwrap(), Id::Num(8));
        assert_eq!(serde_json::from_str::<Id>("4.0").unwrap(), Id::Num(4));
        assert!(serde_json::from_str::<Id>("0.5").is_err());
        // Round-trip: integer out, never 4.0 — clangd rejects float ids coming back.
        assert_eq!(serde_json::to_string(&Id::Num(4)).unwrap(), "4");
        // And threaded through a whole response.
        let raw = r#"{"jsonrpc":"2.0","id":4.0,"result":null}"#;
        match serde_json::from_str::<ServerMessage>(raw).unwrap() {
            ServerMessage::Output(Output::Success(s)) => assert_eq!(s.id, Id::Num(4)),
            other => panic!("expected a Success output, got {other:?}"),
        }
    }

    #[test]
    fn tolerates_unknown_extra_fields() {
        // clangd-style extra field on a response...
        let raw = r#"{"jsonrpc":"2.0","result":1,"id":1,"requestMethod":"initialize"}"#;
        match serde_json::from_str::<ServerMessage>(raw).unwrap() {
            ServerMessage::Output(Output::Success(s)) => assert_eq!(s.result, Value::from(1)),
            other => panic!("expected a Success output, got {other:?}"),
        }
        // ...and a tracing-proxy field on a notification.
        let raw = r#"{"traceparent":"00-84b1954e-5f78c8b6-00","jsonrpc":"2.0","method":"window/logMessage","params":{"type":5,"message":"Initialized"}}"#;
        match serde_json::from_str::<ServerMessage>(raw).unwrap() {
            ServerMessage::Call(Call::Notification(n)) => assert_eq!(n.method, "window/logMessage"),
            other => panic!("expected a Notification, got {other:?}"),
        }
    }

    #[test]
    fn error_response_is_failure_not_success() {
        let raw = r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"method not found"}}"#;
        match serde_json::from_str::<ServerMessage>(raw).unwrap() {
            ServerMessage::Output(Output::Failure(f)) => {
                assert_eq!(f.id, Id::Num(2));
                assert_eq!(f.error.code, Error::METHOD_NOT_FOUND);
            }
            other => panic!("expected a Failure output, got {other:?}"),
        }
        // The variant-order safeguard: a nonconforming body carrying BOTH fields is a failure.
        let raw = r#"{"jsonrpc":"2.0","id":3,"result":null,"error":{"code":-32801,"message":"content modified"}}"#;
        match serde_json::from_str::<Output>(raw).unwrap() {
            Output::Failure(f) => assert_eq!(f.error.code, Error::CONTENT_MODIFIED),
            other => panic!("expected a Failure output, got {other:?}"),
        }
    }

    #[test]
    fn server_to_client_request_is_method_call() {
        // rust-analyzer asks for configuration right after `initialized`.
        let raw = r#"{"jsonrpc":"2.0","id":3,"method":"workspace/configuration","params":{"items":[{"section":"rust-analyzer"}]}}"#;
        match serde_json::from_str::<ServerMessage>(raw).unwrap() {
            ServerMessage::Call(Call::MethodCall(c)) => {
                assert_eq!(c.id, Id::Num(3));
                assert_eq!(c.method, "workspace/configuration");
                assert!(!c.params.is_none());
            }
            other => panic!("expected a MethodCall, got {other:?}"),
        }
    }

    #[test]
    fn builders_emit_jsonrpc_2_0() {
        let req = request(7, "textDocument/hover", json!({"position": {"line": 0}}));
        assert_eq!(req["jsonrpc"], "2.0");
        assert_eq!(req["id"], 7);
        // A built request parses back as the request it claims to be.
        match serde_json::from_value::<ServerMessage>(req).unwrap() {
            ServerMessage::Call(Call::MethodCall(c)) => assert_eq!(c.method, "textDocument/hover"),
            other => panic!("expected a MethodCall, got {other:?}"),
        }

        let note = notification("initialized", json!({}));
        assert_eq!(note["jsonrpc"], "2.0");
        assert_eq!(note.get("id"), None);

        // Response builders echo the server's id verbatim, string ids included.
        let ok = response(&Id::Str("cfg-1".into()), Value::Null);
        assert_eq!(ok["id"], "cfg-1");
        assert_eq!(ok["result"], Value::Null);
        let err = error_response(&Id::Num(9), Error::METHOD_NOT_FOUND, "unhandled");
        assert_eq!(err["error"]["code"], -32601);
    }
}
