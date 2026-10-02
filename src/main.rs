//! The `uscope` command-line debugger.

mod cli;
mod dap;

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;
use uscope::{CoreDumpOptions, Debugger, ProcessId, SourcePathMap};

use cli::terminal::{ColorChoice, Role};
use cli::{Cli, DisassemblySyntax, LaunchSettings, Renderers};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Native executable to launch, or the executable for --attach or --core
    /// when automatic discovery is unavailable.
    #[arg(value_name = "EXECUTABLE", required_unless_present_any = ["attach", "core"])]
    executable: Option<PathBuf>,

    /// Attach to an existing process. The executable is discovered through /proc by default.
    #[arg(short = 'p', long, value_name = "PID", conflicts_with = "core")]
    attach: Option<u64>,

    /// Open a post-mortem core dump. The executable recorded in the dump is used by default.
    #[arg(long, value_name = "CORE")]
    core: Option<PathBuf>,

    /// Look up the core dump's recorded module paths inside DIR, a copy of the
    /// files of the machine that wrote it, instead of on this machine.
    #[arg(long, value_name = "DIR", requires = "core")]
    sysroot: Option<PathBuf>,

    /// Search DIR for core dump modules missing from their recorded paths or
    /// not matching the dump, by file name and then by build-id. May be repeated.
    #[arg(long = "module-path", value_name = "DIR", requires = "core")]
    module_paths: Vec<PathBuf>,

    /// Read source files recorded under FROM from TO instead, such as for a
    /// program built elsewhere. May be repeated; earlier rules are tried first.
    #[arg(long, num_args = 2, value_names = ["FROM", "TO"])]
    source_map: Vec<PathBuf>,

    /// Use module files that cannot be proven to match the core dump.
    #[arg(long, requires = "core")]
    allow_module_mismatch: bool,

    /// Execute commands from a file. May be repeated.
    #[arg(short = 'c', long = "command", value_name = "FILE")]
    command_files: Vec<PathBuf>,

    /// Execute one command. May be repeated.
    #[arg(short = 'e', long = "eval", value_name = "COMMAND")]
    commands: Vec<String>,

    /// Execute commands without starting the interactive REPL.
    #[arg(long)]
    batch: bool,

    /// Control colored terminal output.
    #[arg(long, value_enum, default_value_t)]
    color: ColorChoice,

    /// The assembly syntax `disassemble` renders.
    #[arg(long, value_enum, default_value_t)]
    disassembly_syntax: DisassemblySyntax,

    /// Run the launched program in DIR instead of the current directory.
    #[arg(long, value_name = "DIR", conflicts_with_all = ["attach", "core"])]
    cwd: Option<PathBuf>,

    /// Set NAME to VALUE in the launched program's environment. May be repeated.
    #[arg(long = "env", value_name = "NAME=VALUE", value_parser = parse_environment_variable,
          conflicts_with_all = ["attach", "core"])]
    environment: Vec<(OsString, OsString)>,

    /// Arguments passed to the launched program.
    #[arg(last = true, value_name = "ARGS", conflicts_with_all = ["attach", "core"])]
    arguments: Vec<OsString>,
}

fn parse_environment_variable(text: &str) -> std::result::Result<(OsString, OsString), String> {
    match text.split_once('=') {
        Some((name, value)) if !name.is_empty() => Ok((name.into(), value.into())),
        _ => Err(format!("expected NAME=VALUE, found '{text}'")),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|command| command == "dap")
    {
        let args = dap::DapArgs::parse_from(std::env::args_os().skip(1));
        let code = match dap::run(args).await {
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
    let args = Args::parse();
    let renderers = Renderers::detect(args.color, args.batch);

    match run(&args, renderers).await {
        Ok(()) => ExitCode::SUCCESS,
        // A closed stdout pipe, such as `uscope ... | head`, is a normal end.
        Err(error) if is_broken_pipe(&error) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!(
                "{}: {error:#}",
                renderers.stderr.paint(Role::Error, "error")
            );
            ExitCode::FAILURE
        }
    }
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

fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<io::Error>())
        .any(|cause| cause.kind() == io::ErrorKind::BrokenPipe)
}
