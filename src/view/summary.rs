//! One-line summaries in one style for every language and client
//! (`docs/views.md`): `"text"`, `len=3 [1, 2, 3]`, `None`. Clients render
//! values with these too, so a summary and a printed value agree.

use std::fmt::Write as _;
use std::sync::Arc;

use rustc_apfloat::Float as _;
use rustc_apfloat::ieee::X87DoubleExtended;

use crate::{
    BaseTypeEncoding, FloatValue, IntegerValue, PresentedCount, PresentedShape, ScalarValue,
    TextCompletion, TextSummary, TypeInfo, TypeKind, ValueChildren, VariableState,
    VariableUnavailableReason, VariableValue, VirtualAddress,
};

/// The most elements a summary previews.
pub const MAX_ELEMENTS: usize = 16;

/// The most characters a summary's elements take before it stops.
pub const MAX_CHARACTERS: usize = 96;

/// Text in double quotes, escaping what is not printable, saying when more
/// text follows or could not be read.
#[must_use]
pub fn quoted(text: &TextSummary) -> String {
    let mut output = String::from("\"");
    for chunk in text.bytes.utf8_chunks() {
        for character in chunk.valid().chars() {
            match character {
                '"' => output.push_str("\\\""),
                '\\' => output.push_str("\\\\"),
                '\n' => output.push_str("\\n"),
                '\t' => output.push_str("\\t"),
                '\r' => output.push_str("\\r"),
                character if character.is_control() => {
                    output.push_str(&character.escape_unicode().to_string());
                }
                character => output.push(character),
            }
        }
        for byte in chunk.invalid() {
            let _ = write!(output, "\\x{byte:02x}");
        }
    }
    output.push('"');
    match text.completion {
        TextCompletion::Complete => {}
        TextCompletion::Truncated { length: None } => output.push_str("..."),
        TextCompletion::Truncated {
            length: Some(length),
        } => {
            let _ = write!(output, "... ({length} bytes)");
        }
        TextCompletion::Unreadable { address } => {
            let _ = write!(output, "... <unreadable at {address}>");
        }
        TextCompletion::Limited { length, exhaustion } => {
            if let Some(length) = length {
                let _ = write!(output, "... ({length} bytes)");
            } else {
                output.push_str("...");
            }
            let _ = write!(
                output,
                " <{}>",
                VariableUnavailableReason::InspectionLimit(exhaustion)
            );
        }
    }
    output
}

/// An integer, in decimal.
#[must_use]
pub fn integer(value: IntegerValue) -> String {
    match value {
        IntegerValue::Signed(value) => value.to_string(),
        IntegerValue::Unsigned(value) => value.to_string(),
    }
}

/// A scalar, with the printable ASCII character a character type's value
/// stands for.
#[must_use]
pub fn scalar(value: &ScalarValue, character: bool) -> String {
    let with_character = |number: String, code: Option<u8>| match code {
        Some(code) if character && code.is_ascii_graphic() => {
            format!("{number} '{}'", char::from(code).escape_default())
        }
        _ => number,
    };
    match value {
        ScalarValue::Boolean(value) => value.to_string(),
        ScalarValue::Signed(value) => with_character(value.to_string(), u8::try_from(*value).ok()),
        ScalarValue::Unsigned(value) => {
            with_character(value.to_string(), u8::try_from(*value).ok())
        }
        ScalarValue::Floating(value) => float(*value),
        ScalarValue::Complex { real, imaginary } => {
            let imaginary = float(*imaginary);
            let sign = if imaginary.starts_with('-') { "" } else { "+" };
            format!("({}{sign}{imaginary}i)", float(*real))
        }
    }
}

/// A float, exactly as its format holds it: its shortest digits that read
/// back as it, with an exponent when it is very large or very small.
#[must_use]
pub fn float(value: FloatValue) -> String {
    match value {
        FloatValue::Binary32(bits) => shortest(f32::from_bits(bits)),
        FloatValue::Binary64(bits) => shortest(f64::from_bits(bits)),
        FloatValue::X87Extended {
            significand,
            sign_exponent,
        } => X87DoubleExtended::from_bits(
            u128::from(significand) | (u128::from(sign_exponent) << 64),
        )
        .to_string(),
    }
}

/// A float's shortest round-trip digits, which take an exponent past 1e21
/// or below 1e-6, as JavaScript writes numbers, rather than hundreds of
/// zeros.
fn shortest<F: std::fmt::Display + std::fmt::LowerExp>(value: F) -> String {
    let scientific = format!("{value:e}");
    let exponent = scientific
        .rsplit_once('e')
        .and_then(|(_, exponent)| exponent.parse::<i32>().ok());
    match exponent {
        Some(exponent) if !(-6..21).contains(&exponent) => scientific,
        _ => value.to_string(),
    }
}

