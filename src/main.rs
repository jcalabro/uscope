//! The `uscope` command-line debugger.

mod cli;
mod dap;
mod web;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use uscope::{CoreDumpOptions, Debugger, ProcessId, SourcePathMap};

use cli::terminal::{ColorChoice, Role};
use cli::{Cli, DisassemblySyntax, LaunchSettings, Renderers};

const COMMON_FORMS: &str = "\
Common forms:
  uscope EXECUTABLE [-- ARGS...]
  uscope --attach PID [EXECUTABLE]
  uscope --core CORE [EXECUTABLE]

Use `uscope --help` for every option.";

#[derive(Parser)]
#[command(
    version,
    about = "Debug Linux x86-64 programs, processes, and core dumps",
    after_help = COMMON_FORMS,
    subcommand_help_heading = "Tools",
    subcommand_value_name = "TOOL",
    subcommand_negates_reqs = true,
    args_conflicts_with_subcommands = true
)]
struct Args {
    /// Native executable to launch.
    ///
    /// With --attach or --core, the executable is normally discovered but
    /// can be supplied when automatic discovery is unavailable.
    #[arg(
        value_name = "EXECUTABLE",
        required_unless_present_any = ["attach", "core"],
        help_heading = "Target"
    )]
    executable: Option<PathBuf>,

    /// Attach to a running process.
    ///
    /// The executable is discovered through /proc by default.
    #[arg(
        short = 'p',
        long,
        value_name = "PID",
        conflicts_with = "core",
        help_heading = "Target"
    )]
    attach: Option<u64>,

    /// Inspect a core dump.
    ///
    /// The executable recorded in the dump is used by default.
    #[arg(long, value_name = "CORE", help_heading = "Target")]
    core: Option<PathBuf>,

    /// Execute commands from a file. May be repeated.
    #[arg(
        short = 'c',
        long = "command",
        value_name = "FILE",
        help_heading = "Startup"
    )]
    command_files: Vec<PathBuf>,

    /// Execute one command. May be repeated.
    #[arg(
        short = 'e',
        long = "eval",
        value_name = "COMMAND",
        help_heading = "Startup"
    )]
    commands: Vec<String>,

    /// Execute commands without starting the interactive REPL.
    #[arg(long, help_heading = "Startup")]
    batch: bool,

    /// Look up the core dump's recorded module paths inside DIR, a copy of the
    /// files of the machine that wrote it, instead of on this machine.
    #[arg(
        long,
        value_name = "DIR",
        requires = "core",
        hide_short_help = true,
        help_heading = "Core dump files"
    )]
    sysroot: Option<PathBuf>,

    /// Search DIR for core dump modules missing from their recorded paths or
    /// not matching the dump, by file name and then by build-id. May be repeated.
    #[arg(
        long = "module-path",
        value_name = "DIR",
        requires = "core",
        hide_short_help = true,
        help_heading = "Core dump files"
    )]
    module_paths: Vec<PathBuf>,

    /// Use module files that cannot be proven to match the core dump.
    #[arg(
        long,
        requires = "core",
        hide_short_help = true,
        help_heading = "Core dump files"
    )]
    allow_module_mismatch: bool,

    /// Read source files recorded under FROM from TO instead, such as for a
    /// program built elsewhere. May be repeated; earlier rules are tried first.
    #[arg(
        long,
        num_args = 2,
        value_names = ["FROM", "TO"],
        hide_short_help = true,
        help_heading = "Debug information"
    )]
    source_map: Vec<PathBuf>,

    /// Present values with the views in FILE, ahead of the project's, the
    /// user's, the program's own, and the built-in ones. May be repeated;
    /// later files come first.
    #[arg(
        long = "views",
        value_name = "FILE",
        hide_short_help = true,
        help_heading = "Debug information"
    )]
    views: Vec<PathBuf>,

    /// Run the launched program in DIR instead of the current directory.
    #[arg(
        long,
        value_name = "DIR",
        conflicts_with_all = ["attach", "core"],
        hide_short_help = true,
        help_heading = "Launch environment"
    )]
    cwd: Option<PathBuf>,

    /// Set NAME to VALUE in the launched program's environment. May be repeated.
    #[arg(
        long = "env",
        value_name = "NAME=VALUE",
        value_parser = parse_environment_variable,
        conflicts_with_all = ["attach", "core"],
        hide_short_help = true,
        help_heading = "Launch environment"
    )]
    environment: Vec<(OsString, OsString)>,

    /// Arguments passed to the launched program.
    #[arg(
        last = true,
        value_name = "ARGS",
        conflicts_with_all = ["attach", "core"],
        hide_short_help = true,
        help_heading = "Launch environment"
    )]
    arguments: Vec<OsString>,

    /// Control colored terminal output.
    #[arg(
        long,
        value_enum,
        default_value_t,
        hide_short_help = true,
        help_heading = "Display"
    )]
    color: ColorChoice,

    /// The assembly syntax `disassemble` renders.
    #[arg(
        long,
        value_enum,
        default_value_t,
        hide_short_help = true,
        help_heading = "Display"
    )]
    disassembly_syntax: DisassemblySyntax,

    #[command(subcommand)]
    tool: Option<Tool>,
}

