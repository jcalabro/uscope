use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::io::{AsyncBufReadExt, BufReader};
use uscope::{
    BreakpointLocation, BreakpointSpec, Debugger, DebuggerHandle, Error, ExitStatus, StopReason,
    VirtualAddress,
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

    /// Print plain-text results without opening the terminal UI.
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

    let result = run(&debugger.handle(), &args).await;
    let shutdown = debugger
        .shutdown()
        .await
        .context("failed to shut down debugger");

    result?;
    shutdown?;

    Ok(())
}

async fn run(debugger: &DebuggerHandle, args: &Args) -> Result<()> {
    let mut output = vec![format!("debugging {}", debugger.executable().display())];

    for path in &args.command_files {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read command file {}", path.display()))?;

        if !run_lines(
            debugger,
            contents.lines(),
            &path.display().to_string(),
            args.batch,
            &mut output,
        )
        .await?
        {
            return Ok(());
        }
    }

    for (index, command) in args.commands.iter().enumerate() {
        if !run_line(
            debugger,
            command,
            &format!("--eval #{}", index + 1),
            args.batch,
            &mut output,
        )
        .await?
        {
            return Ok(());
        }
    }

    if args.batch {
        if args.command_files.is_empty() && args.commands.is_empty() {
            let mut lines = BufReader::new(tokio::io::stdin()).lines();
            let mut number = 0;

            while let Some(line) = lines.next_line().await? {
                number += 1;

                if !run_line(
                    debugger,
                    &line,
                    &format!("stdin:{number}"),
                    true,
                    &mut output,
                )
                .await?
                {
                    break;
                }
            }
        }

        Ok(())
    } else {
        repl(debugger, output).await
    }
}

async fn run_lines<'a>(
    debugger: &DebuggerHandle,
    lines: impl Iterator<Item = &'a str>,
    source: &str,
    batch: bool,
    output: &mut Vec<String>,
) -> Result<bool> {
    for (index, line) in lines.enumerate() {
        if !run_line(
            debugger,
            line,
            &format!("{source}:{}", index + 1),
            batch,
            output,
        )
        .await?
        {
            return Ok(false);
        }
    }

    Ok(true)
}

async fn run_line(
    debugger: &DebuggerHandle,
    line: &str,
    source: &str,
    batch: bool,
    output: &mut Vec<String>,
) -> Result<bool> {
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
                if batch {
                    println!("{message}");
                    io::stdout().flush()?;
                } else {
                    output.push(format!("> {line}"));
                    output.push(message);
                }
            }

            Ok(true)
        }
        Control::Quit => Ok(false),
    }
}

async fn repl(debugger: &DebuggerHandle, output: Vec<String>) -> Result<()> {
    enable_raw_mode().context("failed to enable terminal raw mode")?;

    let backend = CrosstermBackend::new(io::stdout());
    let options = TerminalOptions {
        viewport: Viewport::Inline(12),
    };
    let mut terminal =
        Terminal::with_options(backend, options).context("failed to initialize terminal")?;

    let result = run_repl(&mut terminal, debugger, output).await;

    disable_raw_mode().context("failed to disable terminal raw mode")?;
    terminal.show_cursor().context("failed to restore cursor")?;

    result
}

async fn run_repl(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    debugger: &DebuggerHandle,
    mut output: Vec<String>,
) -> Result<()> {
    let mut input = String::new();

    loop {
        terminal.draw(|frame| {
            let [history, prompt] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(frame.area());
            let visible = history.height.saturating_sub(2) as usize;
            let start = output.len().saturating_sub(visible);

            let lines: Vec<Line<'_>> = output[start..]
                .iter()
                .map(String::as_str)
                .map(Line::from)
                .collect();

            frame.render_widget(
                Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("uscope")),
                history,
            );

            frame.render_widget(
                Paragraph::new(format!("> {input}")).block(Block::default().borders(Borders::ALL)),
                prompt,
            );
            frame.set_cursor_position((prompt_cursor_x(prompt, input.len()), prompt.y + 1));
        })?;

        let Event::Key(key) = event::read()? else {
            continue;
        };

        if key.kind != KeyEventKind::Press {
            continue;
        }

        match key.code {
            KeyCode::Char('c' | 'd') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(());
            }
            KeyCode::Char(character) => input.push(character),
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Enter => {
                let command = std::mem::take(&mut input);

                match run_line(debugger, &command, "repl", false, &mut output).await {
                    Ok(true) => {}
                    Ok(false) => return Ok(()),
                    Err(error) => output.push(format!("error: {error}")),
                }
            }
            _ => {}
        }
    }
}

fn prompt_cursor_x(prompt: Rect, input_len: usize) -> u16 {
    let input_width = u16::try_from(input_len).unwrap_or(u16::MAX);

    prompt
        .x
        .saturating_add(3)
        .saturating_add(input_width)
        .min(prompt.right().saturating_sub(2))
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
        "run" | "r" => Ok(Control::Continue(format_stop(debugger.run().await?))),
        "continue" | "c" => Ok(Control::Continue(format_stop(debugger.resume().await?))),
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
        "quit" | "q" => Ok(Control::Quit),
        "" => Ok(Control::Continue(String::new())),
        other => Err(Error::InvalidCommand(other.to_owned())),
    }
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
    fn prompt_cursor_follows_input_and_stays_inside_border() {
        let prompt = Rect::new(10, 0, 20, 3);

        assert_eq!(prompt_cursor_x(prompt, 0), 13);
        assert_eq!(prompt_cursor_x(prompt, 5), 18);
        assert_eq!(prompt_cursor_x(prompt, usize::MAX), 28);
    }
}
