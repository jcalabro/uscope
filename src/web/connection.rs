//! One tab's WebSocket.
//!
//! A connection sends `hello`, the current state, recent output, then
//! everything as it happens. Each request runs in a task of its own, so a
//! slow one, such as loading a large program, never holds up the state,
//! output, or other answers the tab is waiting for.

use std::sync::Arc;

use axum::extract::ws::{Message, Utf8Bytes, WebSocket};
use tokio::sync::{broadcast, mpsc};

use super::protocol::{self, Envelope, ErrorBody, ErrorKind, Hello, Role, ServerMessage};
use super::session::Session;

/// Answers waiting to be sent; a tab that stops reading slows only its own
/// requests.
const PENDING_ANSWERS: usize = 64;

pub async fn serve(mut socket: WebSocket, session: Arc<Session>, role: Role) {
    let mut joined = session.join(role);
    let connection = joined.connection;
    let hello = ServerMessage::Hello(Hello {
        version: protocol::VERSION,
        connection,
        role,
        name: joined.name.clone(),
        cwd: session.cwd().display().to_string(),
    });
    let state = ServerMessage::State((**joined.state.borrow_and_update()).clone());
    let mut opening = vec![encode(&hello), encode(&state)];
    opening.extend(joined.history.iter().map(|text| Utf8Bytes::from(&**text)));
    for text in opening {
        if socket.send(Message::Text(text)).await.is_err() {
            session.leave(connection);
            return;
        }
    }

    let (answers, mut pending) = mpsc::channel::<Utf8Bytes>(PENDING_ANSWERS);
    loop {
        // Biased, in this order: requests are read even during a flood of
        // output, and a request's state change goes out before its answer.
        let outgoing = tokio::select! {
            biased;
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    dispatch(&session, connection, role, text.as_str(), answers.clone());
                    continue;
                }
                Some(Ok(Message::Binary(_))) => encode(&error(0, ErrorKind::Invalid, "messages are JSON text")),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
            },
            changed = joined.state.changed() => {
                if changed.is_err() {
                    break;
                }
                let state = (**joined.state.borrow_and_update()).clone();
                encode(&ServerMessage::State(state))
            }
            Some(answer) = pending.recv() => answer,
            message = joined.messages.recv() => match message {
                Ok(text) => Utf8Bytes::from(&*text),
                // Output was dropped; the state, which matters, is sent whole.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
        };
        if socket.send(Message::Text(outgoing)).await.is_err() {
            break;
        }
    }
    session.leave(connection);
}

fn dispatch(
    session: &Arc<Session>,
    connection: u32,
    role: Role,
    text: &str,
    answers: mpsc::Sender<Utf8Bytes>,
) {
    let envelope = match serde_json::from_str::<Envelope>(text) {
        Ok(envelope) => envelope,
        Err(problem) => {
            // Answer with the id when the request names one.
            let id = serde_json::from_str::<serde_json::Value>(text)
                .ok()
                .and_then(|value| value.get("id")?.as_u64())
                .unwrap_or(0);
            let answer = encode(&error(id, ErrorKind::Invalid, &problem.to_string()));
            tokio::spawn(async move { answers.send(answer).await });
            return;
        }
    };
    let session = Arc::clone(session);
    tokio::spawn(async move {
        let id = envelope.id;
        let answer = match session.handle(connection, role, envelope.request).await {
            Ok(result) => ServerMessage::Result { id, result },
            Err(failure) => ServerMessage::Error {
                id,
                error: failure.body(),
            },
        };
        let _ = answers.send(encode(&answer)).await;
    });
}

fn error(id: u64, kind: ErrorKind, message: &str) -> ServerMessage {
    ServerMessage::Error {
        id,
        error: ErrorBody {
            kind,
            message: message.to_owned(),
        },
    }
}

fn encode(message: &ServerMessage) -> Utf8Bytes {
    serde_json::to_string(message)
        .expect("messages serialize")
        .into()
}