#[derive(Subcommand)]
enum Tool {
    /// Serve the Debug Adapter Protocol to an editor.
    #[command(version)]
    Dap(dap::DapArgs),
    /// Check and explain how views present program types.
    Views(ViewsArgs),
    /// Serve a debugger to web browsers.
    Web(web::WebArgs),
}

fn parse_environment_variable(text: &str) -> std::result::Result<(OsString, OsString), String> {
    match text.split_once('=') {
        Some((name, value)) if !name.is_empty() => Ok((name.into(), value.into())),
        _ => Err(format!("expected NAME=VALUE, found '{text}'")),
    }
}

fn main() -> ExitCode {
    // The terminal launcher must stay single-threaded, so it runs before the
    // runtime starts its threads.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|command| command == "dap-launcher")
    {
        return dap::launcher::run(std::env::args_os().skip(1));
    }
    // So is the helper that opens browsers, which forks.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|command| command == web::terminal::HELPER)
    {
        return web::terminal::run_helper(std::env::args_os().skip(2));
    }
    #[cfg(debug_assertions)]
    start_flight_recording();
    match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime.block_on(async_main()),
        Err(error) => {
            eprintln!("error: cannot start the async runtime: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Streams a development build's flight recording to the file named by
/// `USCOPE_FLIGHT_RECORDING`, or to a new file under the recorder's `runs`
/// directory. An empty variable turns recording off, as does running under
/// nextest without the variable, so test runs do not crowd out recordings of
/// development runs.
///
/// The variable is removed, so programs the debugger launches see the same
/// environment, and so the same stack layout, as under a release build.
#[cfg(debug_assertions)]
#[allow(unsafe_code, reason = "the environment can only be edited unsafely")]
fn start_flight_recording() {
    use uscope::flight_recorder;

    const VARIABLE: &str = "USCOPE_FLIGHT_RECORDING";
    let path = std::env::var_os(VARIABLE);
    // SAFETY: this runs before the async runtime starts, while the process
    // has no other thread that could read the environment.
    unsafe { std::env::remove_var(VARIABLE) };
    let started = match path {
        Some(path) if path.is_empty() => return,
        Some(path) => flight_recorder::stream_to(std::path::Path::new(&path)),
        None if std::env::var_os("NEXTEST").is_some() => return,
        None => flight_recorder::stream_run().map(drop),
    };
    if let Err(error) = started {
        eprintln!("warning: cannot write the flight recording: {error}");
    }
}

async fn async_main() -> ExitCode {
    let args = parse_args();
    match &args.tool {
        Some(Tool::Dap(dap_args)) => {
            let code = match dap::run(dap_args).await {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("error: {error:#}");
                    1
                }
            };
            // Reading stdin blocks a runtime thread that would keep the
            // runtime from shutting down after the client left.
            std::process::exit(code);
        }
        Some(Tool::Web(web_args)) => {
            let code = match web::run(web_args).await {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("error: {error:#}");
                    1
                }
            };
            // Tabs still connected would keep the runtime from shutting down.
            std::process::exit(code);
        }
        Some(Tool::Views(views)) => {
            return match run_views(views).await {
                Ok(true) => ExitCode::SUCCESS,
                Ok(false) => ExitCode::FAILURE,
                Err(error) => {
                    eprintln!("error: {error:#}");
                    ExitCode::FAILURE
                }
            };
        }
        None => {}
    }
    let renderers = Renderers::detect(args.color, args.batch);

    match run(&args, renderers).await {
        Ok(()) => ExitCode::SUCCESS,
        // A closed stdout pipe, such as `uscope ... | head`, is a normal end.
        Err(error) if cli::is_broken_pipe(&error) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!(
                "{}: {error:#}",
                renderers.stderr.paint(Role::Error, "error")
            );
            ExitCode::FAILURE
        }
    }
}

