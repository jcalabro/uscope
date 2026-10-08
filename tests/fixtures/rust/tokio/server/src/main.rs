//! A line server on a multi-thread runtime, one task per connection: each
//! line a client sends comes back with the count of lines served so far.
//! It prints `READY <address>` once it listens on a port of its own,
//! answers `fd` with the connection's file descriptor, and exits when a
//! client sends `quit`. It is the program a debugger attaches to while
//! clients load it.

use std::hint::black_box;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// The answer to one line.
#[inline(never)]
fn answer(line: &str, served: u64) -> String {
    format!("{line} {served}\n") // SERVED: answer
}

/// Answers a connection's lines until the client closes it.
async fn serve(stream: TcpStream, served: Arc<AtomicU64>) {
    let connection = stream.as_raw_fd();
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line == "quit" {
            std::process::exit(0);
        }
        let reply = if line == "fd" {
            format!("{connection}\n")
        } else {
            let count = black_box(served.fetch_add(1, Ordering::Relaxed) + 1);
            answer(&line, count)
        };
        if writer.write_all(reply.as_bytes()).await.is_err() {
            break;
        }
    }
}

/// Accepts connections, each served by a task of its own.
async fn listen(listener: TcpListener) {
    let served = Arc::new(AtomicU64::new(0));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(serve(stream, Arc::clone(&served)));
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port to listen on");
    let address = listener.local_addr().expect("the listening address");
    println!("READY {address}");
    tokio::spawn(listen(listener)).await.expect("the listener runs");
}
