//! Developer tools: dump every answer a program's debug information gives,
//! summarize `--timings` reports, and benchmark loading a pinned corpus.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use uscope::profile::Report;
use uscope::tools::Measured;
use uscope::tools::dump::{Options as DumpOptions, Section};

// Allocates as uscope does, so that loads cost what they cost there.
#[global_allocator]
static ALLOCATOR: uscope::profile::alloc::Counting<uscope::profile::alloc::Selected> =
    uscope::profile::alloc::selected();

#[derive(Parser)]
#[command(about = "uscope's developer tools")]
struct Args {
    /// How many threads load debug information; otherwise `USCOPE_JOBS`,
    /// or one per CPU up to 16.
    #[arg(long, global = true)]
    jobs: Option<std::num::NonZeroUsize>,
    /// Load through the image cache in DIR; loads are uncached otherwise,
    /// so that measurements ingest debug information.
    #[arg(long, global = true, value_name = "DIR")]
    cache: Option<PathBuf>,
    #[command(subcommand)]
    command: Tool,
}

#[derive(Subcommand)]
enum Tool {
    /// Print every answer PROGRAM's debug information gives, in a canonical
    /// form two loaders' dumps can be compared in.
    Dump {
        program: PathBuf,
        /// Dump only these sections; every one by default.
        #[arg(long, value_enum, value_delimiter = ',')]
        sections: Vec<Section>,
        /// How many statement addresses of each code instance to inspect
        /// variables at, besides its entry and last instruction.
        #[arg(long, default_value_t = 3)]
        variable_addresses: usize,
        /// Write the dump to FILE instead of stdout.
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
    /// Load PROGRAM once and print what it cost, as JSON.
    Load {
        program: PathBuf,
        /// Count retired instructions with a hardware counter.
        #[arg(long)]
        instructions: bool,
    },
    /// Load every program and shared library under PATHS through the image
    /// cache `--cache` names, so that later loads read it.
    Warm {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// Summarize a `--timings` report, or the differences between two.
    Timings {
        /// The report; with a second, the base it is compared to.
        file: PathBuf,
        /// The newer report, compared with the first.
        new: Option<PathBuf>,
        /// Also show each thread's busy time and the work each span spread
        /// across threads.
        #[arg(long)]
        threads: bool,
        /// Show every phase, not only the first forty.
        #[arg(long)]
        all: bool,
    },
    /// Load each program of a pinned corpus in processes of their own, and
    /// report what loading costs.
    Bench(BenchArgs),
    /// Group the blocks a DHAT profile records by the first uscope function
    /// that allocated them.
    Heap {
        /// The `dhat.out` file DHAT wrote.
        profile: PathBuf,
        /// How many sites to list in each ranking.
        #[arg(long, default_value_t = 25)]
        rows: usize,
    },
}

#[derive(clap::Args)]
struct BenchArgs {
    /// The seconds-long corpus of small programs, or every program.
    #[arg(long, value_enum, default_value = "full")]
    corpus: Corpus,
    /// Loads per program, in processes of their own.
    #[arg(long, default_value_t = 5)]
    repeat: usize,
    /// Write the report as JSON to FILE.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Compare with a saved report whose provenance matches.
    #[arg(long, value_name = "BASE")]
    compare: Option<PathBuf>,
    /// Fail unless the deterministic counts are within those BASELINE
    /// records for the same programs.
    #[arg(long, value_name = "BASELINE")]
    check: Option<PathBuf>,
    /// Rewrite BASELINE with this run's deterministic counts, without
    /// checking them against it. With `--only`, the other programs keep
    /// their counts.
    #[arg(long, value_name = "BASELINE")]
    record: Option<PathBuf>,
    /// Also count each load's instructions under Callgrind.
    #[arg(long)]
    callgrind: bool,
    /// Only the programs whose names contain one of these.
    #[arg(long, value_delimiter = ',')]
    only: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Corpus {
    Smoke,
    Full,
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<bool> {
    let jobs = uscope::pool::configure(args.jobs, None)?;
    uscope::cache::configure(Some(args.cache.clone().map_or(
        uscope::cache::Setting::Off,
        uscope::cache::Setting::Directory,
    )))?;
    match args.command {
        Tool::Dump {
            program,
            sections,
            variable_addresses,
            output,
        } => {
            let mut options = DumpOptions {
                variable_addresses,
                ..DumpOptions::default()
            };
            if !sections.is_empty() {
                options.sections = sections.into_iter().collect();
            }
            let mut out: Box<dyn std::io::Write> = match &output {
                Some(path) => Box::new(std::io::BufWriter::new(
                    std::fs::File::create(path)
                        .with_context(|| format!("cannot create {}", path.display()))?,
                )),
                None => Box::new(std::io::BufWriter::new(std::io::stdout().lock())),
            };
            uscope::tools::dump::dump(&program, &options, &mut out)?;
            out.flush()?;
            Ok(true)
        }
        Tool::Load {
            program,
            instructions,
        } => {
            let measured = uscope::tools::measure_load(&program, instructions)?;
            println!("{}", serde_json::to_string(&measured)?);
            Ok(true)
        }
        Tool::Warm { paths } => {
            if args.cache.is_none() {
                bail!("warm needs --cache DIR");
            }
            let warmed = uscope::tools::warm(&paths)?;
            for (path, error) in &warmed.failed {
                println!("{}: {error}", path.display());
            }
            println!(
                "{} modules in the image cache, {} that do not load",
                warmed.loaded,
                warmed.failed.len()
            );
            Ok(true)
        }
        Tool::Timings {
            file,
            new,
            threads,
            all,
        } => {
            let first = read_report(&file)?;
            match new {
                None => {
                    print!("{}", first.text(all));
                    if threads {
                        print!("\n{}", first.threads_text());
                    }
                }
                Some(new) => {
                    let new = read_report(&new)?;
                    print!("{}", new.diff_text(&first, all));
                    if threads {
                        print!(
                            "\nbase:\n{}\nnew:\n{}",
                            first.threads_text(),
                            new.threads_text()
                        );
                    }
                }
            }
            Ok(true)
        }
        // Unless told how many, each program loads on one worker, whose
        // counts repeat exactly, and on as many as there are.
        Tool::Bench(bench) => run_bench(
            &bench,
            &match args.jobs {
                Some(jobs) => vec![jobs.get()],
                None if jobs == 1 => vec![1],
                None => vec![1, jobs],
            },
        ),
        Tool::Heap { profile, rows } => {
            print!("{}", heap_text(&profile, rows)?);
            Ok(true)
        }
    }
}

/// A DHAT profile, as `dhat.out` holds it.
#[derive(Deserialize)]
struct Dhat {
    pps: Vec<DhatSite>,
    ftbl: Vec<String>,
}

/// One allocation site: its total bytes and blocks, and those live at the
/// heap's peak.
#[derive(Deserialize)]
struct DhatSite {
    tb: u64,
    tbk: u64,
    #[serde(default)]
    gb: u64,
    #[serde(default)]
    gbk: u64,
    fs: Vec<usize>,
}

/// The first uscope frame of a stack that is not the standard library's
/// plumbing, with its file and line.
fn allocation_site(frames: &[usize], table: &[String]) -> String {
    const PLUMBING: [&str; 22] = [
        "alloc::",
        "core::",
        "__rust",
        "RawVec",
        "raw_vec",
        "from_iter",
        "collect",
        "Spec",
        "to_vec",
        "<std::",
        "hashbrown",
        "clone",
        "::fmt",
        "format",
        "to_string",
        "to_owned",
        "From<",
        "FromIterator",
        "Extend",
        "btree",
        "extend",
        "push",
    ];
    for &frame in frames {
        let Some(name) = table.get(frame) else {
            continue;
        };
        let function = name.split(" (").next().unwrap_or(name);
        let function = function.split_once(": ").map_or(function, |(_, rest)| rest);
        if !function.contains("uscope") || PLUMBING.iter().any(|skip| function.contains(skip)) {
            continue;
        }
        let place = name
            .rsplit_once('(')
            .and_then(|(_, place)| place.strip_suffix(')'))
            .and_then(|place| place.split_once("src/").map(|(_, place)| place));
        let short = function.get(..110).unwrap_or(function);
        return place.map_or_else(|| short.to_owned(), |place| format!("{short}  @{place}"));
    }
    frames
        .first()
        .and_then(|frame| table.get(*frame))
        .map_or_else(
            || "??".to_owned(),
            |name| format!("?? {}", name.get(..100).unwrap_or(name)),
        )
}

#[expect(clippy::cast_precision_loss, reason = "reported approximately")]
fn heap_text(path: &Path, rows: usize) -> Result<String> {
    let profile: Dhat = serde_json::from_str(
        &std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?,
    )?;
    let mut sites = BTreeMap::<String, [u64; 4]>::new();
    for site in &profile.pps {
        let totals = sites
            .entry(allocation_site(&site.fs, &profile.ftbl))
            .or_default();
        totals[0] += site.tb;
        totals[1] += site.tbk;
        totals[2] += site.gb;
        totals[3] += site.gbk;
    }
    let sum = |index: usize| sites.values().map(|totals| totals[index]).sum::<u64>();
    let mut text = format!(
        "total {:.0} MB in {:.2} M blocks; at peak {:.0} MB in {:.2} M blocks\n",
        sum(0) as f64 / 1e6,
        sum(1) as f64 / 1e6,
        sum(2) as f64 / 1e6,
        sum(3) as f64 / 1e6
    );
    let mut ranked = sites.iter().collect::<Vec<_>>();
    ranked.sort_by_key(|(site, totals)| (std::cmp::Reverse(totals[2]), (*site).clone()));
    text.push_str("\nby bytes live at peak\n");
    for (site, totals) in ranked.iter().take(rows) {
        let _ = writeln!(
            text,
            "{:8.1} MB {:9} blocks  {site}",
            totals[2] as f64 / 1e6,
            totals[3]
        );
    }
    ranked.sort_by_key(|(site, totals)| (std::cmp::Reverse(totals[1]), (*site).clone()));
    text.push_str("\nby blocks allocated\n");
    for (site, totals) in ranked.iter().take(rows) {
        let _ = writeln!(
            text,
            "{:9} blocks {:8.1} MB  {site}",
            totals[1],
            totals[0] as f64 / 1e6
        );
    }
    Ok(text)
}

fn read_report(path: &Path) -> Result<Report> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    Report::from_json(&text).map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))
}

/// The bench report format's version.
const BENCH_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Provenance {
    commit: String,
    dirty: bool,
    cpu: String,
    cpus: usize,
    kernel: String,
    flake_lock: String,
    cargo_lock: String,
    debug_assertions: bool,
    system_alloc: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BenchReport {
    schema: u32,
    provenance: Provenance,
    programs: Vec<ProgramResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProgramResult {
    name: String,
    /// The loader's workers; reports from before there were several had
    /// one.
    #[serde(default = "one")]
    jobs: usize,
    path: PathBuf,
    digest: String,
    bytes: u64,
    outcome: Result<(), String>,
    wall_ms: Vec<f64>,
    ready_ms_median: f64,
    cpu_ms_median: f64,
    peak_rss_bytes_median: Option<u64>,
    allocations: Option<u64>,
    allocated_bytes: Option<u64>,
    peak_heap_growth_bytes: Option<i64>,
    retained_heap_bytes: Option<i64>,
    instructions: Option<u64>,
    callgrind_instructions: Option<u64>,
}

const fn one() -> usize {
    1
}

impl ProgramResult {
    /// The program, and how many workers loaded it when more than one.
    fn label(&self) -> String {
        if self.jobs == 1 {
            self.name.clone()
        } else {
            format!("{} x{}", self.name, self.jobs)
        }
    }
}

/// The deterministic counts a baseline holds for each program, loaded on
/// one worker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Counts {
    digest: String,
    allocations: Option<u64>,
    callgrind_instructions: Option<u64>,
}

/// One program of the corpus, by its name and where it is built.
struct Program {
    name: String,
    path: PathBuf,
}

fn corpus(kind: Corpus) -> Result<Vec<Program>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut programs = Vec::new();
    // The golden programs' executables, one directory of builds each.
    let golden = root.join("build/golden");
    let mut golden_programs = Vec::new();
    for directory in std::fs::read_dir(&golden).with_context(|| {
        format!(
            "run `just build-test-programs` to build {}",
            golden.display()
        )
    })? {
        for file in std::fs::read_dir(directory?.path())? {
            let path = file?.path();
            let executable = std::fs::metadata(&path).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            });
            if executable {
                golden_programs.push(path);
            }
        }
    }
    golden_programs.sort();
    for path in golden_programs {
        let name = format!(
            "golden/{}",
            path.file_name().expect("a file name").to_string_lossy()
        );
        programs.push(Program { name, path });
    }
    let fixtures = [
        "containers-cpp-gcc-o2",
        "containers-zig-self-hosted",
        "containers-rust-o0",
        "server-go-o0",
    ];
    let large = ["tokio-server-o0", "tokio-values-o0"];
    let fixture_names = match kind {
        Corpus::Smoke => fixtures.to_vec(),
        Corpus::Full => fixtures.iter().chain(&large).copied().collect(),
    };
    for name in fixture_names {
        programs.push(Program {
            name: name.to_owned(),
            path: root.join("build/test-programs").join(name),
        });
    }
    if kind == Corpus::Full {
        // The Go command the pinned toolchain ships: 23 MB with compressed
        // debug sections.
        let go = Command::new("go")
            .args(["env", "GOROOT"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| {
                PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()).join("bin/go")
            });
        if let Some(go) = go {
            programs.push(Program {
                name: "go-command".to_owned(),
                path: go,
            });
        }
        programs.push(Program {
            name: "uscope-large".to_owned(),
            path: root.join("target/bench/large/uscope"),
        });
    }
    Ok(programs)
}

