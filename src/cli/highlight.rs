//! Syntax highlighting for source listings: a small lexer per language that
//! finds keywords, strings, comments, and numbers. A file is lexed from its
//! start, so a comment or string that opened above the lines shown is
//! still known.

use std::ops::Range;
use std::path::Path;

use super::terminal::{Renderer, Role};

/// A language the lexer knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    C,
    Cpp,
    Rust,
    Go,
    Zig,
}

const C_KEYWORDS: &[&str] = &[
    "_Alignas",
    "_Alignof",
    "_Atomic",
    "_Bool",
    "_Noreturn",
    "_Static_assert",
    "_Thread_local",
    "auto",
    "bool",
    "break",
    "case",
    "char",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extern",
    "false",
    "float",
    "for",
    "goto",
    "if",
    "inline",
    "int",
    "long",
    "register",
    "restrict",
    "return",
    "short",
    "signed",
    "sizeof",
    "static",
    "struct",
    "switch",
    "true",
    "typedef",
    "union",
    "unsigned",
    "void",
    "volatile",
    "while",
];
const CPP_KEYWORDS: &[&str] = &[
    "alignas",
    "alignof",
    "asm",
    "auto",
    "bool",
    "break",
    "case",
    "catch",
    "char",
    "char8_t",
    "char16_t",
    "char32_t",
    "class",
    "co_await",
    "co_return",
    "co_yield",
    "concept",
    "const",
    "const_cast",
    "consteval",
    "constexpr",
    "constinit",
    "continue",
    "decltype",
    "default",
    "delete",
    "do",
    "double",
    "dynamic_cast",
    "else",
    "enum",
    "explicit",
    "export",
    "extern",
    "false",
    "final",
    "float",
    "for",
    "friend",
    "goto",
    "if",
    "inline",
    "int",
    "long",
    "mutable",
    "namespace",
    "new",
    "noexcept",
    "nullptr",
    "operator",
    "override",
    "private",
    "protected",
    "public",
    "register",
    "reinterpret_cast",
    "requires",
    "return",
    "short",
    "signed",
    "sizeof",
    "static",
    "static_assert",
    "static_cast",
    "struct",
    "switch",
    "template",
    "this",
    "thread_local",
    "throw",
    "true",
    "try",
    "typedef",
    "typeid",
    "typename",
    "union",
    "unsigned",
    "using",
    "virtual",
    "void",
    "volatile",
    "wchar_t",
    "while",
];
const RUST_KEYWORDS: &[&str] = &[
    "Self", "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
    "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
    "mut", "pub", "ref", "return", "self", "static", "struct", "super", "trait", "true", "type",
    "union", "unsafe", "use", "where", "while", "yield",
];
const GO_KEYWORDS: &[&str] = &[
    "break",
    "case",
    "chan",
    "const",
    "continue",
    "default",
    "defer",
    "else",
    "fallthrough",
    "false",
    "for",
    "func",
    "go",
    "goto",
    "if",
    "import",
    "interface",
    "iota",
    "map",
    "nil",
    "package",
    "range",
    "return",
    "select",
    "struct",
    "switch",
    "true",
    "type",
    "var",
];
const ZIG_KEYWORDS: &[&str] = &[
    "addrspace",
    "align",
    "allowzero",
    "and",
    "anyframe",
    "anytype",
    "asm",
    "break",
    "callconv",
    "catch",
    "comptime",
    "const",
    "continue",
    "defer",
    "else",
    "enum",
    "errdefer",
    "error",
    "export",
    "extern",
    "false",
    "fn",
    "for",
    "if",
    "inline",
    "linksection",
    "noalias",
    "noinline",
    "nosuspend",
    "null",
    "opaque",
    "or",
    "orelse",
    "packed",
    "pub",
    "resume",
    "return",
    "struct",
    "suspend",
    "switch",
    "test",
    "threadlocal",
    "true",
    "try",
    "undefined",
    "union",
    "unreachable",
    "usingnamespace",
    "var",
    "volatile",
    "while",
];

impl Language {
    /// The language a file's extension names. A `.h` header is C's.
    pub fn of(path: &Path) -> Option<Self> {
        Some(match path.extension()?.to_str()? {
            "c" | "h" => Self::C,
            "cc" | "cpp" | "cxx" | "c++" | "hh" | "hpp" | "hxx" | "h++" | "ipp" | "tcc" => {
                Self::Cpp
            }
            "rs" => Self::Rust,
            "go" => Self::Go,
            "zig" => Self::Zig,
            _ => return None,
        })
    }

