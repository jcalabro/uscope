//! Renders inspected values within a fixed output budget.

use rustc_apfloat::Float as _;
use rustc_apfloat::ieee::X87DoubleExtended;
use uscope::{
    BaseTypeEncoding, ByteOrder, DebuggerHandle, FloatValue, InspectionExhaustion, InspectionLimit,
    InspectionLimits, IntegerValue, ModuleImage, ScalarValue, TypeInfo, TypeKind, ValueChildPage,
    ValueChildQuery, ValueChildRelationship, ValueChildren, Variable, VariableSnapshot,
    VariableState, VariableValue,
};

use super::format::register_bytes;
use super::terminal::{Renderer, Role};

/// The most bytes one command renders.
pub const OUTPUT_LIMIT: usize = 64 * 1024;
pub const OUTPUT_TRUNCATION_MARKER: &str = "<truncated: OutputBytes>";
const ANSI_RESET: &str = "\u{1b}[0m";
/// The most children requested in one page while expanding a value.
const MAX_EXPANDED_CHILDREN: u64 = 256;

/// A string that stops growing at a byte limit, ending with a marker when
/// truncated. Truncation never splits a UTF-8 character or an ANSI sequence.
struct BoundedOutput {
    value: String,
    limit: usize,
    truncated: bool,
}

impl BoundedOutput {
    const fn new(limit: usize) -> Self {
        Self {
            value: String::new(),
            limit,
            truncated: false,
        }
    }

    fn push_str(&mut self, text: &str) {
        if self.truncated {
            return;
        }
        if self.value.len() + text.len() <= self.limit {
            self.value.push_str(text);
            return;
        }
        let marker_bytes = OUTPUT_TRUNCATION_MARKER.len().min(self.limit);
        let ansi = self.value.contains('\u{1b}') || text.contains('\u{1b}');
        let reset_bytes = if ansi && self.limit >= marker_bytes + ANSI_RESET.len() {
            ANSI_RESET.len()
        } else {
            0
        };
        let content_limit = self.limit - marker_bytes - reset_bytes;
        if self.value.len() > content_limit {
            let end = safe_ansi_prefix_end(&self.value, content_limit);
            self.value.truncate(end);
        }
        let end = safe_ansi_prefix_end(text, content_limit - self.value.len());
        self.value.push_str(&text[..end]);
        if reset_bytes != 0 {
            self.value.push_str(ANSI_RESET);
        }
        self.value.push_str(
            &OUTPUT_TRUNCATION_MARKER
                [..floor_char_boundary(OUTPUT_TRUNCATION_MARKER, marker_bytes)],
        );
        self.truncated = true;
    }

    const fn is_truncated(&self) -> bool {
        self.truncated
    }

    fn into_string(self) -> String {
        self.value
    }
}

impl std::fmt::Write for BoundedOutput {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        self.push_str(text);
        Ok(())
    }
}

