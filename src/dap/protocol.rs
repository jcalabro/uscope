//! Debug Adapter Protocol message types.
//!
//! Only the messages and fields uscope uses are modeled. Unknown fields are
//! ignored, absent or `null` arguments read as empty, and a client's `seq`
//! is echoed exactly as sent, since some clients send strings.

use serde::Deserialize;
use serde_json::{Value, json};

/// A message received from the client.
#[derive(Debug)]
pub enum Incoming {
    Request {
        seq: Value,
        command: String,
        arguments: Value,
    },
    /// The client's answer to a reverse request.
    Response {
        request_seq: Option<u64>,
        /// The response's body, or its error message.
        result: Result<Value, String>,
    },
}

/// Why a message could not be understood.
#[derive(Debug, thiserror::Error)]
pub enum MessageError {
    #[error("the message is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("the message is not a JSON object")]
    NotAnObject,
    #[error("the message has no `type`")]
    MissingType,
    #[error("the request has no `command`")]
    MissingCommand,
    #[error("unsupported message type {0:?}")]
    UnsupportedType(String),
}

impl MessageError {
    /// The `seq` of a request that could not be parsed, so its error can
    /// still be answered.
    pub fn request_seq(body: &[u8]) -> Option<(Value, String)> {
        let value = serde_json::from_slice::<Value>(body).ok()?;
        let seq = value.get("seq")?.clone();
        let command = value
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Some((seq, command))
    }
}

/// Parses one message body.
pub fn parse(body: &[u8]) -> Result<Incoming, MessageError> {
    let value = serde_json::from_slice::<Value>(body)?;
    let Value::Object(mut object) = value else {
        return Err(MessageError::NotAnObject);
    };
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or(MessageError::MissingType)?
        .to_owned();
    match kind.as_str() {
        "request" => Ok(Incoming::Request {
            seq: object.remove("seq").unwrap_or(Value::Null),
            command: object
                .remove("command")
                .and_then(|command| command.as_str().map(str::to_owned))
                .ok_or(MessageError::MissingCommand)?,
            arguments: match object.remove("arguments") {
                None | Some(Value::Null) => Value::Object(serde_json::Map::new()),
                Some(arguments) => arguments,
            },
        }),
        "response" => Ok(Incoming::Response {
            request_seq: object.get("request_seq").and_then(Value::as_u64),
            result: if object.get("success").and_then(Value::as_bool) == Some(true) {
                Ok(object.remove("body").unwrap_or(Value::Null))
            } else {
                Err(object
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("the client refused the request")
                    .to_owned())
            },
        }),
        other => Err(MessageError::UnsupportedType(other.to_owned())),
    }
}

/// A message to send to the client; the writer numbers it.
#[derive(Debug)]
pub enum Outgoing {
    Response {
        request_seq: Value,
        command: String,
        result: Result<Value, ErrorBody>,
    },
    Event {
        event: &'static str,
        body: Value,
    },
    /// A reverse request, whose response settles `ticket`.
    Request {
        command: &'static str,
        arguments: Value,
        ticket: u64,
    },
}

impl Outgoing {
    /// Serializes the message with the writer's `seq`. Every response and
    /// event carries a body object, which strict clients require.
    pub fn to_json(&self, seq: u64) -> Value {
        match self {
            Self::Response {
                request_seq,
                command,
                result: Ok(body),
            } => json!({
                "seq": seq,
                "type": "response",
                "request_seq": request_seq,
                "success": true,
                "command": command,
                "body": object_or_empty(body),
            }),
            Self::Response {
                request_seq,
                command,
                result: Err(error),
            } => json!({
                "seq": seq,
                "type": "response",
                "request_seq": request_seq,
                "success": false,
                "command": command,
                "message": error.short,
                "body": {"error": {
                    "id": 1,
                    "format": error.format,
                    "showUser": error.show_user,
                }},
            }),
            Self::Event { event, body } => json!({
                "seq": seq,
                "type": "event",
                "event": event,
                "body": object_or_empty(body),
            }),
            Self::Request {
                command, arguments, ..
            } => json!({
                "seq": seq,
                "type": "request",
                "command": command,
                "arguments": arguments,
            }),
        }
    }
}

fn object_or_empty(body: &Value) -> Value {
    if body.is_null() {
        json!({})
    } else {
        body.clone()
    }
}