fn digest(path: &Path) -> Result<(String, u64)> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok((
        format!("{:032x}", twox_hash::XxHash3_128::oneshot(&bytes)),
        bytes.len() as u64,
    ))
}

fn file_digest(path: &Path) -> String {
    std::fs::read(path).map_or_else(
        |_| "missing".to_owned(),
        |bytes| format!("{:016x}", twox_hash::XxHash3_64::oneshot(&bytes)),
    )
}

fn provenance() -> Provenance {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .ok()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .unwrap_or_default()
    };
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|info| {
            info.lines()
                .find_map(|line| line.strip_prefix("model name"))
                .map(|line| line.trim_start_matches([' ', '\t', ':']).to_owned())
        })
        .unwrap_or_default();
    Provenance {
        commit: git(&["rev-parse", "HEAD"]),
        dirty: !git(&["status", "--porcelain", "--untracked-files=no"]).is_empty(),
        cpu,
        cpus: std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
        kernel: std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .unwrap_or_default()
            .trim()
            .to_owned(),
        flake_lock: file_digest(&root.join("flake.lock")),
        cargo_lock: file_digest(&root.join("Cargo.lock")),
        debug_assertions: cfg!(debug_assertions),
        system_alloc: cfg!(feature = "system-alloc"),
    }
}

fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn load_once(program: &Path, jobs: usize) -> Result<Measured> {
    let output = Command::new(std::env::current_exe()?)
        .arg(format!("--jobs={jobs}"))
        .arg("load")
        .arg("--instructions")
        .arg(program)
        .output()?;
    if !output.status.success() {
        bail!(
            "loading {} failed: {}",
            program.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

/// The instructions one load of `program` runs, as Callgrind counts them.
fn callgrind(program: &Path) -> Result<u64> {
    let out = std::env::temp_dir().join(format!("uscope-callgrind-{}.out", std::process::id()));
    // One worker, so that the count is the same each run.
    let status = Command::new("valgrind")
        .arg("--tool=callgrind")
        .arg(format!("--callgrind-out-file={}", out.display()))
        .arg(std::env::current_exe()?)
        .arg("--jobs=1")
        .arg("load")
        .arg(program)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("run valgrind, which the dev shell provides")?;
    let text = std::fs::read_to_string(&out);
    let _ = std::fs::remove_file(&out);
    if !status.success() {
        bail!("callgrind failed on {}", program.display());
    }
    text?
        .lines()
        .find_map(|line| {
            line.strip_prefix("summary: ")
                .or_else(|| line.strip_prefix("totals: "))
        })
        .and_then(|count| count.split_whitespace().next()?.parse().ok())
        .context("callgrind wrote no instruction total")
}

#[expect(clippy::too_many_lines, reason = "one benchmark run, start to end")]
fn run_bench(args: &BenchArgs, sweep: &[usize]) -> Result<bool> {
    let mut programs = corpus(args.corpus)?;
    if !args.only.is_empty() {
        programs.retain(|program| args.only.iter().any(|only| program.name.contains(only)));
    }
    let mut results = Vec::new();
    for (program, &jobs) in programs
        .iter()
        .flat_map(|program| sweep.iter().map(move |jobs| (program, jobs)))
    {
        let Ok((digest, bytes)) = digest(&program.path) else {
            eprintln!(
                "skipping {}: {} is missing",
                program.name,
                program.path.display()
            );
            continue;
        };
        eprint!("{} x{jobs} ", program.name);
        let mut measured = Vec::new();
        for _ in 0..args.repeat.max(1) {
            measured.push(load_once(&program.path, jobs)?);
            eprint!(".");
        }
        let callgrind_instructions = if args.callgrind && jobs == 1 {
            let count = callgrind(&program.path)?;
            eprint!(" callgrind");
            Some(count)
        } else {
            None
        };
        eprintln!();
        let first = &measured[0];
        let mut wall = measured
            .iter()
            .map(|load| load.report.summary.wall_ms)
            .collect::<Vec<_>>();
        let mut ready = wall.clone();
        let mut cpu = measured
            .iter()
            .map(|load| load.report.summary.cpu_ms)
            .collect::<Vec<_>>();
        #[expect(
            clippy::cast_precision_loss,
            reason = "sizes are compared approximately"
        )]
        let mut rss = measured
            .iter()
            .filter_map(|load| load.report.summary.peak_rss_bytes)
            .map(|bytes| bytes as f64)
            .collect::<Vec<_>>();
        let instructions = first
            .report
            .phases
            .iter()
            .find(|phase| phase.name == "load")
            .and_then(|phase| phase.instructions);
        wall.sort_by(f64::total_cmp);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a median of byte counts"
        )]
        results.push(ProgramResult {
            name: program.name.clone(),
            jobs,
            path: program.path.clone(),
            digest,
            bytes,
            outcome: first.outcome.clone(),
            ready_ms_median: median(&mut ready),
            cpu_ms_median: median(&mut cpu),
            peak_rss_bytes_median: (!rss.is_empty()).then(|| median(&mut rss) as u64),
            allocations: first.report.summary.allocations,
            allocated_bytes: first.report.summary.allocated_bytes,
            peak_heap_growth_bytes: first.report.summary.peak_heap_growth_bytes,
            retained_heap_bytes: first.retained_heap_bytes,
            instructions,
            callgrind_instructions,
            wall_ms: wall,
        });
    }
    let report = BenchReport {
        schema: BENCH_SCHEMA,
        provenance: provenance(),
        programs: results,
    };
    print!("{}", bench_text(&report));
    if let Some(out) = &args.out {
        std::fs::write(out, serde_json::to_string_pretty(&report)?)?;
    }
    let mut passed = true;
    if let Some(base) = &args.compare {
        let base: BenchReport = serde_json::from_str(&std::fs::read_to_string(base)?)?;
        print!("\n{}", compare_text(&base, &report));
    }
    // A baseline being recorded again is not checked: its counts are the
    // ones this run replaces.
    if let Some(path) = args
        .check
        .as_ref()
        .filter(|path| args.record.as_ref() != Some(*path))
    {
        let baseline: BTreeMap<String, Counts> =
            serde_json::from_str(&std::fs::read_to_string(path)?)?;
        let (text, ok) = check_counts(&baseline, &report, args.only.is_empty());
        print!("\n{text}");
        passed &= ok;
    }
    if let Some(path) = &args.record {
        // A run of some programs keeps the others' counts; a run of all
        // replaces the baseline, dropping programs no longer measured.
        let mut counts: BTreeMap<String, Counts> = if args.only.is_empty() {
            BTreeMap::new()
        } else {
            serde_json::from_str(&std::fs::read_to_string(path)?)?
        };
        let measured = report.programs.iter().filter(|program| program.jobs == 1);
        let mut recorded = 0;
        for program in measured {
            counts.insert(
                program.name.clone(),
                Counts {
                    digest: program.digest.clone(),
                    allocations: program.allocations,
                    callgrind_instructions: program.callgrind_instructions,
                },
            );
            recorded += 1;
        }
        let mut text = serde_json::to_string_pretty(&counts)?;
        text.push('\n');
        std::fs::write(path, text)?;
        println!("\nrecorded {recorded} programs in {}", path.display());
    }
    Ok(passed)
}