fn parse_args() -> Args {
    let mut arguments = std::env::args_os().collect::<Vec<_>>();
    if arguments.len() == 1 {
        arguments.push("-h".into());
    }
    let color = cli::help::color_choice(&arguments);
    let command = cli::help::configure(Args::command(), color);
    let matches = command.get_matches_from(arguments);
    Args::from_arg_matches(&matches).unwrap_or_else(|error| error.exit())
}

async fn run(args: &Args, renderers: Renderers) -> Result<()> {
    let mut source_paths = SourcePathMap::new();
    for [from, to] in args.source_map.as_chunks::<2>().0 {
        source_paths
            .push(from, to)
            .context("invalid --source-map rule")?;
    }
    let debugger = open_debugger(args).await?;
    let handle = debugger.handle().with_source_paths(source_paths);
    let launch = LaunchSettings {
        arguments: args.arguments.clone(),
        environment: args
            .environment
            .iter()
            .map(|(name, value)| (name.clone(), Some(value.clone())))
            .collect(),
        working_directory: args.cwd.clone(),
    };
    let result = Cli::new(handle, renderers, args.disassembly_syntax.into(), launch)
        .run(args)
        .await;
    let shutdown = debugger
        .shutdown()
        .await
        .context("failed to shut down debugger");

    result?;
    shutdown
}

/// Checks which views present a program's types, from its debug
/// information alone.
#[derive(clap::Args)]
struct ViewsArgs {
    #[command(subcommand)]
    command: ViewsCommand,
}

#[derive(clap::Subcommand)]
enum ViewsCommand {
    /// Report which view presents each of the program's types that a view's
    /// pattern names, and the loaded views that present no type. Fails
    /// when a view file, or a view loaded for the session or carried by
    /// the program, cannot be used.
    Check {
        /// The program whose types to check.
        program: PathBuf,
        /// Also present values with the views in FILE. May be repeated.
        #[arg(long = "views", value_name = "FILE")]
        views: Vec<PathBuf>,
    },
    /// Explain which view presents a type of the program, and why each
    /// view tried before it did not bind.
    Explain {
        /// The program that defines the type.
        program: PathBuf,
        /// The type, as an expression names one: `intvec`, `std::vector<int>`.
        #[arg(value_name = "TYPE")]
        name: String,
        /// Also present values with the views in FILE. May be repeated.
        #[arg(long = "views", value_name = "FILE")]
        views: Vec<PathBuf>,
    },
    /// Replay kernel runs that `views record` recorded, with no program.
    /// Fails unless every run does again what it did.
    Replay {
        /// The recorded runs.
        recording: PathBuf,
        /// Replay with the kernel module in FILE, rather than the built-in
        /// kernel each run names.
        #[arg(long = "kernel", value_name = "FILE")]
        kernel: Option<PathBuf>,
    },
}

/// The largest recording `uscope views replay` reads: far more than the
/// largest inspection's runs record.
const MAX_RECORDING_BYTES: u64 = 64 * 1024 * 1024;

