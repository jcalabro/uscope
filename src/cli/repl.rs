//! The interactive line-editing REPL.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

use anyhow::{Context as _, Result, anyhow};
use rustyline::config::Configurer as _;
use rustyline::error::ReadlineError;
use rustyline::{ColorMode, DefaultEditor};

use super::commands::command_named;
use super::config::state_directory;
use super::terminal::Role;
use super::{Cli, Renderers};

enum Input {
    /// A line, and the terminal's width when it was entered.
    Line(String, Option<usize>),
    Eof,
    Failed(String),
}

/// Runs the REPL until `quit` or end-of-input.
///
/// Rustyline blocks, so it runs on its own thread. After sending each line
/// the editor waits for an acknowledgement, which keeps it from reading the
/// terminal while a command, and possibly the inferior, is running.
pub async fn run(cli: &Cli) -> Result<()> {
    let (input_sender, mut inputs) = tokio::sync::mpsc::channel(1);
    let (ack_sender, acknowledgements) = mpsc::channel::<bool>();
    let renderers = cli.renderers;
    let prompt = cli.settings.config.ui.prompt.clone();
    let history_size = cli.settings.config.history.size;
    let editor = thread::Builder::new()
        .name("uscope-line-editor".to_owned())
        .spawn(move || {
            line_editor(
                &input_sender,
                &acknowledgements,
                renderers,
                &prompt,
                history_size,
            );
        })?;

    let mut outcome = Ok(());
    let mut last_repeatable = None;
    while let Some(input) = inputs.recv().await {
        let keep_running = match input {
            Input::Line(text, columns) => {
                if let Some(columns) = columns {
                    cli.set_columns(columns);
                }
                let entered = text.trim();
                // An empty line repeats the last repeatable command.
                let command = if entered.is_empty() {
                    last_repeatable.clone().unwrap_or_default()
                } else {
                    let expanded = cli.expand_alias(entered);
                    let repeatable = expanded
                        .as_deref()
                        .unwrap_or(entered)
                        .split_whitespace()
                        .next()
                        .and_then(command_named)
                        .is_some_and(|spec| spec.repeatable);
                    last_repeatable = repeatable.then(|| entered.to_owned());
                    entered.to_owned()
                };
                cli.run_line(&command).await.unwrap_or_else(|error| {
                    cli.report_error(&error);
                    true
                })
            }
            Input::Eof => false,
            Input::Failed(error) => {
                outcome = Err(anyhow!(error));
                break;
            }
        };
        if ack_sender.send(keep_running).is_err() {
            outcome = Err(anyhow!(
                "line editor stopped before command acknowledgement"
            ));
            break;
        }
        if !keep_running {
            break;
        }
    }

    tokio::task::spawn_blocking(move || editor.join())
        .await
        .context("failed to join line editor task")?
        .map_err(|_| anyhow!("line editor thread panicked"))?;
    outcome
}

fn line_editor(
    input: &tokio::sync::mpsc::Sender<Input>,
    acknowledgements: &mpsc::Receiver<bool>,
    renderers: Renderers,
    prompt: &str,
    history_size: u32,
) {
    let warn = |message: String| {
        eprintln!(
            "{}: {message}",
            renderers.stderr.paint(Role::Warning, "warning")
        );
    };
    let mut editor = match DefaultEditor::new() {
        Ok(editor) => editor,
        Err(error) => {
            let _ = input.blocking_send(Input::Failed(error.to_string()));
            return;
        }
    };
    // Rustyline needs a helper to select the styled prompt. The unit helper's
    // line highlighter is a no-op, so command input remains unstyled.
    editor.set_helper(Some(()));
    editor.set_color_mode(if renderers.stdout.is_colored() {
        ColorMode::Forced
    } else {
        ColorMode::Disabled
    });
    let history = history_path(state_directory());
    let keep = usize::try_from(history_size).unwrap_or(usize::MAX);
    if let Err(error) = editor.set_max_history_size(keep) {
        warn(format!("failed to limit command history: {error}"));
    }
    if keep != 0
        && history.exists()
        && let Err(error) = editor.load_history(&history)
    {
        warn(format!(
            "failed to load command history {}: {error}",
            history.display()
        ));
    }
    let styled_prompt = renderers.stdout.paint(Role::Prompt, prompt).to_string();
    loop {
        match editor.readline(&(prompt, &styled_prompt)) {
            Ok(line) => {
                if !line.trim().is_empty()
                    && let Err(error) = editor.add_history_entry(line.as_str())
                {
                    warn(format!("failed to record command history: {error}"));
                }
                let columns = editor.dimensions().map(|(columns, _)| usize::from(columns));
                if input.blocking_send(Input::Line(line, columns)).is_err()
                    || !acknowledgements.recv().unwrap_or(false)
                {
                    break;
                }
            }
            Err(ReadlineError::Interrupted) => {}
            Err(ReadlineError::Eof) => {
                let _ = input.blocking_send(Input::Eof);
                let _ = acknowledgements.recv();
                break;
            }
            Err(error) => {
                let _ = input.blocking_send(Input::Failed(error.to_string()));
                break;
            }
        }
    }
    if keep != 0
        && let Err(error) = persist_history(&mut editor, &history)
    {
        warn(error);
    }
}

/// Locates the history file in the user's state directory, falling back to
/// the current directory.
fn history_path(state: Option<PathBuf>) -> PathBuf {
    state.map_or_else(
        || PathBuf::from(".uscope_history"),
        |state| state.join("history"),
    )
}

fn persist_history(editor: &mut DefaultEditor, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "failed to create history directory {}: {error}",
                parent.display()
            )
        })?;
    }
    if path.exists() {
        editor.append_history(path)
    } else {
        editor.save_history(path)
    }
    .map_err(|error| format!("failed to save command history {}: {error}", path.display()))
}