#[expect(clippy::cast_precision_loss, reason = "reported approximately")]
fn bench_text(report: &BenchReport) -> String {
    let mut text = String::new();
    let provenance = &report.provenance;
    let _ = writeln!(
        text,
        "{}{} on {} ({} cpus), kernel {}",
        provenance.commit.get(..12).unwrap_or(&provenance.commit),
        if provenance.dirty { "+dirty" } else { "" },
        provenance.cpu,
        provenance.cpus,
        provenance.kernel
    );
    let _ = writeln!(
        text,
        "{:<34} {:>9} {:>15} {:>9} {:>9} {:>11} {:>10} {:>10} {:>14}",
        "program",
        "size MB",
        "wall ms (spread)",
        "cpu ms",
        "rss MB",
        "allocs",
        "peak MB",
        "kept MB",
        "instructions"
    );
    for program in &report.programs {
        let spread = match (program.wall_ms.first(), program.wall_ms.last()) {
            (Some(low), Some(high)) => format!("{low:.0}-{high:.0}"),
            _ => "-".to_owned(),
        };
        let mb = |bytes: Option<f64>| {
            bytes.map_or_else(|| "-".to_owned(), |bytes| format!("{:.1}", bytes / 1e6))
        };
        let _ = writeln!(
            text,
            "{:<34} {:>9.1} {:>6.1} {:>8} {:>9.1} {:>9} {:>11} {:>10} {:>10} {:>14}{}",
            program.label(),
            program.bytes as f64 / 1e6,
            program.ready_ms_median,
            spread,
            program.cpu_ms_median,
            mb(program.peak_rss_bytes_median.map(|bytes| bytes as f64)),
            program
                .allocations
                .map_or_else(|| "-".to_owned(), |count| count.to_string()),
            mb(program.peak_heap_growth_bytes.map(|bytes| bytes as f64)),
            mb(program.retained_heap_bytes.map(|bytes| bytes as f64)),
            program
                .callgrind_instructions
                .or(program.instructions)
                .map_or_else(|| "-".to_owned(), |count| count.to_string()),
            match &program.outcome {
                Ok(()) => String::new(),
                Err(error) => format!("  FAILED: {error}"),
            }
        );
    }
    text
}