fn floor_char_boundary(text: &str, limit: usize) -> usize {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Returns the longest prefix length within `limit` that does not end inside
/// a character or an incomplete ANSI escape sequence.
fn safe_ansi_prefix_end(text: &str, limit: usize) -> usize {
    let end = floor_char_boundary(text, limit);
    let Some(escape) = text[..end].rfind('\u{1b}') else {
        return end;
    };
    let sequence = &text.as_bytes()[escape..end];
    if sequence.get(1) != Some(&b'[')
        || !sequence
            .get(2..)
            .is_some_and(|body| body.iter().any(|byte| (0x40..=0x7e).contains(byte)))
    {
        return escape;
    }
    end
}

/// Truncates already rendered text to the output budget.
pub fn bound_output(rendered: &str) -> String {
    let mut output = BoundedOutput::new(OUTPUT_LIMIT);
    output.push_str(rendered);
    output.into_string()
}

/// Describes a state that holds no value, or `None` for an available one.
fn state_failure(state: &VariableState) -> Option<(Role, String)> {
    match state {
        VariableState::Available { .. } => None,
        VariableState::Unavailable(reason) => {
            Some((Role::Warning, format!("<unavailable: {reason}>")))
        }
        VariableState::Malformed(reason) => {
            Some((Role::Error, format!("<malformed: {}>", reason.description)))
        }
        VariableState::Invalid { reason, .. } => {
            Some((Role::Error, format!("<invalid value: {reason}>")))
        }
    }
}

fn exhaustion(exhaustion: InspectionExhaustion) -> String {
    format!(
        "<truncated: {:?} limit {} after {}; requested {}>",
        exhaustion.resource, exhaustion.limit, exhaustion.used, exhaustion.requested
    )
}

/// Renders `(type) name = value` within the output budget.
fn assignment(type_name: &str, name: &str, value: &str, renderer: Renderer) -> String {
    bound_output(&format!(
        "({}) {} = {value}",
        renderer.paint(Role::Type, type_name),
        renderer.paint(Role::Name, name)
    ))
}

pub fn variables(snapshot: &VariableSnapshot, renderer: Renderer) -> String {
    let mut output = BoundedOutput::new(OUTPUT_LIMIT);
    let mut lines = snapshot
        .variables
        .iter()
        .map(|variable| variable_summary(variable, renderer))
        .chain(snapshot.completion.exhaustion().map(exhaustion));
    if let Some(first) = lines.next() {
        output.push_str(&first);
    }
    for line in lines {
        if output.is_truncated() {
            break;
        }
        output.push_str("\n");
        output.push_str(&line);
    }
    output.into_string()
}

fn variable_summary(variable: &Variable, renderer: Renderer) -> String {
    let Some(type_info) = &variable.type_info else {
        return untyped(&variable.name, &variable.state, renderer);
    };
    let value = match state_failure(&variable.state) {
        Some((role, failure)) => renderer.paint(role, failure).to_string(),
        None => renderer
            .paint(Role::Value, state_summary(type_info, &variable.state))
            .to_string(),
    };
    assignment(&type_info.name, &variable.name, &value, renderer)
}

/// Renders a value whose type could not be resolved.
pub fn untyped(name: &str, state: &VariableState, renderer: Renderer) -> String {
    let (role, value) =
        state_failure(state).unwrap_or_else(|| (Role::Warning, "<unknown value>".to_owned()));
    assignment(
        "<unknown type>",
        name,
        &renderer.paint(role, value).to_string(),
        renderer,
    )
}

/// Summarizes a value on one line without expanding aggregates, as
/// `print` shows each variable, or describes why it has none.
pub fn summary(type_info: Option<&TypeInfo>, state: &VariableState) -> String {
    type_info.map_or_else(
        || state_failure(state).map_or_else(|| "<unknown value>".to_owned(), |(_, text)| text),
        |type_info| state_summary(type_info, state),
    )
}

/// Summarizes an available state's value, or describes why it has none.
fn state_summary(type_info: &TypeInfo, state: &VariableState) -> String {
    match state {
        VariableState::Available {
            value, children, ..
        } => value_summary(type_info, value, children),
        _ => state_failure(state)
            .map(|(_, text)| text)
            .unwrap_or_default(),
    }
}

pub fn range(expression: &str, page: &ValueChildPage, renderer: Renderer) -> String {
    let mut values = BoundedOutput::new(OUTPUT_LIMIT);
    let items = page
        .children
        .iter()
        .map(|child| state_summary(&child.type_info, &child.state))
        .chain(page.completion.exhaustion().map(exhaustion));
    for (index, item) in items.enumerate() {
        if values.is_truncated() {
            break;
        }
        if index != 0 {
            values.push_str(", ");
        }
        values.push_str(&item);
    }
    bound_output(&format!(
        "{} = [{}]",
        renderer.paint(Role::Name, expression),
        renderer.paint(Role::Value, values.into_string())
    ))
}

fn value_summary(type_info: &TypeInfo, value: &VariableValue, children: &ValueChildren) -> String {
    let total = match children {
        ValueChildren::Available(reference) => reference.total(),
        _ => 0,
    };
    match value {
        VariableValue::Scalar(value) => scalar(value, is_character(type_info)),
        VariableValue::Enumeration { value, matches } => {
            let raw = integer(*value);
            match matches.as_ref() {
                [] => raw,
                [enumerator] => format!("{} ({raw})", enumerator.name),
                aliases => format!(
                    "{raw} <{}>",
                    aliases
                        .iter()
                        .map(|alias| alias.name.as_ref())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }
        VariableValue::Address(value) => {
            let width = type_info
                .byte_size
                .and_then(|size| usize::try_from(size.checked_mul(2)?).ok())
                .unwrap_or(16);
            format!("0x{:0width$x}", value.address.get())
        }
        VariableValue::ImplicitPointer => "<implicit pointer>".to_owned(),
        VariableValue::Array { .. } => format!("[<{total} elements>]"),
        VariableValue::Slice { length, capacity } => capacity.map_or_else(
            || format!("[<{length} elements>]"),
            |capacity| format!("[<{length} elements; capacity {capacity}>]"),
        ),
        VariableValue::Record => format!("{{<{total} fields>}}"),
        VariableValue::Union => format!("{{<{total} alternatives; active unknown>}}"),
        VariableValue::Variant {
            discriminant,
            active,
        } => {
            let active = active
                .as_ref()
                .and_then(|variant| variant.name.as_deref())
                .unwrap_or("<no matching variant>");
            discriminant.map_or_else(
                || format!("{{<{active}; {total} fields>}}"),
                |value| format!("{{<{active} = {}; {total} fields>}}", integer(value)),
            )
        }
        _ => "<unsupported value>".to_owned(),
    }
}

/// Pending output of [`expanded`], consumed from the back.
enum Work {
    State(Box<(TypeInfo, VariableState)>, u64),
    Text(String),
}

/// Renders a value with its aggregates expanded, fetching child pages until
/// the remaining inspection limits or the output budget run out.
pub async fn expanded(
    debugger: &DebuggerHandle,
    type_info: &TypeInfo,
    name: &str,
    state: &VariableState,
    mut remaining: InspectionLimits,
    renderer: Renderer,
) -> uscope::Result<String> {
    let mut output = BoundedOutput::new(OUTPUT_LIMIT);
    let mut work = vec![Work::State(Box::new((type_info.clone(), state.clone())), 0)];
    while let Some(item) = work.pop() {
        if output.is_truncated() {
            break;
        }
        let (boxed, depth) = match item {
            Work::Text(text) => {
                output.push_str(&text);
                continue;
            }
            Work::State(boxed, depth) => (boxed, depth),
        };
        let (type_info, state) = &*boxed;
        let VariableState::Available {
            value, children, ..
        } = state
        else {
            output.push_str(&state_summary(type_info, state));
            continue;
        };
        if matches!(
            value,
            VariableValue::Scalar(_)
                | VariableValue::Enumeration { .. }
                | VariableValue::Address(_)
                | VariableValue::ImplicitPointer
        ) {
            output.push_str(&value_summary(type_info, value, children));
            continue;
        }
        if depth >= remaining.aggregate_depth {
            output.push_str("<truncated: AggregateDepth>");
            continue;
        }
        let ValueChildren::Available(reference) = children else {
            output.push_str(&value_summary(type_info, value, children));
            continue;
        };
        if let Some(resource) = [
            (InspectionLimit::ValueNodes, remaining.value_nodes),
            (InspectionLimit::MemoryReads, remaining.memory_reads),
            (InspectionLimit::MemoryBytes, remaining.memory_bytes),
            (InspectionLimit::ExpressionWork, remaining.expression_work),
        ]
        .into_iter()
        .find_map(|(resource, remaining)| (remaining == 0).then_some(resource))
        {
            output.push_str(&format!("<truncated: {resource:?}>"));
            continue;
        }

        let requested = reference
            .total()
            .min(MAX_EXPANDED_CHILDREN)
            .min(remaining.value_nodes);
        let page = if requested == 0 {
            None
        } else {
            let page = debugger
                .value_children_with_limits(
                    reference.clone(),
                    ValueChildQuery {
                        offset: 0,
                        limit: u32::try_from(requested).expect("bounded page fits u32"),
                    },
                    remaining,
                )
                .await?;
            remaining = remaining.remaining_after(page.usage);
            Some(page)
        };
        let (opening, closing) = match value {
            VariableValue::Array { .. } | VariableValue::Slice { .. } => ("[", "]".to_owned()),
            VariableValue::Variant { active, .. } => (
                "{",
                active
                    .as_ref()
                    .and_then(|variant| variant.name.as_deref())
                    .map_or_else(|| "}".to_owned(), |name| format!("}}<{name}>")),
            ),
            VariableValue::Union => ("{", "} <active member unknown>".to_owned()),
            _ => ("{", "}".to_owned()),
        };
        output.push_str(opening);
        work.push(Work::Text(closing));
        schedule_children(&mut work, reference.total(), page.as_ref(), depth + 1);
    }
    Ok(assignment(
        &type_info.name,
        name,
        &renderer
            .paint(Role::Value, output.into_string())
            .to_string(),
        renderer,
    ))
}

/// Schedules one aggregate's children, then any truncation markers, as
/// comma-separated items.
fn schedule_children(work: &mut Vec<Work>, total: u64, page: Option<&ValueChildPage>, depth: u64) {
    let children = page.map_or(&[][..], |page| page.children.as_ref());
    let omitted = total.saturating_sub(children.len() as u64);
    let rendered = children.iter().filter(|child| {
        !matches!(
            &child.relationship,
            ValueChildRelationship::Member(member) if member.artificial
        )
    });
    let mut items = rendered
        .map(|child| {
            let label = match &child.relationship {
                ValueChildRelationship::Member(member) => {
                    format!("{} = ", member.name.as_deref().unwrap_or("<anonymous>"))
                }
                ValueChildRelationship::Base(_) => format!("<base {}> = ", child.type_info.name),
                ValueChildRelationship::ArrayElement { .. }
                | ValueChildRelationship::SliceElement { .. } => String::new(),
                _ => "<child> = ".to_owned(),
            };
            vec![
                Work::Text(label),
                Work::State(
                    Box::new((child.type_info.clone(), child.state.clone())),
                    depth,
                ),
            ]
        })
        .chain(
            page.and_then(|page| page.completion.exhaustion())
                .map(|marker| vec![Work::Text(exhaustion(marker))]),
        )
        .chain((omitted != 0).then(|| vec![Work::Text(format!("<{omitted} omitted>"))]))
        .collect::<Vec<_>>();
    while let Some(item) = items.pop() {
        work.extend(item.into_iter().rev());
        if !items.is_empty() {
            work.push(Work::Text(", ".to_owned()));
        }
    }
}

/// Whether one-byte integers of this type are characters.
const fn is_character(type_info: &TypeInfo) -> bool {
    matches!(
        &type_info.kind,
        TypeKind::Base(base) if base.byte_size == 1
            && matches!(base.encoding, BaseTypeEncoding::SignedCharacter | BaseTypeEncoding::UnsignedCharacter)
    )
}

fn integer(value: IntegerValue) -> String {
    match value {
        IntegerValue::Signed(value) => value.to_string(),
        IntegerValue::Unsigned(value) => value.to_string(),
        _ => "<unsupported integer value>".to_owned(),
    }
}

/// Renders a scalar, adding the printable ASCII character for characters.
fn scalar(value: &ScalarValue, character: bool) -> String {
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
        _ => "<unsupported scalar value>".to_owned(),
    }
}

fn float(value: FloatValue) -> String {
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
        _ => "<unsupported floating-point format>".to_owned(),
    }
}

/// How watched bytes are decoded.
enum WatchedScalar {
    Integer { signed: bool },
    Boolean,
    Float,
    Address,
}

/// Renders watched bytes as a scalar when the type resolves to one in the
/// main image, and as little-endian hexadecimal otherwise.
pub fn watched_bytes(
    bytes: Option<&[u8]>,
    type_info: Option<&TypeInfo>,
    image: Option<&ModuleImage>,
) -> String {
    let Some(bytes) = bytes else {
        return "<unreadable>".to_owned();
    };
    let little_endian = |bytes: &[u8]| {
        let mut word = [0_u8; 16];
        word[..bytes.len()].copy_from_slice(bytes);
        u128::from_le_bytes(word)
    };
    match (
        type_info.and_then(|info| watched_scalar(info, image)),
        bytes.len(),
    ) {
        (Some(WatchedScalar::Integer { signed }), length @ 1..=16) => {
            let value = little_endian(bytes);
            if signed {
                let shift = 128 - 8 * length;
                ((value.cast_signed() << shift) >> shift).to_string()
            } else {
                value.to_string()
            }
        }
        (Some(WatchedScalar::Boolean), 1) => (bytes[0] != 0).to_string(),
        (Some(WatchedScalar::Float), 4) => {
            f32::from_le_bytes(bytes.try_into().expect("four bytes")).to_string()
        }
        (Some(WatchedScalar::Float), 8) => {
            f64::from_le_bytes(bytes.try_into().expect("eight bytes")).to_string()
        }
        (Some(WatchedScalar::Address), 8) => format!("{:#x}", little_endian(bytes)),
        _ => register_bytes(bytes, ByteOrder::Little),
    }
}

/// Follows typedefs and qualifiers in the main image to a decodable scalar.
fn watched_scalar(type_info: &TypeInfo, image: Option<&ModuleImage>) -> Option<WatchedScalar> {
    const MAX_TYPE_CHAIN: usize = 16;
    let mut current = type_info;
    for _ in 0..MAX_TYPE_CHAIN {
        let next = match &current.kind {
            TypeKind::Base(base)
            | TypeKind::Enumeration {
                representation: base,
                ..
            } => {
                return Some(match base.encoding {
                    BaseTypeEncoding::Boolean => WatchedScalar::Boolean,
                    BaseTypeEncoding::Floating => WatchedScalar::Float,
                    BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter => {
                        WatchedScalar::Integer { signed: true }
                    }
                    _ => WatchedScalar::Integer { signed: false },
                });
            }
            TypeKind::Pointer { .. } | TypeKind::Reference { .. } => {
                return Some(WatchedScalar::Address);
            }
            TypeKind::Modified { target, .. }
            | TypeKind::Named {
                target: Some(target),
                ..
            } => *target,
            _ => return None,
        };
        current = image
            .filter(|image| image.id() == next.image)?
            .type_info(next)?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_output_is_utf8_safe_and_never_exceeds_its_limit() {
        let mut output = BoundedOutput::new(32);
        output.push_str("prefix ");
        output.push_str(&"é".repeat(32));
        let rendered = output.into_string();

        assert!(rendered.len() <= 32, "{rendered:?}");
        assert!(rendered.ends_with(OUTPUT_TRUNCATION_MARKER));
    }

    #[test]
    fn bounded_output_closes_ansi_style_before_its_marker() {
        let styled = Renderer::new(true)
            .paint(Role::Value, "é".repeat(32))
            .to_string();
        let mut output = BoundedOutput::new(32);
        output.push_str(&styled);
        let rendered = output.into_string();
        let content = rendered
            .strip_suffix(OUTPUT_TRUNCATION_MARKER)
            .expect("truncated output ends with its marker");

        assert!(rendered.len() <= 32, "{rendered:?}");
        assert!(content.ends_with(ANSI_RESET), "{rendered:?}");
    }

    #[test]
    fn range_and_untyped_rendering_bound_the_complete_emitted_value() {
        let page = ValueChildPage {
            stop_id: uscope::StopId::new(1),
            offset: 0,
            total: 0,
            children: [].into(),
            completion: uscope::InspectionCompletion::Complete,
            usage: uscope::InspectionUsage::default(),
        };
        let state = VariableState::Malformed(uscope::VariableMalformedReason {
            kind: uscope::VariableMalformedKind::InvalidAttribute,
            description: "x".repeat(OUTPUT_LIMIT).into(),
        });
        for rendered in [
            range(&"x".repeat(OUTPUT_LIMIT), &page, Renderer::new(false)),
            untyped("value", &state, Renderer::new(false)),
        ] {
            assert!(
                rendered.len() <= OUTPUT_LIMIT,
                "rendered {} bytes",
                rendered.len()
            );
            assert!(rendered.ends_with(OUTPUT_TRUNCATION_MARKER));
        }
    }

    #[test]
    fn characters_render_their_escaped_ascii_form() {
        let render = |value: i128| scalar(&ScalarValue::Signed(value), true);
        assert_eq!(render(65), "65 'A'");
        assert_eq!(render(39), r"39 '\''");
        assert_eq!(render(92), r"92 '\\'");
        assert_eq!(render(-1), "-1");
        assert_eq!(scalar(&ScalarValue::Unsigned(66), true), "66 'B'");
        assert_eq!(scalar(&ScalarValue::Unsigned(66), false), "66");
    }

    #[test]
    fn floating_values_preserve_special_signs_and_extended_precision() {
        assert_eq!(float(FloatValue::Binary32(f32::INFINITY.to_bits())), "inf");
        assert_eq!(float(FloatValue::Binary64((-0.0_f64).to_bits())), "-0");
        assert_eq!(
            float(FloatValue::X87Extended {
                significand: 0xc800_0000_0000_0000,
                sign_exponent: 0x4000,
            }),
            "3.125"
        );
    }

    #[test]
    fn watched_bytes_without_a_scalar_type_render_as_little_endian_hex() {
        assert_eq!(
            watched_bytes(Some(&[0x34, 0x12, 0, 0]), None, None),
            "0x00001234"
        );
        assert_eq!(watched_bytes(None, None, None), "<unreadable>");
    }
}
