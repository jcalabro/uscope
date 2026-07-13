use std::ffi::{OsStr, OsString};
use std::fmt;

use anstyle::{AnsiColor, Style};
use clap::ValueEnum;

/// User-selected color behavior for terminal output.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum ColorChoice {
    /// Detect color support from the output stream and environment.
    #[default]
    Auto,
    /// Emit ANSI color even when output is redirected.
    Always,
    /// Never emit ANSI color.
    Never,
}

#[derive(Debug)]
pub struct ColorEnvironment {
    term: Option<OsString>,
    no_color: bool,
    clicolor: Option<bool>,
    clicolor_force: bool,
}

impl ColorEnvironment {
    pub(crate) fn current() -> Self {
        Self {
            term: std::env::var_os("TERM"),
            no_color: anstyle_query::no_color(),
            clicolor: anstyle_query::clicolor(),
            clicolor_force: anstyle_query::clicolor_force(),
        }
    }
}

/// Determines color support without consulting global state, keeping the
/// precedence rules deterministic and independently testable.
pub fn color_enabled(
    choice: ColorChoice,
    environment: &ColorEnvironment,
    is_terminal: bool,
    batch: bool,
) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            if environment.no_color {
                return false;
            }
            if environment.clicolor_force {
                return true;
            }
            if environment.clicolor == Some(false) || batch || !is_terminal {
                return false;
            }
            environment.clicolor == Some(true)
                || environment.term.as_deref().is_some_and(term_supports_color)
        }
    }
}

fn term_supports_color(term: &OsStr) -> bool {
    !term.is_empty() && term != "dumb"
}

/// Determines whether cursor-control sequences are safe for the output stream.
pub fn terminal_control_enabled(environment: &ColorEnvironment, is_terminal: bool) -> bool {
    is_terminal && environment.term.as_deref().is_some_and(term_supports_color)
}

/// Semantic presentation roles shared by all terminal formatters.
#[derive(Clone, Copy, Debug)]
pub enum Role {
    Prompt,
    Command,
    Alias,
    Muted,
    Name,
    Type,
    Value,
    Metadata,
    Current,
    Success,
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug)]
pub struct Renderer {
    color: bool,
}

impl Renderer {
    pub(crate) const fn new(color: bool) -> Self {
        Self { color }
    }

    pub(crate) const fn is_colored(self) -> bool {
        self.color
    }

    pub(crate) fn paint<T>(self, role: Role, value: T) -> Painted<T> {
        Painted {
            style: self.color.then(|| style(role)),
            value,
        }
    }
}

pub struct Painted<T> {
    style: Option<Style>,
    value: T,
}

impl<T: fmt::Display> fmt::Display for Painted<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(style) = self.style {
            write!(formatter, "{style}{}{style:#}", self.value)
        } else {
            self.value.fmt(formatter)
        }
    }
}

fn style(role: Role) -> Style {
    match role {
        Role::Prompt | Role::Muted => return Style::new().dimmed(),
        Role::Command => {
            return Style::new()
                .fg_color(Some(AnsiColor::BrightBlue.into()))
                .bold();
        }
        _ => {}
    }

    let color = match role {
        Role::Name => AnsiColor::BrightCyan,
        Role::Alias => AnsiColor::BrightBlue,
        Role::Type => AnsiColor::BrightMagenta,
        Role::Metadata => AnsiColor::Cyan,
        Role::Current | Role::Warning => AnsiColor::BrightYellow,
        Role::Value | Role::Success => AnsiColor::BrightGreen,
        Role::Error => AnsiColor::BrightRed,
        Role::Prompt | Role::Command | Role::Muted => {
            unreachable!("non-color style returned above")
        }
    };

    let style = Style::new().fg_color(Some(color.into()));
    match role {
        Role::Current | Role::Error => style.bold(),
        _ => style,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(
        term: Option<&str>,
        no_color: bool,
        clicolor: Option<bool>,
        clicolor_force: bool,
    ) -> ColorEnvironment {
        ColorEnvironment {
            term: term.map(OsString::from),
            no_color,
            clicolor,
            clicolor_force,
        }
    }

    #[test]
    fn color_policy_has_deterministic_precedence() {
        let capable = environment(Some("xterm-256color"), false, None, false);
        assert!(color_enabled(ColorChoice::Auto, &capable, true, false));
        assert!(!color_enabled(ColorChoice::Auto, &capable, false, false));
        assert!(!color_enabled(ColorChoice::Auto, &capable, true, true));

        let dumb = environment(Some("dumb"), false, None, false);
        assert!(!color_enabled(ColorChoice::Auto, &dumb, true, false));
        let unknown = environment(None, false, None, false);
        assert!(!color_enabled(ColorChoice::Auto, &unknown, true, false));

        let disabled = environment(Some("xterm"), false, Some(false), false);
        assert!(!color_enabled(ColorChoice::Auto, &disabled, true, false));
        let requested = environment(None, false, Some(true), false);
        assert!(color_enabled(ColorChoice::Auto, &requested, true, false));

        let forced = environment(None, false, None, true);
        assert!(color_enabled(ColorChoice::Auto, &forced, false, true));
        let no_color = environment(Some("xterm"), true, Some(true), true);
        assert!(!color_enabled(ColorChoice::Auto, &no_color, true, false));

        assert!(color_enabled(ColorChoice::Always, &no_color, false, true));
        assert!(!color_enabled(ColorChoice::Never, &capable, true, false));
    }

    #[test]
    fn renderer_preserves_plain_text_and_bounds_every_style() {
        for role in [
            Role::Prompt,
            Role::Command,
            Role::Alias,
            Role::Muted,
            Role::Name,
            Role::Type,
            Role::Value,
            Role::Metadata,
            Role::Current,
            Role::Success,
            Role::Warning,
            Role::Error,
        ] {
            assert_eq!(Renderer::new(false).paint(role, "text").to_string(), "text");

            let rendered = Renderer::new(true).paint(role, "text").to_string();
            assert!(rendered.starts_with("\x1b["), "{role:?}: {rendered:?}");
            assert!(rendered.ends_with("\x1b[0m"), "{role:?}: {rendered:?}");
            assert!(rendered.contains("text"), "{role:?}: {rendered:?}");
        }
    }

    #[test]
    fn terminal_control_requires_an_ansi_terminal() {
        let capable = environment(Some("xterm-256color"), false, None, false);
        assert!(terminal_control_enabled(&capable, true));
        assert!(!terminal_control_enabled(&capable, false));

        let dumb = environment(Some("dumb"), false, None, false);
        assert!(!terminal_control_enabled(&dumb, true));
        let unknown = environment(None, false, None, false);
        assert!(!terminal_control_enabled(&unknown, true));
    }
}
