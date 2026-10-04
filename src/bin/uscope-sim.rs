//! Sweeps and replays deterministic simulations of debugging sessions.
//!
//! `uscope-sim sweep` runs random seeds on every core for a while and
//! reports each kind of failure once, with its smallest seed. `uscope-sim
//! replay SEED` reruns one seed and prints its whole trace.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use uscope::sim::{Corpus, Outcome, Settings, describe_failure, run};

#[derive(Parser)]
#[command(about = "Deterministic simulation of uscope debugging sessions")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Runs random seeds on every core and reports each kind of failure.
    Sweep {
        /// How long to sweep.
        #[arg(long, default_value_t = 60)]
        seconds: u64,
        /// How many worker threads; every core by default.
        #[arg(long)]
        threads: Option<usize>,
        /// The first seed; chosen from the clock by default.
        #[arg(long, value_parser = parse_seed)]
        start: Option<u64>,
    },
    /// Reruns one seed, printing its whole trace.
    Replay {
        #[arg(value_parser = parse_seed)]
        seed: u64,
        /// Stops after this action and prints the state there.
        #[arg(long)]
        at: Option<u64>,
        /// The fingerprint the seed's run had, which the replay must match.
        #[arg(long, value_parser = parse_seed)]
        fingerprint: Option<u64>,
    },
}

fn parse_seed(text: &str) -> Result<u64, String> {
    text.strip_prefix("0x")
        .map_or_else(|| text.parse(), |hex| u64::from_str_radix(hex, 16))
        .map_err(|error| format!("not a seed: {error}"))
}

/// How many things per second, for people to read.
#[expect(clippy::cast_precision_loss, reason = "a rate for people to read")]
fn rate(count: u64, elapsed: Duration, threads: usize) -> f64 {
    count as f64 / elapsed.as_secs_f64() / threads as f64
}

fn main() -> Result<ExitCode> {
    let args = Args::parse();
    let corpus = Corpus::load().context("loading the golden corpus")?;
    match args.command {
        Command::Sweep {
            seconds,
            threads,
            start,
        } => Ok(sweep(&corpus, Duration::from_secs(seconds), threads, start)),
        Command::Replay {
            seed,
            at,
            fingerprint,
        } => replay(&corpus, seed, at, fingerprint),
    }
}

/// One kind of failure: how often it happened and its smallest seed's
/// report.
struct Group {
    count: u64,
    seed: u64,
    report: String,
}

/// Counts a failure in its group, which keeps the smallest seed's report.
fn record_failure(groups: &Mutex<BTreeMap<String, Group>>, key: String, seed: u64, report: String) {
    let mut groups = groups.lock().unwrap_or_else(PoisonError::into_inner);
    let group = groups.entry(key).or_insert_with(|| Group {
        count: 0,
        seed,
        report: report.clone(),
    });
    group.count += 1;
    if seed < group.seed {
        group.seed = seed;
        group.report = report;
    }
    drop(groups);
}

fn sweep(
    corpus: &Corpus,
    duration: Duration,
    threads: Option<usize>,
    start: Option<u64>,
) -> ExitCode {
    let threads = threads.unwrap_or_else(|| {
        std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
    });
    let start = start.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                since
                    .as_secs()
                    .wrapping_mul(1_000_000_000)
                    .wrapping_add(u64::from(since.subsec_nanos()))
            })
    });
    println!("sweeping from seed {start:#x} on {threads} threads for {duration:?}");
    let next = AtomicU64::new(0);
    let done = AtomicBool::new(false);
    let sessions = AtomicU64::new(0);
    let groups = Mutex::new(BTreeMap::<String, Group>::new());
    let began = Instant::now();
    let settings = Settings::default();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                while !done.load(Ordering::Relaxed) {
                    let seed = start.wrapping_add(next.fetch_add(1, Ordering::Relaxed));
                    let outcome = run(seed, corpus, &settings);
                    sessions.fetch_add(1, Ordering::Relaxed);
                    if let Some(failure) = &outcome.failure {
                        let key = format!("{} {}", failure.kind, failure.check);
                        record_failure(&groups, key, seed, describe_failure(&outcome));
                    }
                }
            });
        }
        while began.elapsed() < duration {
            std::thread::sleep(Duration::from_secs(1).min(duration));
            let count = sessions.load(Ordering::Relaxed);
            println!(
                "{:>5.0}s  {count} sessions, {:.0} per second",
                began.elapsed().as_secs_f64(),
                rate(count, began.elapsed(), 1)
            );
        }
        done.store(true, Ordering::Relaxed);
    });
    let groups = groups.into_inner().unwrap_or_else(PoisonError::into_inner);
    let count = sessions.load(Ordering::Relaxed);
    println!(
        "{count} sessions in {:.1}s, {:.0} per second per thread",
        began.elapsed().as_secs_f64(),
        rate(count, began.elapsed(), threads)
    );
    if groups.is_empty() {
        println!("no failures");
        return ExitCode::SUCCESS;
    }
    for (key, group) in &groups {
        println!(
            "\n{key}: {} sessions; smallest seed {:#x}\n{}",
            group.count, group.seed, group.report
        );
    }
    ExitCode::FAILURE
}

fn replay(
    corpus: &Corpus,
    seed: u64,
    at: Option<u64>,
    fingerprint: Option<u64>,
) -> Result<ExitCode> {
    let settings = Settings {
        keep: None,
        stop_at: at,
        ..Settings::default()
    };
    let outcome = run(seed, corpus, &settings);
    let text = report(&outcome);
    print!("{text}");
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/sim")
        .join(format!("{seed:#x}"));
    std::fs::create_dir_all(&directory)?;
    std::fs::write(directory.join("trace.log"), &text)?;
    println!("written to {}", directory.join("trace.log").display());
    if let Some(expected) = fingerprint
        && at.is_none()
        && expected != outcome.fingerprint
    {
        println!(
            "the replay's fingerprint {:#x} differs from {expected:#x}: the run is not \
             deterministic, or the code changed since",
            outcome.fingerprint
        );
        return Ok(ExitCode::FAILURE);
    }
    Ok(if outcome.failure.is_some() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn report(outcome: &Outcome) -> String {
    let mut text = describe_failure(outcome);
    if outcome.failure.is_none() {
        text = format!(
            "seed {:#x} passed: {} {:?}, {} actions\nswarm {}\n{text}",
            outcome.seed, outcome.program, outcome.arguments, outcome.steps, outcome.swarm
        );
    }
    if let Some(state) = &outcome.state {
        let _ = writeln!(text, "state after action #{}:{state}", outcome.steps);
    }
    text
}
