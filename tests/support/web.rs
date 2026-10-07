//! Drives the real `uscope web` as browsers do: a join link's token traded
//! for a cookie at `/api/login`, then WebSocket clients sending the page's
//! requests.
//!
//! Every message is kept in a transcript printed when a test fails. With
//! `USCOPE_WEB_TRANSCRIPTS=DIR`, each test also writes its traffic to
//! `DIR/<test>.jsonl`, which the page's tests replay.

#![allow(dead_code, reason = "each test uses a subset of the harness")]

use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::net::TcpStream as AsyncTcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::support::flight_recordings;

/// Bounds every wait for the server.
pub const DEADLINE: Duration = Duration::from_secs(10);

type Transcript = Arc<Mutex<Vec<String>>>;

/// A running `uscope web`, stopped when dropped.
pub struct Web {
    name: String,
    child: Option<Child>,
    pub address: SocketAddr,
    pub control_token: String,
    transcript: Transcript,
    recording: Option<PathBuf>,
    started: Instant,
}

impl Web {
    /// Starts `uscope web` with `arguments` on a free port.
    pub fn start(name: &str, arguments: &[&str]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_uscope"));
        command
            .args(["web", "--port", "0"])
            // Names in presence and notices, and so in transcripts.
            .env("USER", "tester")
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let recording = flight_recordings::watch_adapter(&mut command);
        let mut child = command.spawn().expect("start uscope web");
        let started = Instant::now();
        let transcript = Transcript::default();
        let stderr = child.stderr.take().expect("stderr");
        let errors = Arc::clone(&transcript);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                errors
                    .lock()
                    .expect("transcript")
                    .push(format!("!! server stderr: {line}"));
            }
        });
        let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut link = None;
        let mut printed = String::new();
        while link.is_none() {
            printed.clear();
            assert!(
                stdout
                    .read_line(&mut printed)
                    .expect("read the server's output")
                    > 0,
                "uscope web exited before printing its link"
            );
            link = printed.trim().strip_prefix("open ").map(str::to_owned);
        }
        // Keep draining stdout so the server never blocks writing to it.
        std::thread::spawn(move || {
            let mut rest = Vec::new();
            let _ = stdout.read_to_end(&mut rest);
        });
        let link = link.expect("a link");
        let rest = link.strip_prefix("http://").expect("an http link");
        let (address, token) = rest.split_once("/join#").expect("a join link");
        Self {
            name: name.to_owned(),
            child: Some(child),
            address: address.parse().expect("a socket address"),
            control_token: token.to_owned(),
            transcript,
            recording,
            started,
        }
    }

    pub fn origin(&self) -> String {
        format!("http://{}", self.address)
    }

    /// Posts to `/api/login` with these headers, returning the status and
    /// the `Set-Cookie` value.
    pub fn post_login(
        &self,
        token: &str,
        origin: Option<&str>,
        host: Option<&str>,
    ) -> (u16, Option<String>) {
        let mut request = format!(
            "POST /api/login HTTP/1.1\r\nConnection: close\r\nContent-Length: {}\r\n",
            token.len()
        );
        if let Some(host) = host {
            let _ = write!(request, "Host: {host}\r\n");
        }
        if let Some(origin) = origin {
            let _ = write!(request, "Origin: {origin}\r\n");
        }
        request.push_str("\r\n");
        request.push_str(token);
        let response = self.http(&request);
        let status = response
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("a status line");
        let cookie = response.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("set-cookie").then(|| {
                value
                    .trim()
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
        });
        (status, cookie)
    }

    /// Sends raw HTTP and returns the whole response.
    pub fn http(&self, request: &str) -> String {
        let mut stream = TcpStream::connect(self.address).expect("connect");
        stream
            .set_read_timeout(Some(DEADLINE))
            .expect("read timeout");
        stream.write_all(request.as_bytes()).expect("send");
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
    }

    /// Logs in with `token` as this server's own page would.
    pub fn cookie(&self, token: &str) -> String {
        let host = self.address.to_string();
        let (status, cookie) = self.post_login(token, Some(&self.origin()), Some(&host));
        assert_eq!(status, 204, "login with {token}");
        cookie.expect("a cookie")
    }

    /// A control client, logged in.
    pub async fn control(&self, name: &str) -> Client {
        let cookie = self.cookie(&self.control_token.clone());
        self.connect(name, Some(&cookie), Some(&self.origin()), None)
            .await
            .expect("connect")
    }

    /// A client with a token another client's `share` made.
    pub async fn joining(&self, name: &str, link: &str) -> Client {
        let token = link.rsplit_once('#').expect("a token in the link").1;
        let cookie = self.cookie(token);
        self.connect(name, Some(&cookie), Some(&self.origin()), None)
            .await
            .expect("connect")
    }

    /// Opens a WebSocket with these headers, or returns the HTTP status
    /// that refused it.
    pub async fn connect(
        &self,
        name: &str,
        cookie: Option<&str>,
        origin: Option<&str>,
        host: Option<&str>,
    ) -> Result<Client, u16> {
        let mut request = format!("ws://{}/api/ws", self.address)
            .into_client_request()
            .expect("a request");
        let headers = request.headers_mut();
        if let Some(cookie) = cookie {
            headers.insert("Cookie", cookie.parse().expect("header"));
        }
        if let Some(origin) = origin {
            headers.insert("Origin", origin.parse().expect("header"));
        }
        if let Some(host) = host {
            headers.insert("Host", host.parse().expect("header"));
        }
        match timeout(DEADLINE, tokio_tungstenite::connect_async(request)).await {
            Ok(Ok((socket, _))) => Ok(Client {
                name: name.to_owned(),
                socket,
                next_id: 1,
                pending: Vec::new(),
                transcript: Arc::clone(&self.transcript),
                traffic: Vec::new(),
                started: self.started,
                state: None,
            }),
            Ok(Err(tokio_tungstenite::tungstenite::Error::Http(response))) => {
                Err(response.status().as_u16())
            }
            Ok(Err(error)) => panic!("connect: {error}"),
            Err(elapsed) => panic!("connecting timed out: {elapsed}"),
        }
    }

    /// Stops the server as a terminal's Ctrl-C would and waits for it.
    pub fn interrupt(&mut self) -> std::process::ExitStatus {
        let mut child = self.child.take().expect("a running server");
        let pid = nix::unistd::Pid::from_raw(i32::try_from(child.id()).expect("a pid"));
        let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT);
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Some(status) = child.try_wait().expect("wait") {
                return status;
            }
            assert!(Instant::now() < deadline, "uscope web did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn print_transcript(&self) {
        eprintln!("--- {} transcript ---", self.name);
        for line in self.transcript.lock().expect("transcript").iter() {
            eprintln!("{line}");
        }
    }
}

