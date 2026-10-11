//! `uscope record` and `uscope report`: the sampling profiler from the
//! command line.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use uscope::profiler::{Preset, Profile, RecordOptions, SampleSource, StackView};

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum PresetArg {
    /// 49 samples a second with a small stack copy: safe in production.
    Prod,
    /// 999 samples a second with a deep stack copy.
    Dev,
    /// 9999 samples a second.
    Max,
}

#[derive(Debug, clap::Args)]
pub struct RecordArgs {
    /// Write the profile to FILE.
    #[arg(short, long, value_name = "FILE", default_value = "uscope.usprof")]
    output: PathBuf,
    /// How much to record, and what it may cost the program.
    #[arg(long, value_enum, default_value = "dev")]
    preset: PresetArg,
    /// Samples a second of each thread's CPU time, over the preset's.
    #[arg(long, value_name = "HZ")]
    rate: Option<u32>,
    /// Bytes of stack each sample copies, over the preset's.
    #[arg(long, value_name = "BYTES")]
    stack_bytes: Option<u32>,
    /// Sample each call of the function SYMBOL names, instead of CPU time.
    #[arg(long, value_name = "SYMBOL", conflicts_with = "rate")]
    function: Option<String>,
    /// The program to record and its arguments.
    #[arg(last = true, required = true, value_name = "PROGRAM")]
    command: Vec<OsString>,
}

#[derive(Debug, clap::Args)]
pub struct ReportArgs {
    /// The profile to report.
    file: PathBuf,
    /// Show the call tree instead of the most expensive functions.
    #[arg(long)]
    tree: bool,
    /// Show this many functions.
    #[arg(long, default_value_t = 20)]
    limit: usize,
    /// Show the frames threads ran, not each async task's own stack.
    #[arg(long)]
    native: bool,
    /// Hide call-tree nodes below this percentage of the total.
    #[arg(long, value_name = "PERCENT", default_value_t = 1.0)]
    min_percent: f64,
}

/// Runs `uscope record`, exiting as the program did.
pub fn run_record(args: &RecordArgs) -> std::process::ExitCode {
    match record(args) {
        Ok(code) => std::process::ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Runs `uscope report` to standard output.
pub fn run_report(args: &ReportArgs) -> std::process::ExitCode {
    match report(args, &mut std::io::stdout().lock()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) if super::is_broken_pipe(&error) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn record(args: &RecordArgs) -> Result<i32> {
    let mut options = RecordOptions::preset(match args.preset {
        PresetArg::Prod => Preset::Production,
        PresetArg::Dev => Preset::Development,
        PresetArg::Max => Preset::Maximum,
    });
    if let Some(rate) = args.rate {
        options.source = SampleSource::Clock { rate };
    }
    if let Some(symbol) = &args.function {
        options.source = SampleSource::Function {
            symbol: symbol.clone(),
        };
    }
    if let Some(bytes) = args.stack_bytes {
        options.stack_bytes = bytes;
    }
    let mut command = Command::new(&args.command[0]);
    command.args(&args.command[1..]);
    let recording = uscope::profiler::record(command, &options)?;
    recording
        .profile
        .write(&args.output)
        .with_context(|| format!("writing {}", args.output.display()))?;
    eprintln!(
        "uscope: recorded {} samples to {}",
        recording.profile.samples(),
        args.output.display()
    );
    let lost = recording.profile.lost_samples();
    if lost > 0 {
        eprintln!("uscope: the kernel dropped {lost} samples");
    }
    Ok(recording.status.code().unwrap_or(1))
}

#[expect(
    clippy::cast_precision_loss,
    reason = "seconds and percentages need no more precision than f64's"
)]
fn report(args: &ReportArgs, out: &mut impl std::io::Write) -> Result<()> {
    let read =
        Profile::read(&args.file).with_context(|| format!("reading {}", args.file.display()))?;
    let profile = read.profile;
    let view = if args.native {
        StackView::Native
    } else {
        StackView::Logical
    };
    if read.truncated {
        writeln!(
            out,
            "The file was cut short; this is the part that was complete."
        )?;
    }
    let total = profile.total_weight();
    let program = profile.setting("program").unwrap_or("the program");
    let unit = profile.setting("weight").unwrap_or("nanoseconds");
    let weight = |value: u64| match unit {
        "nanoseconds" => format!("{:.3} s", value as f64 / 1e9),
        _ => format!("{value} executions"),
    };
    writeln!(
        out,
        "{program}: {} samples, {}",
        profile.samples(),
        weight(total)
    )?;
    let lost = profile.lost_samples();
    if lost > 0 {
        writeln!(out, "The kernel dropped {lost} samples.")?;
    }
    let percent = |value: u64| {
        if total == 0 {
            0.0
        } else {
            value as f64 * 100.0 / total as f64
        }
    };
    writeln!(out)?;
    if args.tree {
        let tree = profile.call_tree(view);
        let mut depth = vec![0_usize; tree.len()];
        // Each node with its children after it, depth first.
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); tree.len()];
        let mut roots = Vec::new();
        for (index, node) in tree.iter().enumerate() {
            match node.parent {
                Some(parent) => children[parent].push(index),
                None => roots.push(index),
            }
        }
        let by_total = |nodes: &mut Vec<usize>| {
            nodes.sort_by(|a, b| tree[*b].total.cmp(&tree[*a].total).then(a.cmp(b)));
        };
        for list in &mut children {
            by_total(list);
        }
        by_total(&mut roots);
        let mut pending: Vec<usize> = roots.into_iter().rev().collect();
        while let Some(index) = pending.pop() {
            let node = &tree[index];
            if percent(node.total) < args.min_percent {
                continue;
            }
            writeln!(
                out,
                "{:6.2}% {:6.2}%  {}{}",
                percent(node.total),
                percent(node.own),
                "  ".repeat(depth[index]),
                node.name
            )?;
            for &child in children[index].iter().rev() {
                depth[child] = depth[index] + 1;
                pending.push(child);
            }
        }
    } else {
        writeln!(out, "{:>7} {:>7}  function", "own", "total")?;
        for cost in profile.top(view).iter().take(args.limit) {
            writeln!(
                out,
                "{:6.2}% {:6.2}%  {}",
                percent(cost.own),
                percent(cost.total),
                cost.name
            )?;
        }
    }
    let truncations = profile.truncations();
    if !truncations.is_empty() {
        writeln!(out)?;
        writeln!(out, "Stacks that end short of their outermost frame:")?;
        for (reason, count) in truncations {
            writeln!(out, "  {count:>8}  {reason}")?;
        }
    }
    Ok(())
}
