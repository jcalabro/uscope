//! A program instrumented with uscope's probes SDK. Each mode waits until
//! uscope listens, does its work, and exits.
//!
//! - `zones`: three rounds of nested zones, each in a label, with a plot,
//!   a message, and a frame, while a worker thread runs zones of its own,
//!   then a trigger.
//! - `burst COUNT`: COUNT zones as fast as it can write them.
//! - `churn COUNT`: COUNT threads, one after another, each one zone.
//! - `fork`: a zone around a forked child, whose own zone records nothing.
//! - `quiet`: says it is ready to be attached to, then zones until the
//!   sites switch themselves off.

use std::io::Write as _;
use std::time::{Duration, Instant};

use uscope_probes::{frame, label, message, plot, trigger, zone};

fn main() {
    let arguments: Vec<String> = std::env::args().collect();
    let count = || arguments[2].parse::<u64>().expect("a count");
    if arguments[1] == "quiet" {
        println!("READY");
    }
    wait_until_listening();
    match arguments[1].as_str() {
        "zones" => zones(),
        "burst" => burst(count()),
        "churn" => churn(count()),
        "fork" => forked(),
        "quiet" => quiet(),
        mode => panic!("unknown mode {mode}"),
    }
}

/// Waits until uscope has switched the sites on.
fn wait_until_listening() {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !uscope_probes::listening() {
        assert!(Instant::now() < deadline, "uscope never listened");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn zones() {
    let worker = std::thread::Builder::new()
        .name("worker".into())
        .spawn(|| {
            let _zone = zone!("work");
            for _ in 0..2 {
                let _step = zone!("step");
                std::hint::black_box(spin(1000));
            }
        })
        .expect("spawn");
    for round in 0..3_u32 {
        let _round = label!("round", round);
        let _outer = zone!("outer");
        {
            let _a = zone!("a");
            std::hint::black_box(spin(100));
        }
        {
            let _b = zone!("b");
            let _leaf = zone!("leaf");
            plot!("round", f64::from(round));
            message!("round {round} of {}", 3);
        }
        frame!();
    }
    worker.join().expect("join");
    trigger!("done");
}

fn burst(count: u64) {
    for _ in 0..count {
        let _tick = zone!("tick");
    }
}

fn churn(count: u64) {
    for _ in 0..count {
        std::thread::spawn(|| {
            let _life = zone!("life");
        })
        .join()
        .expect("join");
    }
}

unsafe extern "C" {
    fn fork() -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn _exit(status: i32) -> !;
}

fn forked() {
    let _parent = zone!("parent");
    // SAFETY: the child only writes and exits.
    let child = unsafe { fork() };
    if child == 0 {
        {
            let _child = zone!("child");
        }
        let line = format!("child listening: {}\n", uscope_probes::listening());
        let _ = std::io::stdout().write_all(line.as_bytes());
        let _ = std::io::stdout().flush();
        // SAFETY: the child ends here, without its parent's exit handlers.
        unsafe { _exit(0) };
    }
    let mut status = 0;
    // SAFETY: the child is this process's.
    unsafe { waitpid(child, &raw mut status, 0) };
}

fn quiet() {
    println!("listening");
    let deadline = Instant::now() + Duration::from_secs(30);
    while uscope_probes::listening() {
        assert!(Instant::now() < deadline, "the sites never switched off");
        let _spin = zone!("spin");
        std::hint::black_box(spin(10));
    }
    println!("quiet");
}

fn spin(rounds: u64) -> u64 {
    (0..rounds).fold(0, |sum, value| std::hint::black_box(sum ^ value))
}