/// What changed between two runs, measure by measure. Times compare only
/// when the runs' provenance matches; a change within either run's
/// spread is marked as noise.
#[expect(clippy::cast_precision_loss, reason = "reported approximately")]
#[expect(clippy::too_many_lines, reason = "one row for each measure")]
fn compare_text(base: &BenchReport, new: &BenchReport) -> String {
    let mut text = String::new();
    let comparable = Provenance {
        commit: String::new(),
        dirty: false,
        ..base.provenance.clone()
    } == Provenance {
        commit: String::new(),
        dirty: false,
        ..new.provenance.clone()
    };
    if !comparable {
        let _ = writeln!(
            text,
            "the runs' machines, toolchains, or builds differ, so only deterministic counts compare"
        );
    }
    let base_programs = base
        .programs
        .iter()
        .map(|program| (program.label(), program))
        .collect::<BTreeMap<_, _>>();
    let _ = writeln!(
        text,
        "{:<28} {:<14} {:>14} {:>14} {:>9}",
        "program", "measure", "base", "new", "change"
    );
    for program in &new.programs {
        let label = program.label();
        let Some(before) = base_programs.get(&label) else {
            continue;
        };
        let same_input = before.digest == program.digest;
        let mut row = |measure: &str, old: Option<f64>, now: Option<f64>, noise: f64| {
            let (Some(old), Some(now)) = (old, now) else {
                return;
            };
            let change = if old == 0.0 {
                0.0
            } else {
                (now - old) / old * 100.0
            };
            let flag = if change.abs() <= noise { " ~" } else { "" };
            let _ = writeln!(
                text,
                "{label:<28} {measure:<14} {old:>14.1} {now:>14.1} {change:>+8.1}%{flag}"
            );
        };
        if comparable {
            let spread =
                |result: &ProgramResult| match (result.wall_ms.first(), result.wall_ms.last()) {
                    (Some(low), Some(high)) if *low > 0.0 => (high - low) / low * 100.0,
                    _ => 0.0,
                };
            let noise = spread(before).max(spread(program)).max(3.0);
            row(
                "wall ms",
                Some(before.ready_ms_median),
                Some(program.ready_ms_median),
                noise,
            );
            row(
                "cpu ms",
                Some(before.cpu_ms_median),
                Some(program.cpu_ms_median),
                noise,
            );
            row(
                "rss MB",
                before.peak_rss_bytes_median.map(|bytes| bytes as f64 / 1e6),
                program
                    .peak_rss_bytes_median
                    .map(|bytes| bytes as f64 / 1e6),
                2.0,
            );
        }
        if same_input {
            row(
                "allocations",
                before.allocations.map(|count| count as f64),
                program.allocations.map(|count| count as f64),
                0.0,
            );
            row(
                "kept MB",
                before.retained_heap_bytes.map(|bytes| bytes as f64 / 1e6),
                program.retained_heap_bytes.map(|bytes| bytes as f64 / 1e6),
                0.5,
            );
            row(
                "callgrind Ir",
                before.callgrind_instructions.map(|count| count as f64),
                program.callgrind_instructions.map(|count| count as f64),
                0.1,
            );
        } else {
            let _ = writeln!(
                text,
                "{label:<28} rebuilt since the base: only times compare"
            );
        }
    }
    text
}