/// Replays recorded kernel runs, saying for each whether it reproduced.
fn replay_views(recording: &std::path::Path, kernel: Option<&std::path::Path>) -> Result<bool> {
    use std::io::Read as _;
    let mut text = String::new();
    std::fs::File::open(recording)
        .and_then(|file| file.take(MAX_RECORDING_BYTES + 1).read_to_string(&mut text))
        .with_context(|| format!("failed to read {}", recording.display()))?;
    if text.len() as u64 > MAX_RECORDING_BYTES {
        anyhow::bail!(
            "{} is larger than a recording may be, {MAX_RECORDING_BYTES} bytes",
            recording.display()
        );
    }
    let wasm = kernel
        .map(|path| {
            std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))
        })
        .transpose()?;
    let runs = uscope::replay_kernel_runs(&text, wasm.as_deref())
        .map_err(|error| anyhow::anyhow!("{}: {error}", recording.display()))?;
    let mut reproduced = true;
    for (index, run) in runs.iter().enumerate() {
        match &run.outcome {
            Ok(events) => println!(
                "run {}: kernel `{}` reproduced {events} of {} events",
                index + 1,
                run.kernel,
                run.events
            ),
            Err(difference) => {
                reproduced = false;
                println!(
                    "run {}: kernel `{}` differs: {difference}",
                    index + 1,
                    run.kernel
                );
            }
        }
    }
    Ok(reproduced && !runs.is_empty())
}

/// Runs `uscope views check`, `explain`, or `replay`.
async fn run_views(args: &ViewsArgs) -> Result<bool> {
    let (program, views) = match &args.command {
        ViewsCommand::Check { program, views } | ViewsCommand::Explain { program, views, .. } => {
            (program, views)
        }
        ViewsCommand::Replay { recording, kernel } => {
            return replay_views(recording, kernel.as_deref());
        }
    };
    let debugger = Debugger::new(program)
        .with_context(|| format!("failed to initialize debugger for {}", program.display()))?;
    let renderers = Renderers::detect(ColorChoice::Auto, true);
    let console = Cli::new(
        debugger.handle(),
        renderers,
        uscope::AssemblySyntax::Intel,
        LaunchSettings::default(),
    );
    let working_directory = std::env::current_dir().unwrap_or_default();
    // A file that could not be used fails either command, as a view that
    // binds nothing fails a check.
    let usable = console.load_views(&working_directory, views).await;
    let handle = debugger.handle();
    let reported: Result<bool> = async {
        Ok(match &args.command {
            ViewsCommand::Check { .. } => {
                let check = handle.check_views().await?;
                let (report, failed) = cli::format::view_check(&check, renderers.stdout);
                println!("{report}");
                !failed
            }
            ViewsCommand::Replay { .. } => unreachable!("a replay needs no program"),
            ViewsCommand::Explain { name, .. } => {
                let types = handle.explain_type(name).await?;
                println!(
                    "{}",
                    cli::format::type_views(name, &types, renderers.stdout)
                );
                !types.is_empty()
            }
        })
    }
    .await;
    let shutdown = debugger
        .shutdown()
        .await
        .context("failed to shut down the debugger");
    let succeeded = reported?;
    shutdown?;
    Ok(succeeded && usable)
}

async fn open_debugger(args: &Args) -> Result<Debugger> {
    if let Some(core) = &args.core {
        return Debugger::open_core(&CoreDumpOptions {
            core: core.clone(),
            executable: args.executable.clone(),
            sysroot: args.sysroot.clone(),
            module_paths: args.module_paths.clone(),
            allow_module_mismatch: args.allow_module_mismatch,
        })
        .with_context(|| format!("failed to open core dump {}", core.display()));
    }
    if let Some(pid) = args.attach {
        let process = ProcessId::new(pid);
        return match &args.executable {
            Some(executable) => Debugger::attach_with_executable(process, executable)
                .await
                .with_context(|| format!("failed to attach to process {pid}")),
            None => Debugger::attach(process).await.with_context(|| {
                format!(
                    "failed to attach to process {pid}; if automatic /proc executable discovery is unavailable, pass EXECUTABLE explicitly"
                )
            }),
        };
    }
    let executable = args
        .executable
        .as_ref()
        .expect("clap requires an executable unless --attach or --core is present");
    Debugger::new(executable)
        .with_context(|| format!("failed to initialize debugger for {}", executable.display()))
}