    const fn keywords(self) -> &'static [&'static str] {
        match self {
            Self::C => C_KEYWORDS,
            Self::Cpp => CPP_KEYWORDS,
            Self::Rust => RUST_KEYWORDS,
            Self::Go => GO_KEYWORDS,
            Self::Zig => ZIG_KEYWORDS,
        }
    }
}

/// What a span of source is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Keyword,
    String,
    Comment,
    Number,
}

/// A highlighted byte range of one line.
pub type Span = (Range<usize>, Kind);

/// `text` with its `spans` painted and its tabs expanded to stops every
/// `tab_width` columns, or kept with a width of 0.
pub fn render(text: &str, spans: &[Span], tab_width: usize, renderer: Renderer) -> String {
    let mut output = String::with_capacity(text.len());
    let mut column = 0;
    let expand = |segment: &str, column: &mut usize| {
        let mut expanded = String::with_capacity(segment.len());
        for character in segment.chars() {
            if character == '\t' && tab_width != 0 {
                let spaces = tab_width - *column % tab_width;
                expanded.extend(std::iter::repeat_n(' ', spaces));
                *column += spaces;
            } else {
                expanded.push(character);
                *column += 1;
            }
        }
        expanded
    };
    let mut position = 0;
    for (range, kind) in spans {
        let range = range.start.min(text.len())..range.end.min(text.len());
        if range.start < position
            || !text.is_char_boundary(range.start)
            || !text.is_char_boundary(range.end)
        {
            continue;
        }
        output.push_str(&expand(&text[position..range.start], &mut column));
        let role = match kind {
            Kind::Keyword => Role::Keyword,
            Kind::String => Role::String,
            Kind::Comment => Role::Comment,
            Kind::Number => Role::Number,
        };
        let segment = expand(&text[range.clone()], &mut column);
        output.push_str(&renderer.paint(role, segment).to_string());
        position = range.end;
    }
    output.push_str(&expand(&text[position..], &mut column));
    output
}

