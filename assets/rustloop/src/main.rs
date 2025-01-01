use std::process;
use std::{thread, time};

fn main() {
    let second = time::Duration::from_millis(1000);
    let pid = process::id();

    let mut ndx = 0;
    loop {
        println!("rust looping (pid: {}): {}", pid, ndx);

        ndx += 1;
        thread::sleep(second);
    }
}
