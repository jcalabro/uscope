#![no_main]

use libfuzzer_sys::fuzz_target;
use serde_json::Value;

#[allow(dead_code, reason = "the target exercises decoding only")]
#[path = "../../src/dap/protocol.rs"]
mod protocol;

#[allow(dead_code, reason = "the target exercises the parser only")]
#[path = "../../src/dap/complete.rs"]
mod complete;

/// Decodes request arguments as each request the adapter serves would.
fn decode(arguments: &Value) {
    macro_rules! decode_as {
        ($($kind:ty),*) => {
            $(let _ = serde_json::from_value::<$kind>(arguments.clone());)*
        };
    }
    decode_as!(
        protocol::InitializeArguments,
        protocol::SetBreakpointsArguments,
        protocol::SetFunctionBreakpointsArguments,
        protocol::SetInstructionBreakpointsArguments,
        protocol::SetExceptionBreakpointsArguments,
        protocol::BreakpointLocationsArguments,
        protocol::DataBreakpointInfoArguments,
        protocol::SetDataBreakpointsArguments,
        protocol::ThreadArguments,
        protocol::StackTraceArguments,
        protocol::ScopesArguments,
        protocol::VariablesArguments,
        protocol::EvaluateArguments,
        protocol::DisconnectArguments,
        protocol::ReadMemoryArguments,
        protocol::DisassembleArguments,
        protocol::ModulesArguments,
        protocol::CompletionsArguments,
        protocol::ExceptionInfoArguments,
        protocol::SetVariableArguments,
        protocol::SetExpressionArguments,
        protocol::WriteMemoryArguments,
        protocol::LocationsArguments,
        protocol::SetValueFormatArguments
    );
}

fuzz_target!(|data: &[u8]| {
    match protocol::parse(data) {
        Ok(protocol::Incoming::Request {
            seq,
            command,
            arguments,
        }) => {
            decode(&arguments);
            let response = protocol::Outgoing::Response {
                request_seq: seq,
                command,
                result: Err(protocol::ErrorBody::new(String::from_utf8_lossy(data))),
            }
            .to_json(1);
            assert!(response["body"].is_object());
        }
        Ok(protocol::Incoming::Response { .. }) => {}
        Err(_) => {
            let _ = protocol::MessageError::request_seq(data);
        }
    }
    if let Some(text) = std::str::from_utf8(data).ok() {
        let _ = protocol::address(text);
        // What a completion completes is always the end of the text.
        let (_, partial, start) = complete::completing(text);
        assert_eq!(&text[start..], partial);
    }
});
