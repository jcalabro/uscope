use std::fmt::Write as _;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::io::{AsyncBufReadExt, BufReader};
use uscope::{
    BreakpointLocation, BreakpointSpec, ByteOrder, Debugger, DebuggerHandle, Error, ExitStatus,
    RegisterSnapshot, SourceContext, StopReason, VirtualAddress,
};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Native executable to debug.
    #[arg(value_name = "EXECUTABLE")]
    executable: PathBuf,

    /// Execute commands from a file. May be repeated.
    #[arg(short = 'c', long = "command", value_name = "FILE")]
    command_files: Vec<PathBuf>,

    /// Execute one command. May be repeated.
    #[arg(short = 'e', long = "eval", value_name = "COMMAND")]
    commands: Vec<String>,

    /// Execute commands without starting the interactive REPL.
    #[arg(long)]
    batch: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let debugger = Debugger::new(&args.executable).with_context(|| {
        format!(
            "failed to initialize debugger for {}",
            args.executable.display()
        )
    })?;

    let handle = debugger.handle();
    let result = tokio::select! {
        result = run(&handle, &args) => result,
        signal = tokio::signal::ctrl_c() => signal.context("failed to listen for Ctrl-C"),
    };
    let shutdown = debugger
        .shutdown()
        .await
        .context("failed to shut down debugger");

    result?;
    shutdown?;

    Ok(())
}

async fn run(debugger: &DebuggerHandle, args: &Args) -> Result<()> {
    if !args.batch {
        println!("debugging {}", debugger.executable().display());
        io::stdout().flush()?;
    }

    for path in &args.command_files {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read command file {}", path.display()))?;

        if !run_lines(debugger, contents.lines(), &path.display().to_string()).await? {
            return Ok(());
        }
    }

    for (index, command) in args.commands.iter().enumerate() {
        if !run_line(debugger, command, &format!("--eval #{}", index + 1)).await? {
            return Ok(());
        }
    }

    if args.batch {
        if args.command_files.is_empty() && args.commands.is_empty() {
            let mut lines = BufReader::new(tokio::io::stdin()).lines();
            let mut number = 0_u64;

            while let Some(line) = lines.next_line().await? {
                number = number.checked_add(1).expect("stdin line number overflow");

                if !run_line(debugger, &line, &format!("stdin:{number}")).await? {
                    break;
                }
            }
        }

        Ok(())
    } else {
        repl(debugger).await
    }
}

async fn run_lines<'a>(
    debugger: &DebuggerHandle,
    lines: impl Iterator<Item = &'a str>,
    source: &str,
) -> Result<bool> {
    for (index, line) in lines.enumerate() {
        if !run_line(debugger, line, &format!("{source}:{}", index + 1)).await? {
            return Ok(false);
        }
    }

    Ok(true)
}

async fn run_line(debugger: &DebuggerHandle, line: &str, source: &str) -> Result<bool> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(true);
    }

    match execute(debugger, line)
        .await
        .with_context(|| source.to_owned())?
    {
        Control::Continue(message) => {
            if !message.is_empty() {
                println!("{message}");
                io::stdout().flush()?;
            }

            Ok(true)
        }
        Control::Quit => Ok(false),
    }
}

async fn repl(debugger: &DebuggerHandle) -> Result<()> {
    let show_prompt = io::stdin().is_terminal() && io::stdout().is_terminal();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut number = 0_u64;

    loop {
        if show_prompt {
            print!("> ");
            io::stdout().flush()?;
        }

        let Some(line) = lines.next_line().await? else {
            if show_prompt {
                println!();
            }
            return Ok(());
        };
        number = number.checked_add(1).expect("REPL line number overflow");

        match run_line(debugger, &line, &format!("repl:{number}")).await {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(error) => eprintln!("error: {error:#}"),
        }
    }
}

enum Control {
    Continue(String),
    Quit,
}

