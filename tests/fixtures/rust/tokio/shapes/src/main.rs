//! The shapes a future a task awaits takes: an async function, an async
//! block, a boxed trait object, a generic async function, a trait's async
//! method, and tokio's own future, a channel's receive. A test steps into
//! each await from its line, marked `INTO`, to the line the future's code
//! begins on, marked `STEP`. Each await is alone on its line, so that
//! nothing else there is a call to step into.

use std::future::Future;
use std::hint::black_box;
use std::pin::Pin;

async fn plain(value: u64) -> u64 {
    black_box(value + 1) // STEP: plain
}

async fn generic<T: Into<u64>>(value: T) -> u64 {
    black_box(value.into() + 4) // STEP: generic
}

trait Shape {
    async fn method(&self, value: u64) -> u64;
}

struct Square;

impl Shape for Square {
    async fn method(&self, value: u64) -> u64 {
        black_box(value + 5) // STEP: method
    }
}

async fn shapes(me: u64) -> u64 {
    let a = plain(me).await; // INTO: plain
    let block = async move {
        black_box(me + 2) // STEP: block
    };
    let b = block.await; // INTO: block
    let boxed: Pin<Box<dyn Future<Output = u64> + Send>> = Box::pin(async move {
        black_box(me + 3) // STEP: boxed
    });
    let c = boxed.await; // INTO: boxed
    let small = u32::try_from(me).unwrap_or(0);
    let d = generic(small).await; // INTO: generic
    let e = Square.method(me).await; // INTO: method
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    sender.send(me).await.unwrap_or(());
    let f = receiver.recv().await; // INTO: recv
    a + b + c + d + e + f.unwrap_or(0) // STEP: recv-after
}

fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a runtime");
    let total = runtime
        .block_on(runtime.spawn(shapes(1)))
        .expect("the task ends");
    truth::line(&[&"total", &total]);
}