impl Drop for Web {
    fn drop(&mut self) {
        let failed = std::thread::panicking();
        if self.child.is_some() {
            self.interrupt();
        }
        if failed {
            self.print_transcript();
        } else if let Some(recording) = &self.recording {
            flight_recordings::adapter_finished(recording);
        }
    }
}

/// One tab's connection.
pub struct Client {
    name: String,
    socket: WebSocketStream<MaybeTlsStream<AsyncTcpStream>>,
    next_id: u64,
    /// Messages received while waiting for something else.
    pending: Vec<Value>,
    transcript: Transcript,
    /// Every message, for the page's replay tests.
    traffic: Vec<Value>,
    started: Instant,
    /// The newest `state` received.
    state: Option<Value>,
}

impl Client {
    fn log(&self, direction: &str, text: &str) {
        // Bursts of output would bury the rest.
        let text = match text.char_indices().nth(400) {
            Some((end, _)) => format!("{}… ({} bytes)", &text[..end], text.len()),
            None => text.to_owned(),
        };
        self.transcript.lock().expect("transcript").push(format!(
            "[{:8.3}] {} {direction} {text}",
            self.started.elapsed().as_secs_f64(),
            self.name
        ));
    }

    pub async fn send_raw(&mut self, text: &str) {
        self.log(">>", text);
        if let Ok(value) = serde_json::from_str::<Value>(text) {
            self.traffic.push(json!({"to": "server", "message": value}));
        }
        self.socket.send(Message::text(text)).await.expect("send");
    }

    /// Sends a request without waiting, returning its id.
    pub async fn send(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let mut message = json!({"id": id, "method": method});
        if !params.is_null() {
            message["params"] = params;
        }
        self.send_raw(&message.to_string()).await;
        id
    }

    /// The next message, from those received earlier first.
    pub async fn next(&mut self) -> Value {
        if !self.pending.is_empty() {
            return self.pending.remove(0);
        }
        self.receive().await
    }

    async fn receive(&mut self) -> Value {
        loop {
            let message = timeout(DEADLINE, self.socket.next())
                .await
                .unwrap_or_else(|_| panic!("{} waited too long for a message", self.name))
                .unwrap_or_else(|| panic!("{} was disconnected", self.name))
                .expect("a message");
            let Message::Text(text) = message else {
                continue;
            };
            self.log("<<", &text);
            let value: Value = serde_json::from_str(&text).expect("JSON");
            self.traffic.push(json!({"to": "page", "message": value}));
            if value["type"] == "state" {
                self.state = Some(value.clone());
            }
            return value;
        }
    }

