//! Eight tasks of one async function, each parked three calls deep at a
//! different kind of await, on a multi-thread runtime or, given `current`,
//! a current-thread one. Once every task has registered its await and
//! nothing runs, the program reports its truth and stops at the
//! checkpoint `parked`; then it releases every task and joins them.
//! Each local a test reads is saved across the await, and each task keeps
//! its own id in `me`.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Barrier, Mutex, Notify, Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;

/// What one task waits for.
enum Wait {
    Channel(mpsc::Receiver<u32>),
    Sleep,
    Lock(Arc<Mutex<u32>>),
    Join(JoinHandle<u32>),
    Notified(Arc<Notify>),
    Oneshot(oneshot::Receiver<u32>),
    Barrier(Arc<Barrier>),
    Permit(Arc<Semaphore>),
}

impl Wait {
    fn tag(&self) -> &'static str {
        match self {
            Self::Channel(_) => "channel",
            Self::Sleep => "sleep",
            Self::Lock(_) => "lock",
            Self::Join(_) => "join",
            Self::Notified(_) => "notify",
            Self::Oneshot(_) => "oneshot",
            Self::Barrier(_) => "barrier",
            Self::Permit(_) => "permit",
        }
    }
}

async fn leaf(me: u64, wait: Wait) -> u32 {
    let leaf_local = black_box(me * 100 + 3);
    truth::value(me, "leaf_local", leaf_local);
    let tag = wait.tag();
    let _at = truth::at(me, tag);
    let got = match wait {
        Wait::Channel(mut receiver) => receiver.recv().await.unwrap_or(0), // AWAIT: channel
        Wait::Sleep => {
            tokio::time::sleep(Duration::from_secs(3600)).await; // AWAIT: sleep
            0
        }
        Wait::Lock(mutex) => *mutex.lock().await, // AWAIT: lock
        Wait::Join(handle) => handle.await.unwrap_or(0), // AWAIT: join
        Wait::Notified(notify) => {
            notify.notified().await; // AWAIT: notify
            4
        }
        Wait::Oneshot(receiver) => receiver.await.unwrap_or(0), // AWAIT: oneshot
        Wait::Barrier(barrier) => {
            barrier.wait().await; // AWAIT: barrier
            6
        }
        Wait::Permit(semaphore) => {
            let _permit = semaphore.acquire().await; // AWAIT: permit
            7
        }
    };
    truth::here(me);
    got + u32::try_from(leaf_local % 7).unwrap_or(0)
}

async fn middle(me: u64, wait: Wait) -> u32 {
    let middle_local = black_box(me * 100 + 2);
    truth::value(me, "middle_local", middle_local);
    let _at = truth::at(me, "middle");
    let got = leaf(me, wait).await; // AWAIT: middle
    got + u32::try_from(middle_local % 5).unwrap_or(0)
}

async fn top(wait: Wait) -> u32 {
    let me = truth::start();
    let top_local = black_box(me * 100 + 1);
    truth::value(me, "top_local", top_local);
    let got = {
        let _at = truth::at(me, "top");
        middle(me, wait).await // AWAIT: top
    };
    truth::end(me);
    got + u32::try_from(top_local % 3).unwrap_or(0)
}

/// Spawns every task, waits until each is parked and nothing runs, and
/// stops at the checkpoint; then releases them and joins them.
async fn run(handle: Option<tokio::runtime::Handle>) {
    let (sender, receiver) = mpsc::channel(1);
    let mutex = Arc::new(Mutex::new(3));
    let guard = mutex.clone().lock_owned().await;
    let notify = Arc::new(Notify::new());
    let (oneshot_sender, oneshot_receiver) = oneshot::channel();
    let barrier = Arc::new(Barrier::new(2));
    let semaphore = Arc::new(Semaphore::new(0));

    let channel = tokio::spawn(top(Wait::Channel(receiver)));
    let mut tasks = vec![
        tokio::spawn(top(Wait::Sleep)),
        tokio::spawn(top(Wait::Lock(mutex))),
        tokio::spawn(top(Wait::Join(channel))),
        tokio::spawn(top(Wait::Notified(notify.clone()))),
        tokio::spawn(top(Wait::Oneshot(oneshot_receiver))),
        tokio::spawn(top(Wait::Barrier(barrier.clone()))),
        tokio::spawn(top(Wait::Permit(semaphore.clone()))),
    ];

    while !truth::all_parked(8) || handle.as_ref().is_some_and(|handle| !truth::workers_parked(handle)) {
        if handle.is_some() {
            std::thread::yield_now();
        } else {
            tokio::task::yield_now().await;
        }
    }
    truth::checkpoint("parked", handle.as_ref());

    // The sleeper waits an hour; the rest are released.
    tasks.remove(0).abort();
    sender.send(1).await.expect("the channel's task waits");
    drop(guard);
    notify.notify_one();
    oneshot_sender.send(5).expect("the oneshot's task waits");
    barrier.wait().await;
    semaphore.add_permits(1);
    for task in tasks {
        task.await.expect("a task ends");
    }
    truth::line(&[&"end"]);
}

fn main() {
    let current = std::env::args().nth(1).as_deref() == Some("current");
    if current {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("a runtime");
        runtime.block_on(run(None));
    } else {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_time()
            .build()
            .expect("a runtime");
        let handle = runtime.handle().clone();
        // The checkpoint waits on this thread, outside the runtime, so that
        // every worker can park.
        std::thread::scope(|scope| {
            scope.spawn(|| runtime.block_on(run(Some(handle))));
        });
    }
}