async fn execute(debugger: &DebuggerHandle, line: &str) -> uscope::Result<Control> {
    let mut words = line.split_whitespace();
    let command = words.next().unwrap_or("");

    match command {
        "break" | "b" => {
            let argument = one_argument(&mut words, "break <function|address>")?;
            let spec = parse_address(argument).map_or_else(
                |_| BreakpointSpec::Function(argument.to_owned()),
                |address| BreakpointSpec::Address(VirtualAddress::new(address)),
            );
            let location = debugger.add_breakpoint(spec).await?;
            let (space, address) = match location {
                BreakpointLocation::Image(address) => ("image", address.get()),
                BreakpointLocation::Virtual(address) => ("virtual", address.get()),
            };

            Ok(Control::Continue(format!(
                "breakpoint set at {space} address {address:#x}"
            )))
        }
        "run" | "r" => Ok(Control::Continue(
            format_stop_with_source(debugger, debugger.run().await?).await,
        )),
        "continue" | "c" => Ok(Control::Continue(
            format_stop_with_source(debugger, debugger.resume().await?).await,
        )),
        "x" => {
            let address = parse_address(one_argument(&mut words, "x <runtime-address>")?)?;

            Ok(Control::Continue(format!(
                "{address:#018x}: {:#018x}",
                debugger.read_word(VirtualAddress::new(address)).await?
            )))
        }
        "address" => {
            let name = one_argument(&mut words, "address <symbol>")?;

            Ok(Control::Continue(format!(
                "{name}: {:#x}",
                debugger.runtime_address(name).await?.get()
            )))
        }
        "where" => {
            let location = debugger.current_location().await?;
            let function = location
                .image
                .function
                .as_ref()
                .map_or("<unknown>", |function| function.name.as_ref());
            let source = location.image.source.as_ref().and_then(|source| {
                debugger
                    .module_image()
                    .source_file(source.file)
                    .map(|file| format!("{}:{}", file.path.display(), source.line.get()))
            });

            Ok(Control::Continue(match source {
                Some(source) => format!("{function} at {source} ({:#x})", location.address.get()),
                None => format!("{function} at {:#x}", location.address.get()),
            }))
        }
        "list" | "l" => Ok(Control::Continue(format_source_context(
            &debugger.source_context(3).await?,
        ))),
        "backtrace" | "bt" => {
            let trace = debugger.backtrace().await?;
            let mut lines = Vec::with_capacity(trace.frames.len() + 1);

            for frame in trace.frames.iter() {
                let name = frame
                    .function
                    .as_ref()
                    .map_or("<unknown>", |function| function.name.as_ref());
                let source = frame.source.as_ref().and_then(|source| {
                    debugger
                        .module_image()
                        .source_file(source.file)
                        .map(|file| format!(" at {}:{}", file.path.display(), source.line.get()))
                });
                lines.push(format!(
                    "#{:<2} {:#018x} in {name}{}",
                    frame.level,
                    frame.instruction.get(),
                    source.unwrap_or_default()
                ));
            }
            lines.push(format!("unwind stopped: {:?}", trace.termination));

            Ok(Control::Continue(lines.join("\n")))
        }
        "registers" | "regs" => {
            let registers = debugger.registers().await?;

            Ok(Control::Continue(format_registers(
                &registers,
                registers.target.byte_order,
            )))
        }
        "quit" | "q" => Ok(Control::Quit),
        "" => Ok(Control::Continue(String::new())),
        other => Err(Error::InvalidCommand(other.to_owned())),
    }
}

fn format_registers(registers: &RegisterSnapshot, byte_order: ByteOrder) -> String {
    let name_width = registers
        .registers
        .iter()
        .map(|value| value.register.name.len())
        .max()
        .unwrap_or(0);

    registers
        .registers
        .iter()
        .map(|value| {
            format!(
                "{:<name_width$} {}",
                value.register.name,
                format_register_bytes(&value.bytes, byte_order)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_register_bytes(bytes: &[u8], byte_order: ByteOrder) -> String {
    let mut output = String::with_capacity(2 + bytes.len() * 2);
    output.push_str("0x");

    match byte_order {
        ByteOrder::Little => {
            for byte in bytes.iter().rev() {
                write!(output, "{byte:02x}").expect("writing to a String cannot fail");
            }
        }
        ByteOrder::Big => {
            for byte in bytes {
                write!(output, "{byte:02x}").expect("writing to a String cannot fail");
            }
        }
    }

    output
}

async fn format_stop_with_source(debugger: &DebuggerHandle, reason: StopReason) -> String {
    let stopped_at_breakpoint = matches!(reason, StopReason::Breakpoint { .. });
    let mut output = format_stop(reason);

    if stopped_at_breakpoint {
        match debugger.source_context(3).await {
            Ok(context) => {
                output.push('\n');
                output.push_str(&format_source_context(&context));
            }
            Err(error) => {
                write!(output, "\nsource unavailable: {error}")
                    .expect("writing to a String cannot fail");
            }
        }
    }

    output
}

fn format_source_context(context: &SourceContext) -> String {
    let line_width = context
        .lines
        .last()
        .map_or(1, |line| line.number.get().to_string().len());
    let mut output = format!(
        "{}:{}",
        context.file.path.display(),
        context.location.line.get()
    );

    for line in context.lines.iter() {
        let marker = if line.number == context.location.line {
            "=>"
        } else {
            "  "
        };
        write!(
            output,
            "\n{marker} {:>line_width$} | {}",
            line.number.get(),
            line.text
        )
        .expect("writing to a String cannot fail");
    }

    output
}

fn one_argument<'a>(
    words: &mut impl Iterator<Item = &'a str>,
    usage: &str,
) -> uscope::Result<&'a str> {
    let argument = words
        .next()
        .ok_or_else(|| Error::InvalidCommand(usage.to_owned()))?;

    if words.next().is_some() {
        return Err(Error::InvalidCommand(usage.to_owned()));
    }

    Ok(argument)
}

fn parse_address(value: &str) -> uscope::Result<u64> {
    let value = value.strip_prefix("0x").unwrap_or(value);

    u64::from_str_radix(value, 16)
        .map_err(|_| Error::InvalidCommand(format!("invalid hexadecimal address: {value}")))
}

fn format_stop(reason: StopReason) -> String {
    match reason {
        StopReason::Breakpoint { address } => {
            format!("stopped at breakpoint {:#x}", address.get())
        }
        StopReason::Exception(exception) => format!(
            "stopped by {} ({:#x})",
            exception.description, exception.code
        ),
        StopReason::Exited(ExitStatus::Code(code)) => {
            format!("inferior exited with status {code}")
        }
        StopReason::Exited(ExitStatus::Terminated(exception)) => format!(
            "inferior terminated by {} ({:#x})",
            exception.description, exception.code
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_bytes_are_rendered_in_target_byte_order() {
        assert_eq!(
            format_register_bytes(&[0x78, 0x56, 0x34, 0x12], ByteOrder::Little),
            "0x12345678"
        );
        assert_eq!(
            format_register_bytes(&[0x12, 0x34, 0x56, 0x78], ByteOrder::Big),
            "0x12345678"
        );
    }
}