    /// Waits for the first message `matches` accepts, keeping the rest.
    pub async fn expect(&mut self, what: &str, matches: impl Fn(&Value) -> bool) -> Value {
        if let Some(index) = self.pending.iter().position(&matches) {
            return self.pending.remove(index);
        }
        let deadline = Instant::now() + DEADLINE;
        loop {
            assert!(Instant::now() < deadline, "{} never saw {what}", self.name);
            let message = self.receive().await;
            if matches(&message) {
                return message;
            }
            self.pending.push(message);
        }
    }

    /// Sends a request and waits for its answer.
    pub async fn request(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<Value, (String, String)> {
        let id = self.send(method, params).await;
        let answer = self
            .expect(&format!("the answer to {method}"), |message| {
                message["id"] == id && matches!(message["type"].as_str(), Some("result" | "error"))
            })
            .await;
        if answer["type"] == "result" {
            Ok(answer["result"].clone())
        } else {
            Err((
                answer["error"]["kind"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                answer["error"]["message"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            ))
        }
    }

    /// Sends a request that must succeed.
    pub async fn ok(&mut self, method: &str, params: Value) -> Value {
        self.request(method, params)
            .await
            .unwrap_or_else(|error| panic!("{method} failed: {error:?}"))
    }

    /// The newest state received so far, without waiting.
    pub const fn latest_state(&self) -> Option<&Value> {
        self.state.as_ref()
    }

    /// Waits until the newest state satisfies `condition`, returning it.
    pub async fn state(&mut self, what: &str, condition: impl Fn(&Value) -> bool) -> Value {
        if let Some(state) = &self.state
            && condition(state)
            && !self
                .pending
                .iter()
                .any(|message| message["type"] == "state")
        {
            return state.clone();
        }
        let deadline = Instant::now() + DEADLINE;
        loop {
            assert!(
                Instant::now() < deadline,
                "{} never reached {what}",
                self.name
            );
            // A newer state supersedes queued ones.
            if let Some(index) = self
                .pending
                .iter()
                .rposition(|message| message["type"] == "state")
            {
                let state = self.pending.remove(index);
                self.pending.retain(|message| message["type"] != "state");
                if condition(&state) {
                    return state;
                }
                continue;
            }
            let message = self.receive().await;
            if message["type"] != "state" {
                self.pending.push(message);
            } else if condition(&message) {
                return message;
            }
        }
    }

    /// Waits until the program is in `inferior` state, such as `stopped`.
    pub async fn inferior(&mut self, inferior: &str) -> Value {
        self.state(&format!("an inferior {inferior}"), |state| {
            state["inferior"]["state"] == inferior
        })
        .await
    }

    /// Everything printed on `stream` so far, waiting until `ends` is seen.
    pub async fn output_until(&mut self, stream: &str, ends: &str) -> String {
        let mut text = String::new();
        let deadline = Instant::now() + DEADLINE;
        let mut index = 0;
        loop {
            while index < self.pending.len() {
                let message = &self.pending[index];
                if message["type"] == "output" && message["stream"] == stream {
                    text.push_str(message["text"].as_str().unwrap_or_default());
                    self.pending.remove(index);
                } else {
                    index += 1;
                }
            }
            if text.contains(ends) {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "{} never printed {ends:?}",
                self.name
            );
            let message = self.receive().await;
            self.pending.push(message);
        }
    }

    /// Writes this client's traffic for the page's replay tests when
    /// `USCOPE_WEB_TRANSCRIPTS` names a directory.
    pub fn save_traffic(&self, name: &str) {
        let Some(directory) = std::env::var_os("USCOPE_WEB_TRANSCRIPTS") else {
            return;
        };
        let root = env!("CARGO_MANIFEST_DIR");
        let text = self
            .traffic
            .iter()
            .map(|line| {
                // Keep a burst of output from bloating the checked-in file.
                let mut line = line.clone();
                if let Some(output) = line["message"]["text"].as_str()
                    && output.len() > 200
                {
                    let mut end = 120;
                    while !output.is_char_boundary(end) {
                        end -= 1;
                    }
                    line["message"]["text"] = Value::from(format!("{}…", &output[..end]));
                }
                line.to_string().replace(root, "<root>")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let path = PathBuf::from(directory).join(format!("{name}.jsonl"));
        std::fs::write(&path, text + "\n").expect("write the transcript");
    }
}

/// Whether a process still exists, as a zombie or alive.
pub fn exists(pid: u64) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// Waits until `pid` is gone, failing at the deadline.
pub fn wait_gone(pid: u64) {
    let deadline = Instant::now() + DEADLINE;
    while exists(pid) {
        assert!(
            Instant::now() < deadline,
            "process {pid} outlived the server"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
