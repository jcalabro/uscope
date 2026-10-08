//! Tasks a test steps through, across awaits. Three tasks run the same
//! async functions. Each waits at its gate, which a plain thread opens
//! only once every task waits there, so that an await a test steps over
//! is pending, and the other tasks run the same code meanwhile. Then each
//! goes round a loop of awaits that yield, and finishes.
//!
//! With no argument the tasks run on a multi-thread runtime's two
//! workers; with `current`, on a current-thread runtime.

use std::hint::black_box;

use tokio::sync::oneshot;

const TASKS: usize = 3;

async fn inner(me: u64, gate: oneshot::Receiver<u64>) -> u64 {
    let before = black_box(me * 10); // STEP: inner
    let at = truth::at(me, "gate");
    let opened = gate.await.unwrap_or(0); // AWAIT: inner
    drop(at); // STEP: inner-after
    before + opened
}

async fn outer(me: u64, gate: oneshot::Receiver<u64>) -> u64 {
    let start = black_box(me + 1); // STEP: outer
    let got = inner(me, gate).await; // AWAIT: outer
    start + got // STEP: outer-after
}

async fn rounds(me: u64) -> u64 {
    let mut total = 0;
    for round in 0..3 {
        total += round; // STEP: round
        tokio::task::yield_now().await; // AWAIT: round
    }
    total + me // STEP: rounds-after
}

async fn task(gate: oneshot::Receiver<u64>) -> u64 {
    let me = truth::start();
    let got = outer(me, gate).await; // STEP: task
    let more = rounds(me).await; // AWAIT: rounds
    truth::end(me);
    got + more // STEP: task-last
}

fn main() {
    let runtime = match std::env::args().nth(1).as_deref() {
        None => tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .build(),
        Some("current") => tokio::runtime::Builder::new_current_thread().build(),
        Some(other) => panic!("unknown mode {other}"),
    }
    .expect("a runtime");
    let mut gates = Vec::new();
    let mut handles = Vec::new();
    for _ in 0..TASKS {
        let (open, gate) = oneshot::channel();
        gates.push(open);
        handles.push(runtime.spawn(task(gate)));
    }
    let opener = std::thread::spawn(move || {
        while truth::parked_at("gate") != TASKS {
            std::thread::yield_now();
        }
        for (value, open) in (0..).zip(gates) {
            open.send(value).expect("the task waits");
        }
    });
    let sum = runtime.block_on(async {
        let mut sum = 0;
        for handle in handles {
            sum += handle.await.expect("the task ends");
        }
        sum
    });
    opener.join().expect("the gates open");
    truth::line(&[&"sum", &sum]);
}
