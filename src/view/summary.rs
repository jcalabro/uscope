//! One-line summaries in one style for every language and client
//! (`docs/views.md`): `"text"`, `len=3 [1, 2, 3]`, `None`. Clients render
//! values with these too, so a summary and a printed value agree.

use std::fmt::Write as _;

use rustc_apfloat::Float as _;
use rustc_apfloat::ieee::X87DoubleExtended;

use crate::{
    BaseTypeEncoding, FloatValue, IntegerValue, PresentedShape, ScalarValue, TextCompletion,
    TextSummary, TypeInfo, TypeKind, VariableState, VariableUnavailableReason, VariableValue,
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
    }
}

/// A float, exactly as its format holds it.
#[must_use]
pub fn float(value: FloatValue) -> String {
    match value {
        FloatValue::Binary32(bits) => f32::from_bits(bits).to_string(),
        FloatValue::Binary64(bits) => f64::from_bits(bits).to_string(),
        FloatValue::X87Extended {
            significand,
            sign_exponent,
        } => X87DoubleExtended::from_bits(
            u128::from(significand) | (u128::from(sign_exponent) << 64),
        )
        .to_string(),
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
    let VariableState::Available {
        value,
        text,
        presentation,
        ..
    } = state
    else {
        return match state {
            VariableState::Unavailable(_) => "<unavailable>",
            VariableState::Malformed(_) => "<malformed>",
            VariableState::Invalid { .. } => "<invalid>",
            VariableState::Available { .. } => unreachable!("handled above"),
        }
        .to_owned();
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
    let rendered = match value {
        VariableValue::Scalar(value) => scalar(value, character),
        VariableValue::Enumeration { value, matches } => matches
            .first()
            .map_or_else(|| integer(*value), |enumerator| enumerator.name.to_string()),
        VariableValue::Address(address) => format!("{:#x}", address.address.get()),
        VariableValue::ImplicitPointer => "<implicit pointer>".to_owned(),
        VariableValue::Array { .. } | VariableValue::Slice { .. } => "[…]".to_owned(),
        VariableValue::Record | VariableValue::Union | VariableValue::Variant { .. } => {
            "{…}".to_owned()
        }
    };
    match (text, value) {
        (Some(text), VariableValue::Address(_)) => format!("{rendered} {}", quoted(text)),
        _ => rendered,
    }
}

/// A sequence's summary: its length and the elements previewed.
#[must_use]
pub fn sequence(length: u64, elements: &[String], complete: bool) -> String {
    let mut output = format!("len={length} [");
    for (index, element) in elements.iter().enumerate() {
        if index > 0 {
            output.push_str(", ");
        }
        output.push_str(element);
    }
    if !complete {
        output.push_str(if elements.is_empty() { "…" } else { ", …" });
    }
    output.push(']');
    output
}