/// A failed request's explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorBody {
    /// The raw error in short form: `notStopped`, `cancelled`, or the
    /// message itself.
    pub short: String,
    /// The message shown to the user.
    pub format: String,
    pub show_user: bool,
}

impl ErrorBody {
    /// An error whose message the client may show.
    pub fn new(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            short: message.clone(),
            format: escape_format(&message),
            show_user: false,
        }
    }

    /// An error the user must see, such as an invalid launch configuration.
    pub fn shown(message: impl Into<String>) -> Self {
        Self {
            show_user: true,
            ..Self::new(message)
        }
    }

    /// The well-known error for a request that needs a stopped inferior.
    pub fn not_stopped() -> Self {
        Self {
            short: "notStopped".to_owned(),
            ..Self::new("the program is running; this request needs it stopped")
        }
    }

    /// The well-known error for a request that was cancelled.
    pub fn cancelled() -> Self {
        Self {
            short: "cancelled".to_owned(),
            ..Self::new("the request was cancelled")
        }
    }
}

/// Escapes braces, which a `Message.format` string uses for variables.
fn escape_format(message: &str) -> String {
    message.replace('{', "{{").replace('}', "}}")
}

/// `initialize` arguments: the client's capabilities.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InitializeArguments {
    pub lines_start_at1: Option<bool>,
    pub columns_start_at1: Option<bool>,
    pub supports_variable_type: Option<bool>,
    pub supports_run_in_terminal_request: Option<bool>,
    pub supports_start_debugging_request: Option<bool>,
    pub supports_memory_references: Option<bool>,
    pub supports_progress_reporting: Option<bool>,
    pub supports_invalidated_event: Option<bool>,
    pub supports_memory_event: Option<bool>,
    #[serde(rename = "supportsANSIStyling")]
    pub supports_ansi_styling: Option<bool>,
}

/// A source file as the client names it.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Source {
    pub path: Option<String>,
}

/// One requested source breakpoint.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SourceBreakpoint {
    pub line: i64,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    pub log_message: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SetBreakpointsArguments {
    pub source: Source,
    pub breakpoints: Option<Vec<SourceBreakpoint>>,
    /// The deprecated form of `breakpoints`: lines only.
    pub lines: Option<Vec<i64>>,
}

/// One requested function breakpoint.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FunctionBreakpoint {
    pub name: String,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SetFunctionBreakpointsArguments {
    pub breakpoints: Vec<FunctionBreakpoint>,
}