const fn is_identifier(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Lexes `text` from its start and returns each line's spans, by line.
pub fn lex(text: &str, language: Language) -> Vec<Vec<Span>> {
    let tokens = tokens(text.as_bytes(), language);
    // Each token is split at the lines it spans.
    let mut lines = vec![Vec::new()];
    let mut line_start = 0;
    let mut tokens = tokens.into_iter().peekable();
    for (index, byte) in text.bytes().chain(std::iter::once(b'\n')).enumerate() {
        if byte != b'\n' {
            continue;
        }
        let line = line_start..index;
        while let Some((range, kind)) = tokens.peek().cloned() {
            if range.start >= line.end {
                break;
            }
            let start = range.start.max(line.start);
            let end = range.end.min(line.end);
            if start < end {
                lines
                    .last_mut()
                    .expect("there is a line")
                    .push((start - line.start..end - line.start, kind));
            }
            if range.end <= line.end {
                tokens.next();
            } else {
                break;
            }
        }
        lines.push(Vec::new());
        line_start = index + 1;
    }
    lines.pop();
    lines
}

/// The highlighted tokens of `text`, in order, by byte range.
fn tokens(text: &[u8], language: Language) -> Vec<Span> {
    let at = |index: usize| text.get(index).copied().unwrap_or(0);
    let starts = |index: usize, prefix: &[u8]| text[index.min(text.len())..].starts_with(prefix);
    let mut tokens = Vec::new();
    let mut index = 0;
    // Whether only whitespace precedes `index` on its line.
    let mut line_start = true;
    while index < text.len() {
        let byte = text[index];
        let previous_is_identifier = index > 0 && is_identifier(text[index - 1]);
        let start = index;
        let kind = if starts(index, b"//") {
            index = memchr(text, index, b'\n');
            Some(Kind::Comment)
        } else if starts(index, b"/*") && language != Language::Zig {
            index = block_comment_end(text, index, language == Language::Rust);
            Some(Kind::Comment)
        } else if language == Language::Zig && starts(index, b"\\\\") {
            index = memchr(text, index, b'\n');
            Some(Kind::String)
        } else if language == Language::Go && byte == b'`' {
            index = memchr(text, index + 1, b'`')
                .saturating_add(1)
                .min(text.len());
            Some(Kind::String)
        } else if let Some(end) = (!previous_is_identifier)
            .then(|| raw_string_end(text, index, language))
            .flatten()
        {
            index = end;
            Some(Kind::String)
        } else if byte == b'"' {
            index = quoted_end(text, index, b'"', language == Language::Rust);
            Some(Kind::String)
        } else if byte == b'\'' {
            if let Some(end) = character_end(text, index, language) {
                index = end;
                Some(Kind::String)
            } else {
                // A Rust lifetime or label.
                index += 1;
                None
            }
        } else if byte.is_ascii_digit() && !previous_is_identifier {
            index = number_end(text, index);
            Some(Kind::Number)
        } else if is_identifier(byte) {
            while is_identifier(at(index)) {
                index += 1;
            }
            let word = std::str::from_utf8(&text[start..index]).unwrap_or_default();
            language.keywords().contains(&word).then_some(Kind::Keyword)
        } else if byte == b'#' && line_start && matches!(language, Language::C | Language::Cpp) {
            // A preprocessor directive's name.
            index += 1;
            while matches!(at(index), b' ' | b'\t') {
                index += 1;
            }
            while is_identifier(at(index)) {
                index += 1;
            }
            Some(Kind::Keyword)
        } else {
            index += 1;
            None
        };
        if let Some(kind) = kind {
            tokens.push((start..index, kind));
        }
        line_start = match text[index - 1] {
            b'\n' => true,
            b' ' | b'\t' => line_start,
            _ => false,
        };
    }
    tokens
}

/// The index of the first `byte` at or after `from`, or the end.
fn memchr(text: &[u8], from: usize, byte: u8) -> usize {
    text[from.min(text.len())..]
        .iter()
        .position(|candidate| *candidate == byte)
        .map_or(text.len(), |offset| from + offset)
}

/// Where a block comment opened at `start` ends; Rust's nest.
fn block_comment_end(text: &[u8], start: usize, nests: bool) -> usize {
    let mut depth = 0;
    let mut index = start;
    while index < text.len() {
        if text[index..].starts_with(b"/*") && (nests || depth == 0) {
            depth += 1;
            index += 2;
        } else if text[index..].starts_with(b"*/") {
            depth -= 1;
            index += 2;
            if depth == 0 {
                return index;
            }
        } else {
            index += 1;
        }
    }
    text.len()
}

/// Where a string or character quoted by `quote` from `start` ends, after
/// its closing quote. Only Rust's strings continue past a line's end
/// without an escaped newline.
const fn quoted_end(text: &[u8], start: usize, quote: u8, spans_lines: bool) -> usize {
    let mut index = start + 1;
    while index < text.len() {
        match text[index] {
            b'\\' => index += 2,
            b'\n' if !spans_lines => return index,
            byte if byte == quote => return index + 1,
            _ => index += 1,
        }
    }
    text.len()
}

/// Where a character literal from `start` ends, or `None` for a Rust
/// lifetime, which also begins with a quote.
fn character_end(text: &[u8], start: usize, language: Language) -> Option<usize> {
    if language != Language::Rust {
        return Some(quoted_end(text, start, b'\'', false));
    }
    if text.get(start + 1) == Some(&b'\\') {
        return Some(quoted_end(text, start, b'\'', false));
    }
    // One character, which may take several bytes, then the closing quote.
    let lead = *text.get(start + 1)?;
    let width = match lead {
        b'\'' | b'\n' => return None,
        0..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return None,
    };
    let after = start + 1 + width;
    (text.get(after) == Some(&b'\'')).then_some(after + 1)
}

/// Where a raw string from `start` ends, when one begins there: Rust's
/// `r#"…"#` and `br"…"`, and C++'s `R"delimiter(…)delimiter"`.
fn raw_string_end(text: &[u8], start: usize, language: Language) -> Option<usize> {
    match language {
        Language::Rust => {
            let mut index = start;
            if text.get(index) == Some(&b'b') {
                index += 1;
            }
            if text.get(index) != Some(&b'r') {
                return None;
            }
            index += 1;
            let hashes = text[index..]
                .iter()
                .take_while(|byte| **byte == b'#')
                .count();
            index += hashes;
            if text.get(index) != Some(&b'"') {
                return None;
            }
            let mut closing = vec![b'"'];
            closing.extend(std::iter::repeat_n(b'#', hashes));
            find(text, index + 1, &closing).map(|end| end + closing.len())
        }
        Language::Cpp => {
            let prefix = ["u8R\"", "uR\"", "UR\"", "LR\"", "R\""]
                .into_iter()
                .find(|prefix| text[start..].starts_with(prefix.as_bytes()))?;
            let open = start + prefix.len();
            let delimiter_end = open
                + text[open..]
                    .iter()
                    .take(17)
                    .position(|byte| *byte == b'(')?;
            let mut closing = vec![b')'];
            closing.extend_from_slice(&text[open..delimiter_end]);
            closing.push(b'"');
            Some(
                find(text, delimiter_end + 1, &closing)
                    .map_or(text.len(), |end| end + closing.len()),
            )
        }
        _ => None,
    }
}

/// The index at or after `from` where `needle` begins, if it does.
fn find(text: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    text[from.min(text.len())..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

/// Where a number from `start` ends: digits, letters of a radix, suffix, or
/// exponent, and a point followed by a digit, so that a range `0..5` is
/// two numbers.
fn number_end(text: &[u8], start: usize) -> usize {
    let hexadecimal = text[start..].starts_with(b"0x") || text[start..].starts_with(b"0X");
    let mut index = start;
    while index < text.len() {
        let byte = text[index];
        let exponent_sign =
            matches!(byte, b'+' | b'-') && !hexadecimal && matches!(text[index - 1], b'e' | b'E');
        let point = byte == b'.' && text.get(index + 1).is_some_and(u8::is_ascii_digit);
        if is_identifier(byte)
            || byte == b'\'' && text.get(index + 1).is_some_and(u8::is_ascii_digit)
            || exponent_sign
            || point
        {
            index += 1;
        } else {
            break;
        }
    }
    index
}

#[cfg(test)]
mod tests {
    use super::{Kind, Language, lex};

    /// Each line's highlighted text and its kind.
    fn shown(text: &str, language: Language) -> Vec<Vec<(String, Kind)>> {
        lex(text, language)
            .into_iter()
            .zip(text.split('\n'))
            .map(|(spans, line)| {
                spans
                    .into_iter()
                    .map(|(range, kind)| (line[range].to_owned(), kind))
                    .collect()
            })
            .collect()
    }

    fn of(spans: &[(&str, Kind)]) -> Vec<(String, Kind)> {
        spans
            .iter()
            .map(|(text, kind)| ((*text).to_owned(), *kind))
            .collect()
    }

    #[test]
    fn comments_and_strings_that_span_lines_are_known_on_every_line() {
        use Kind::{Comment, Keyword, Number, String};
        let c =
            "#include <stdio.h>\n/* one\n   two */ int x = 0x1f; // done\nchar *s = \"a\\\"b\";";
        assert_eq!(
            shown(c, Language::C),
            [
                of(&[("#include", Keyword)]),
                of(&[("/* one", Comment)]),
                of(&[
                    ("   two */", Comment),
                    ("int", Keyword),
                    ("0x1f", Number),
                    ("// done", Comment),
                ]),
                of(&[("char", Keyword), ("\"a\\\"b\"", String)]),
            ]
        );

        // Rust's comments nest, its strings span lines, and a quote also
        // begins a lifetime.
        let rust = "/* a /* b */ c */ fn f<'a>(x: &'a u8) -> char {\n    let s = \"one\ntwo\";\n    let r = r#\"x\"y\"#; for i in 0..5 {} 'q'\n}";
        assert_eq!(
            shown(rust, Language::Rust),
            [
                of(&[("/* a /* b */ c */", Comment), ("fn", Keyword)]),
                of(&[("let", Keyword), ("\"one", String)]),
                of(&[("two\"", String)]),
                of(&[
                    ("let", Keyword),
                    ("r#\"x\"y\"#", String),
                    ("for", Keyword),
                    ("in", Keyword),
                    ("0", Number),
                    ("5", Number),
                    ("'q'", String),
                ]),
                of(&[]),
            ]
        );

        // Go's raw strings and C++'s span lines; Zig's line strings and C's
        // ordinary strings end with theirs.
        let go = "s := `one\ntwo` + \"x\"";
        assert_eq!(
            shown(go, Language::Go),
            [
                of(&[("`one", String)]),
                of(&[("two`", String), ("\"x\"", String)])
            ]
        );
        let cpp = "auto s = R\"x(one\n)\" still)x\";";
        assert_eq!(
            shown(cpp, Language::Cpp),
            [
                of(&[("auto", Keyword), ("R\"x(one", String)]),
                of(&[(")\" still)x\"", String)]),
            ]
        );
        let zig = "const s =\n    \\\\one \"two\n;";
        assert_eq!(
            shown(zig, Language::Zig),
            [
                of(&[("const", Keyword)]),
                of(&[("\\\\one \"two", String)]),
                of(&[]),
            ]
        );
        let unterminated = "char *s = \"open\nint x;";
        assert_eq!(
            shown(unterminated, Language::C),
            [
                of(&[("char", Keyword), ("\"open", String)]),
                of(&[("int", Keyword)]),
            ]
        );
    }
}