/// Whether one-byte integers of a type are characters.
#[must_use]
pub const fn is_character(type_info: &TypeInfo) -> bool {
    matches!(
        &type_info.kind,
        TypeKind::Base(base) if base.byte_size == 1
            && matches!(base.encoding, BaseTypeEncoding::SignedCharacter | BaseTypeEncoding::UnsignedCharacter)
    )
}

/// A value on one line: its presentation's summary when a view presents
/// it, its text, a scalar, or a placeholder for an aggregate's parts.
#[must_use]
pub fn value(type_info: Option<&TypeInfo>, state: &VariableState) -> String {
    let (value, text, presentation, children) = match state {
        VariableState::Available {
            value,
            text,
            presentation,
            children,
            ..
        } => (value, text, presentation, children),
        VariableState::Unavailable(_) => return "<unavailable>".to_owned(),
        VariableState::Malformed(_) => return "<malformed>".to_owned(),
        VariableState::Invalid { .. } => return "<invalid>".to_owned(),
    };
    if let Some(presentation) = presentation
        && presentation.shape != PresentedShape::Raw
    {
        return presentation.summary.to_string();
    }
    if let (Some(text), false) = (text, matches!(value, VariableValue::Address(_))) {
        return quoted(text);
    }
    let character = type_info.is_some_and(is_character);
    let partless = matches!(children, ValueChildren::Available(parts) if parts.total() == 0);
    let rendered = match value {
        VariableValue::Scalar(value) => scalar(value, character),
        VariableValue::Enumeration { value, matches } => matches
            .first()
            .map_or_else(|| integer(*value), |enumerator| enumerator.name.to_string()),
        VariableValue::Address(address) => format!("{:#x}", address.address.get()),
        VariableValue::ImplicitPointer => "<implicit pointer>".to_owned(),
        VariableValue::Function { code, function } => self::function(*code, function.as_deref()),
        VariableValue::Array { .. } | VariableValue::Slice { .. } => "[…]".to_owned(),
        // A record with no parts has nothing to elide; Rust's is `()`.
        VariableValue::Record if partless => {
            if type_info.is_some_and(|info| info.name.as_ref() == "()") {
                "()".to_owned()
            } else {
                "{}".to_owned()
            }
        }
        VariableValue::Record | VariableValue::Union | VariableValue::Variant { .. } => {
            "{…}".to_owned()
        }
    };
    match (text, value) {
        (Some(text), VariableValue::Address(_)) => format!("{rendered} {}", quoted(text)),
        _ => rendered,
    }
}

/// A function value: the function it calls, `nil`, or the address of
/// code no debug information names.
#[must_use]
pub fn function(code: Option<VirtualAddress>, function: Option<&str>) -> String {
    match (code, function) {
        (None, _) => "nil".to_owned(),
        (Some(_), Some(function)) => function.to_owned(),
        (Some(code), None) => format!("{:#x}", code.get()),
    }
}

/// A count as a summary begins: `len=3`, or `len>=3` when there are at
/// least that many.
fn length(count: PresentedCount) -> String {
    match count {
        PresentedCount::Exact(count) => format!("len={count}"),
        PresentedCount::AtLeast(count) => format!("len>={count}"),
    }
}

/// A sequence's summary: its length and the elements previewed.
#[must_use]
pub fn sequence(count: PresentedCount, elements: &[String], complete: bool) -> String {
    bracketed(count, elements, complete, ('[', ']'))
}

/// A map's summary: its length and the entries previewed, each `key:
/// value`.
#[must_use]
pub fn map(count: PresentedCount, entries: &[String], complete: bool) -> String {
    bracketed(count, entries, complete, ('{', '}'))
}

/// A record's summary: `{name: value, …}`, or `(value, …)` when its
/// members are positions, as a tuple's are. `complete` says whether every
/// member is in `members`.
#[must_use]
pub fn record(members: &[(Arc<str>, String)], complete: bool) -> String {
    let positional = members
        .iter()
        .enumerate()
        .all(|(index, (name, _))| name.as_ref() == index.to_string());
    let (open, close) = if positional { ('(', ')') } else { ('{', '}') };
    let mut output = String::from(open);
    for (index, (name, value)) in members.iter().enumerate() {
        if index > 0 {
            output.push_str(", ");
        }
        if output.chars().count() > MAX_CHARACTERS {
            output.push('…');
            output.push(close);
            return output;
        }
        if !positional {
            output.push_str(name);
            output.push_str(": ");
        }
        output.push_str(value);
    }
    if !complete {
        output.push_str(if members.is_empty() { "…" } else { ", …" });
    }
    output.push(close);
    output
}

fn bracketed(
    count: PresentedCount,
    items: &[String],
    complete: bool,
    (open, close): (char, char),
) -> String {
    let mut output = format!("{} {open}", length(count));
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            output.push_str(", ");
        }
        output.push_str(item);
    }
    if !complete {
        output.push_str(if items.is_empty() { "…" } else { ", …" });
    }
    output.push(close);
    output
}