/// Whether every deterministic count is at most its baseline's, for each
/// program whose input is the one the baseline measured.
fn check_counts(
    baseline: &BTreeMap<String, Counts>,
    report: &BenchReport,
    whole_corpus: bool,
) -> (String, bool) {
    let mut text = String::new();
    let mut ok = true;
    let mut seen = BTreeSet::new();
    for program in report.programs.iter().filter(|program| program.jobs == 1) {
        let Some(counts) = baseline.get(&program.name) else {
            let _ = writeln!(text, "{}: not in the baseline", program.name);
            continue;
        };
        seen.insert(program.name.as_str());
        if counts.digest != program.digest {
            let _ = writeln!(
                text,
                "{}: rebuilt since the baseline, which must be recorded again",
                program.name
            );
            ok = false;
            continue;
        }
        for (measure, limit, now) in [
            ("allocations", counts.allocations, program.allocations),
            (
                "callgrind instructions",
                counts.callgrind_instructions,
                program.callgrind_instructions,
            ),
        ] {
            match (limit, now) {
                (Some(limit), Some(now)) if now > limit => {
                    let _ = writeln!(
                        text,
                        "{}: {measure} rose from {limit} to {now}",
                        program.name
                    );
                    ok = false;
                }
                (Some(limit), Some(now)) if now < limit => {
                    let _ = writeln!(
                        text,
                        "{}: {measure} fell from {limit} to {now}; record the baseline again",
                        program.name
                    );
                }
                _ => {}
            }
        }
    }
    for name in baseline.keys().filter(|_| whole_corpus) {
        if !seen.contains(name.as_str()) {
            let _ = writeln!(text, "{name}: in the baseline but not measured");
        }
    }
    if ok {
        text.push_str("deterministic counts are within the baseline\n");
    }
    (text, ok)
}
