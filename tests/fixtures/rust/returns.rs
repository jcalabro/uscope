//! What Rust functions return. Before each checkpoint the program prints its
//! own truth, one tab-separated line per value,
//!
//! 	TRUTH	<checkpoint>	<path>	<kind>	<value>
//!
//! and then calls reached(checkpoint). Every checkpoint is named
//! `returned-`: the tests finish the function that reached it and inspect
//! what it returned, which is named for the function. Rust leaves its own
//! calling convention unspecified, so only scalars are known; floats are
//! their bits in hexadecimal.

use std::hint::black_box;
use std::io::Write;

#[inline(never)]
fn reached(checkpoint: &str) {
    black_box(checkpoint);
}

fn truth(checkpoint: &str, path: &str, kind: &str, value: impl std::fmt::Display) {
    println!("TRUTH\t{checkpoint}\t{path}\t{kind}\t{value}");
}

fn reach(_checkpoint: &str) {
    std::io::stdout().flush().expect("flush");
}

#[derive(Clone, Copy, PartialEq, Debug)]
#[allow(dead_code)]
enum Level {
    Low,
    High,
}

#[derive(Clone, Copy)]
struct Pair {
    first: i32,
    second: i32,
}

#[inline(never)]
fn r_int(n: i32) -> i32 {
    let value = n * -9;
    truth("returned-int", "r_int", "int", value);
    reach("returned-int");
    reached("returned-int");
    black_box(value)
}

#[inline(never)]
fn r_bool(n: i32) -> bool {
    let value = n > 0;
    truth("returned-bool", "r_bool", "summary", value);
    reach("returned-bool");
    reached("returned-bool");
    black_box(value)
}

#[inline(never)]
fn r_u128(n: i32) -> u128 {
    let value = (n as u128) << 100 | 3;
    truth("returned-u128", "r_u128", "int", value);
    reach("returned-u128");
    reached("returned-u128");
    black_box(value)
}

#[inline(never)]
fn r_f64(n: i32) -> f64 {
    let value = -2.75 * f64::from(n);
    truth("returned-f64", "r_f64", "f64", format!("{:#x}", value.to_bits()));
    reach("returned-f64");
    reached("returned-f64");
    black_box(value)
}

#[inline(never)]
fn r_f32(n: i32) -> f32 {
    let value = 0.5 * n as f32;
    truth("returned-f32", "r_f32", "f32", format!("{:#x}", value.to_bits()));
    reach("returned-f32");
    reached("returned-f32");
    black_box(value)
}

#[inline(never)]
fn r_level(n: i32) -> Level {
    let value = if n > 0 { Level::High } else { Level::Low };
    truth("returned-level", "r_level", "symbol", format!("{value:?}"));
    reach("returned-level");
    reached("returned-level");
    black_box(value)
}

#[inline(never)]
fn r_pair(n: i32) -> Pair {
    let value = Pair { first: n, second: -n };
    truth("returned-pair", "r_pair.first", "int", value.first);
    reach("returned-pair");
    reached("returned-pair");
    black_box(value)
}

#[inline(never)]
fn r_unit(n: i32) {
    truth("returned-unit", "r_unit", "absent", "");
    reach("returned-unit");
    reached("returned-unit");
    black_box(n);
}

fn main() {
    let n = black_box(std::env::args().count() as i32);
    let mut total = i64::from(r_int(n)) + i64::from(r_bool(n));
    total += (r_u128(n) >> 100) as i64 + r_f64(n) as i64 + r_f32(n) as i64;
    total += (r_level(n) == Level::High) as i64 + i64::from(r_pair(n).second);
    r_unit(n);
    black_box(total);
}
