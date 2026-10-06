//! Following the processes a program forks into child sessions.
//!
//! The debugger holds each child stopped, untraced, and clean of the
//! parent's breakpoints. The session asks the client to start a session
//! that attaches to it with `startDebugging`, and the child session ends
//! the hold. A child no session takes is released to run on its own.

use std::time::Duration;

use serde_json::{Map, Value, json};
use uscope::HeldChild;

use super::session::Client;

/// How long the client may take to answer `startDebugging`. VS Code answers
/// once the child session has attached, which includes loading the
/// program's debug information.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a session the client started, as nvim-dap answers as soon as
/// it starts one, may take to attach.
const ADOPTION_TIMEOUT: Duration = Duration::from_secs(60);

/// How often a child a session may still attach to is checked.
const ADOPTION_POLL: Duration = Duration::from_millis(50);

/// `USCOPE_ADOPTION_TIMEOUT`, in milliseconds, lowers [`ADOPTION_TIMEOUT`]
/// so tests can see a child no session takes.
fn adoption_timeout() -> Duration {
    std::env::var("USCOPE_ADOPTION_TIMEOUT")
        .ok()
        .and_then(|timeout| timeout.parse().ok())
        .map(Duration::from_millis)
        .filter(|timeout| *timeout < ADOPTION_TIMEOUT)
        .unwrap_or(ADOPTION_TIMEOUT)
}

/// The `startDebugging` arguments that start a session attached to `child`,
/// with the settings the parent's configuration passes on.
pub(super) fn start_arguments(
    inherited: &Map<String, Value>,
    program: &str,
    child: &HeldChild,
) -> Value {
    let held = child.process();
    let mut configuration = inherited.clone();
    configuration.insert(
        "name".to_owned(),
        format!("{program} (fork {})", held.process_id).into(),
    );
    configuration.insert("pid".to_owned(), held.process_id.get().into());
    configuration.insert("held".to_owned(), json!({"startTime": held.start_time}));
    json!({"request": "attach", "configuration": configuration})
}

/// Asks the client to debug `child` in a session of its own, and releases
/// the child if none takes it.
pub(super) async fn follow(client: Client, arguments: Value, child: HeldChild) {
    let process = child.process().process_id;
    let answer = tokio::time::timeout(ANSWER_TIMEOUT, client.request("startDebugging", arguments));
    let reason = match answer.await {
        // A session started, and may still be attaching; a client that
        // closed the connection can no longer say whether one will.
        Ok(Ok(Ok(_)) | Err(_)) => {
            await_adoption(&child).await;
            "no session attached to it".to_owned()
        }
        Ok(Ok(Err(message))) => format!("the client did not debug it: {message}"),
        Err(_) => format!(
            "the client did not start a session for it within {} seconds",
            ANSWER_TIMEOUT.as_secs()
        ),
    };
    // A child a session attached to meanwhile is left alone.
    if child.release().unwrap_or(false) {
        let _ = client
            .important(format!("process {process} runs on its own: {reason}"))
            .await;
    }
}

/// Waits while a session may still attach to `child`.
async fn await_adoption(child: &HeldChild) {
    let held = child.process();
    let deadline = tokio::time::Instant::now() + adoption_timeout();
    while tokio::time::Instant::now() < deadline && uscope::still_held(&held).unwrap_or(false) {
        tokio::time::sleep(ADOPTION_POLL).await;
    }
}