/// One requested instruction breakpoint.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InstructionBreakpoint {
    pub instruction_reference: String,
    pub offset: Option<i64>,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SetInstructionBreakpointsArguments {
    pub breakpoints: Vec<InstructionBreakpoint>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BreakpointLocationsArguments {
    pub source: Source,
    pub line: i64,
    pub end_line: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GotoTargetsArguments {
    pub source: Source,
    pub line: i64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GotoArguments {
    pub thread_id: i64,
    pub target_id: i64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DataBreakpointInfoArguments {
    pub variables_reference: Option<i64>,
    pub name: String,
    pub frame_id: Option<i64>,
    pub bytes: Option<i64>,
    pub as_address: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DataBreakpoint {
    pub data_id: String,
    pub access_type: Option<String>,
    /// One of the adapter's `breakpointModes`.
    pub mode: Option<String>,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SetDataBreakpointsArguments {
    pub breakpoints: Vec<DataBreakpoint>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WriteMemoryArguments {
    pub memory_reference: String,
    pub offset: Option<i64>,
    pub allow_partial: Option<bool>,
    pub data: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SetVariableArguments {
    pub variables_reference: i64,
    pub name: String,
    pub value: String,
    pub format: Option<ValueFormat>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SetExpressionArguments {
    pub expression: String,
    pub value: String,
    pub frame_id: Option<i64>,
    pub format: Option<ValueFormat>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ReadMemoryArguments {
    pub memory_reference: String,
    pub offset: Option<i64>,
    pub count: i64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DisassembleArguments {
    pub memory_reference: String,
    pub offset: Option<i64>,
    pub instruction_offset: Option<i64>,
    pub instruction_count: i64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModulesArguments {
    pub start_module: Option<i64>,
    pub module_count: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CompletionsArguments {
    pub frame_id: Option<i64>,
    pub text: String,
    pub column: i64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StackFrameFormat {
    pub parameters: Option<bool>,
    pub parameter_types: Option<bool>,
    pub parameter_names: Option<bool>,
    pub parameter_values: Option<bool>,
    pub line: Option<bool>,
    pub module: Option<bool>,
    pub hex: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExceptionFilterOptions {
    pub filter_id: String,
    pub condition: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SetExceptionBreakpointsArguments {
    pub filters: Vec<String>,
    pub filter_options: Option<Vec<ExceptionFilterOptions>>,
}

/// Arguments naming one thread, as run control requests do.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ThreadArguments {
    pub thread_id: i64,
    pub single_thread: Option<bool>,
    pub granularity: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StackTraceArguments {
    pub thread_id: i64,
    pub start_frame: Option<i64>,
    pub levels: Option<i64>,
    pub format: Option<StackFrameFormat>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ScopesArguments {
    pub frame_id: i64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ValueFormat {
    pub hex: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VariablesArguments {
    pub variables_reference: i64,
    pub filter: Option<String>,
    pub start: Option<i64>,
    pub count: Option<i64>,
    pub format: Option<ValueFormat>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EvaluateArguments {
    pub expression: String,
    pub frame_id: Option<i64>,
    pub context: Option<String>,
    pub format: Option<ValueFormat>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DisconnectArguments {
    pub restart: Option<bool>,
    pub terminate_debuggee: Option<bool>,
}

/// `uscope/setValueFormat` arguments: how values are shown when a request
/// does not say.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct SetValueFormatArguments {
    pub hex: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LocationsArguments {
    pub location_reference: i64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExceptionInfoArguments {
    pub thread_id: i64,
}

/// Reads a memory reference: an address in hexadecimal with `0x`, or in
/// decimal, as some clients send.
pub fn address(reference: &str) -> Option<u64> {
    let reference = reference.trim();
    reference
        .strip_prefix("0x")
        .or_else(|| reference.strip_prefix("0X"))
        .map_or_else(
            || reference.parse().ok(),
            |digits| u64::from_str_radix(digits, 16).ok(),
        )
}

/// Offsets an address by a signed byte count, if the result is one.
pub fn offset(address: u64, offset: Option<i64>) -> Option<u64> {
    address.checked_add_signed(offset.unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_tolerate_string_sequences_and_missing_or_null_arguments() {
        for (text, seq) in [
            (
                r#"{"seq":7,"type":"request","command":"threads"}"#,
                json!(7),
            ),
            (
                r#"{"seq":"7","type":"request","command":"threads","arguments":null}"#,
                json!("7"),
            ),
        ] {
            let Incoming::Request {
                seq: parsed,
                command,
                arguments,
            } = parse(text.as_bytes()).expect("request")
            else {
                panic!("not a request");
            };
            assert_eq!(
                (parsed, command.as_str(), arguments),
                (seq, "threads", json!({}))
            );
        }
        assert!(matches!(
            parse(br#"{"seq":1,"type":"event","event":"x"}"#),
            Err(MessageError::UnsupportedType(kind)) if kind == "event"
        ));
        assert!(matches!(
            parse(br#"{"seq":1,"type":"request"}"#),
            Err(MessageError::MissingCommand)
        ));
        assert!(matches!(parse(b"[1]"), Err(MessageError::NotAnObject)));
        assert!(matches!(parse(b"{"), Err(MessageError::Json(_))));
        assert_eq!(
            MessageError::request_seq(br#"{"seq":3,"command":"x"}"#),
            Some((json!(3), "x".to_owned()))
        );
    }

    #[test]
    fn errors_carry_a_body_and_escape_braces_in_their_format() {
        let response = Outgoing::Response {
            request_seq: json!(2),
            command: "evaluate".to_owned(),
            result: Err(ErrorBody::new("no member {x}")),
        }
        .to_json(9);
        assert_eq!(
            response,
            json!({
                "seq": 9, "type": "response", "request_seq": 2, "success": false,
                "command": "evaluate", "message": "no member {x}",
                "body": {"error": {"id": 1, "format": "no member {{x}}", "showUser": false}},
            })
        );
        let event = Outgoing::Event {
            event: "initialized",
            body: Value::Null,
        }
        .to_json(1);
        assert_eq!(event["body"], json!({}));
    }
}
