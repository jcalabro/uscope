//! A blocking pool of one thread: one `spawn_blocking` closure runs,
//! waiting on a channel, and two more wait in the pool's queue behind it.
//!
//! Once the running closure has reported itself and its thread sleeps,
//! the program reports its truth, with the queued closures' task ids, and
//! stops at the checkpoint `blocking`. Then it releases the running
//! closure, and each closure in turn runs to its end.

use std::sync::mpsc as channel;

/// A closure that reports itself, then waits for `release`.
fn blocker(release: channel::Receiver<()>, started: channel::Sender<i32>) -> u64 {
    let me = truth::start();
    let _at = truth::at(me, "release");
    started.send(truth::gettid()).expect("the program waits");
    release.recv().expect("the program releases it"); // BLOCKER: waits
    truth::here(me); // BLOCKER: released
    me
}

/// A closure that waits behind the blocker.
fn queued() -> u64 {
    let me = truth::start(); // QUEUED: runs
    truth::end(me);
    me
}

fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .build()
        .expect("a runtime");
    let (release, released) = channel::channel();
    let (started, pool_thread) = channel::channel();
    let running = runtime.spawn_blocking(move || blocker(released, started));
    let pool_thread = pool_thread.recv().expect("the blocker starts");
    let waiting = [runtime.spawn_blocking(queued), runtime.spawn_blocking(queued)];
    while !truth::thread_parked(pool_thread) {
        std::thread::yield_now();
    }
    for handle in &waiting {
        truth::line(&[&"queued", &handle.id()]);
    }
    truth::line(&[&"pool", &pool_thread]);
    // The runtime is found through a thread in it, which this one is not;
    // the pool's thread entered it to run the closure.
    truth::checkpoint("blocking", None);
    release.send(()).expect("the blocker waits");
    let ended = runtime.block_on(async move {
        let mut ended = vec![running.await.expect("the blocker ends")];
        for handle in waiting {
            ended.push(handle.await.expect("the closure ends"));
        }
        ended
    });
    truth::line(&[&"ended", &format!("{ended:?}")]);
}
