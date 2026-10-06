//! Renders inspected values within a fixed output budget.

use std::fmt::Write as _;
use std::sync::Arc;

use uscope::{
    BaseTypeEncoding, ByteOrder, DebuggerHandle, InspectionExhaustion, InspectionLimit,
    InspectionLimits, IntegerValue, ModuleImage, Presentation, PresentedCount, PresentedShape,
    ScalarValue, TypeInfo, TypeKind, ValueChildPage, ValueChildQuery, ValueChildRelationship,
    ValueChildren, ValueChildrenReference, Variable, VariableSnapshot, VariableState,
    VariableValue,
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

/// Summarizes an available state's value as its view presents it, or
/// describes why it has none.
fn state_summary(type_info: &TypeInfo, state: &VariableState) -> String {
    rendered_summary(type_info, state, false)
}

/// The text a view gave a value, which the value as stored does not show.
fn stored_text(state: &VariableState) -> Option<&Arc<uscope::TextSummary>> {
    let VariableState::Available {
        text, presentation, ..
    } = state
    else {
        return None;
    };
    let from_view = presentation
        .as_ref()
        .is_some_and(|presentation| presentation.shape == PresentedShape::Text);
    text.as_ref().filter(|_| !from_view)
}

/// A value's one-line summary: as its view presents it, unless `raw`.
fn rendered_summary(type_info: &TypeInfo, state: &VariableState, raw: bool) -> String {
    match state {
        VariableState::Available {
            value,
            children,
            presentation,
            ..
        } => {
            if let Some(presentation) = presentation.as_ref().filter(|_| !raw)
                && presentation.shape != PresentedShape::Raw
            {
                return presentation.summary.to_string();
            }
            let summary = value_summary(type_info, value, children);
            let summary = match (stored_text(state), value) {
                (None, _) => summary,
                // A pointer keeps its address; the text follows it.
                (Some(text), VariableValue::Address(_)) => {
                    format!("{summary} {}", uscope::quoted_text(text))
                }
                (Some(text), _) => uscope::quoted_text(text),
            };
            match presentation.as_ref().filter(|_| !raw) {
                Some(presentation) => format!("{summary} {}", view_failure(presentation)),
                None => summary,
            }
        }
        _ => state_failure(state)
            .map(|(_, text)| text)
            .unwrap_or_default(),
    }
}

/// Why a view showed a value as stored.
fn view_failure(presentation: &Presentation) -> String {
    format!(
        "<view {}: {}>",
        presentation.view,
        presentation
            .problem
            .as_ref()
            .map_or_else(|| "failed".to_owned(), ToString::to_string)
    )
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
        VariableValue::Scalar(value) => uscope::scalar_text(value, is_character(type_info)),
        VariableValue::Enumeration { value, matches } => {
            let raw = uscope::integer_text(*value);
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
            let active = active.as_ref().map_or("<no matching variant>", |variant| {
                // Rust names a variant by its one member instead.
                variant
                    .name
                    .as_deref()
                    .or_else(|| match variant.members.as_ref() {
                        [member] => member.name.as_deref(),
                        _ => None,
                    })
                    .unwrap_or("<unnamed variant>")
            });
            discriminant.map_or_else(
                || format!("{{<{active}; {total} fields>}}"),
                |value| {
                    format!(
                        "{{<{active} = {}; {total} fields>}}",
                        uscope::integer_text(value)
                    )
                },
            )
        }
        _ => "<unsupported value>".to_owned(),
    }
}

/// Whether a value has no parts to expand.
const fn is_leaf(value: &VariableValue) -> bool {
    matches!(
        value,
        VariableValue::Scalar(_)
            | VariableValue::Enumeration { .. }
            | VariableValue::Address(_)
            | VariableValue::ImplicitPointer
    )
}

/// Pending output of [`expanded`], consumed from the back.
enum Work {
    State(Box<(TypeInfo, VariableState)>, u64),
    Text(String),
}

/// Renders a value with its aggregates expanded, fetching child pages until
/// the remaining inspection limits or the output budget run out.
#[expect(
    clippy::too_many_lines,
    reason = "values as stored and as presented are expanded in one bounded walk"
)]
pub async fn expanded(
    debugger: &DebuggerHandle,
    type_info: &TypeInfo,
    name: &str,
    state: &VariableState,
    mut remaining: InspectionLimits,
    raw: bool,
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
            value,
            children,
            presentation,
            ..
        } = state
        else {
            output.push_str(&state_summary(type_info, state));
            continue;
        };
        let presentation = presentation.as_deref().filter(|_| !raw);
        // A view's elements show inside its summary's brackets; any other
        // presentation is its summary.
        if let Some(presentation) = presentation
            && presentation.shape != PresentedShape::Raw
        {
            let (
                shape @ (PresentedShape::Sequence | PresentedShape::Map),
                ValueChildren::Available(reference),
                Some(count),
            ) = (
                presentation.shape,
                &presentation.children,
                presentation.count,
            )
            else {
                output.push_str(&presentation.summary);
                continue;
            };
            let length = match count {
                PresentedCount::Exact(count) => format!("len={count}"),
                PresentedCount::AtLeast(count) => format!("len>={count}"),
                _ => {
                    output.push_str(&presentation.summary);
                    continue;
                }
            };
            let count = count.known();
            let page = first_children(debugger, reference, count, &mut remaining).await?;
            let (opening, closing) = if shape == PresentedShape::Map {
                ("{", "}")
            } else {
                ("[", "]")
            };
            output.push_str(&format!("{length} {opening}"));
            work.push(Work::Text(closing.to_owned()));
            schedule_children(&mut work, count, page.as_ref(), depth + 1, raw);
            continue;
        }
        if let Some(presentation) = presentation {
            work.push(Work::Text(format!(" {}", view_failure(presentation))));
        }
        // Strings show as their text rather than their parts.
        if stored_text(state).is_some() || is_leaf(value) {
            output.push_str(&rendered_summary(type_info, state, true));
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

        let page = first_children(debugger, reference, reference.total(), &mut remaining).await?;
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
        schedule_children(&mut work, reference.total(), page.as_ref(), depth + 1, raw);
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

/// Fetches the first page of up to `count` children that the expansion
/// and the remaining limits allow, charging it to `remaining`.
async fn first_children(
    debugger: &DebuggerHandle,
    reference: &Arc<ValueChildrenReference>,
    count: u64,
    remaining: &mut InspectionLimits,
) -> uscope::Result<Option<ValueChildPage>> {
    let requested = count.min(MAX_EXPANDED_CHILDREN).min(remaining.value_nodes);
    if requested == 0 {
        return Ok(None);
    }
    let page = debugger
        .value_children_with_limits(
            reference.clone(),
            ValueChildQuery {
                offset: 0,
                limit: u32::try_from(requested).expect("bounded page fits u32"),
            },
            *remaining,
        )
        .await?;
    *remaining = remaining.remaining_after(page.usage);
    Ok(Some(page))
}

/// Schedules one aggregate's children, then any truncation markers, as
/// comma-separated items.
fn schedule_children(
    work: &mut Vec<Work>,
    total: u64,
    page: Option<&ValueChildPage>,
    depth: u64,
    raw: bool,
) {
    let children = page.map_or(&[][..], |page| page.children.as_ref());
    let omitted = total.saturating_sub(children.len() as u64);
    let rendered = children.iter().filter(|child| {
        !matches!(
            &child.relationship,
            ValueChildRelationship::Member(member) if member.artificial
        ) && (!raw || !matches!(child.relationship, ValueChildRelationship::Raw))
    });
    let mut items = rendered
        .map(|child| {
            let label = match &child.relationship {
                ValueChildRelationship::Member(member) => {
                    format!("{} = ", member.name.as_deref().unwrap_or("<anonymous>"))
                }
                ValueChildRelationship::Base(_) => format!("<base {}> = ", child.type_info.name),
                ValueChildRelationship::ArrayElement { .. }
                | ValueChildRelationship::SliceElement { .. }
                | ValueChildRelationship::Element { .. } => String::new(),
                ValueChildRelationship::Field { name } => format!("{name} = "),
                ValueChildRelationship::Entry { key, .. } => {
                    format!("{}: ", state_summary(&key.type_info, &key.state))
                }
                ValueChildRelationship::Raw => "[raw] = ".to_owned(),
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
        (Some(WatchedScalar::Float), 4) => uscope::float_text(uscope::FloatValue::Binary32(
            u32::from_le_bytes(bytes.try_into().expect("four bytes")),
        )),
        (Some(WatchedScalar::Float), 8) => uscope::float_text(uscope::FloatValue::Binary64(
            u64::from_le_bytes(bytes.try_into().expect("eight bytes")),
        )),
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
                    BaseTypeEncoding::ComplexFloating => return None,
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

/// Renders an integer in hexadecimal within its type's width, as `print/x`
/// does; other values render as `print` shows them.
pub fn hexadecimal(
    type_info: &TypeInfo,
    name: &str,
    state: &VariableState,
    renderer: Renderer,
) -> String {
    let integer = match state {
        VariableState::Available {
            value: VariableValue::Scalar(ScalarValue::Signed(value)),
            ..
        } => Some(IntegerValue::Signed(*value)),
        VariableState::Available {
            value: VariableValue::Scalar(ScalarValue::Unsigned(value)),
            ..
        } => Some(IntegerValue::Unsigned(*value)),
        VariableState::Available {
            value: VariableValue::Enumeration { value, .. },
            ..
        } => Some(*value),
        _ => None,
    };
    // An exact integer has no width: a negative one keeps its sign.
    let width = type_info
        .byte_size
        .map(|size| size.saturating_mul(8).min(128));
    let text = integer.and_then(|integer| match (integer, width) {
        (IntegerValue::Signed(value), None) if value < 0 => {
            Some(format!("-{:#x}", value.unsigned_abs()))
        }
        (IntegerValue::Signed(value), Some(width)) => {
            let mask = if width >= 128 {
                u128::MAX
            } else {
                (1_u128 << width) - 1
            };
            Some(format!("{:#x}", value.cast_unsigned() & mask))
        }
        (IntegerValue::Signed(value), None) => Some(format!("{value:#x}")),
        (IntegerValue::Unsigned(value), _) => Some(format!("{value:#x}")),
        _ => None,
    });
    let value = text.map_or_else(
        || state_summary(type_info, state),
        |text| renderer.paint(Role::Value, text).to_string(),
    );
    assignment(&type_info.name, name, &value, renderer)
}

/// A type's name with the path its producer's name leaves out, as in
/// `std::vector<int, std::allocator<int> >`. Go and Zig names already
/// carry their packages and modules.
fn qualified_name(type_info: &TypeInfo) -> String {
    let Some(identity) = type_info.identity.as_deref() else {
        return type_info.name.to_string();
    };
    let spells_path = identity.path.first().is_none_or(|first| {
        type_info
            .name
            .strip_prefix(first.as_ref())
            .is_some_and(|rest| rest.starts_with("::"))
    });
    if spells_path
        || !matches!(
            identity.language,
            uscope::SourceLanguage::C | uscope::SourceLanguage::Cpp | uscope::SourceLanguage::Rust
        )
    {
        return type_info.name.to_string();
    }
    let mut name = String::new();
    for segment in identity.path.iter() {
        let _ = write!(name, "{segment}::");
    }
    name.push_str(&type_info.name);
    name
}

/// Renders a type's definition, as `ptype` does: a record's or union's
/// members, an enumeration's enumerators, or the name of any other type,
/// then the type's template or generic arguments.
pub fn type_definition(
    type_info: &TypeInfo,
    images: &[std::sync::Arc<ModuleImage>],
    renderer: Renderer,
) -> String {
    let info_of = |reference: uscope::TypeReference| {
        images.iter().find_map(|image| image.type_info(reference))
    };
    let name_of = |reference: uscope::TypeReference| {
        info_of(reference).map_or_else(|| "<unknown>".to_owned(), |info| info.name.to_string())
    };
    let name = qualified_name(type_info);
    let members = |keyword: &str, members: &[uscope::RecordMember]| {
        let mut output = format!("type = {keyword} {name} {{\n");
        for member in members {
            let bits = match member.layout {
                uscope::RecordMemberLayout::BitRange { bit_size, .. } => format!(" : {bit_size}"),
                _ => String::new(),
            };
            let _ = writeln!(
                output,
                "    {} {}{bits};",
                renderer.paint(Role::Type, name_of(member.type_ref)),
                member.name.as_deref().unwrap_or("<anonymous>"),
            );
        }
        output.push('}');
        output
    };
    let mut output = match &type_info.kind {
        TypeKind::Record {
            kind,
            members: fields,
            ..
        } => members(
            if *kind == uscope::RecordKind::Class {
                "class"
            } else {
                "struct"
            },
            fields,
        ),
        TypeKind::Union {
            members: fields, ..
        } => members("union", fields),
        TypeKind::Enumeration { enumerators, .. } => format!(
            "type = enum {name} {{{}}}",
            enumerators
                .iter()
                .map(|enumerator| {
                    let value = match enumerator.value {
                        IntegerValue::Signed(value) => value.to_string(),
                        IntegerValue::Unsigned(value) => value.to_string(),
                        _ => "?".to_owned(),
                    };
                    format!("{} = {value}", enumerator.name)
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => format!("type = {}", renderer.paint(Role::Type, &type_info.name)),
    };
    if let Some(identity) = type_info.identity.as_deref()
        && !identity.arguments.is_empty()
    {
        let arguments = identity
            .arguments
            .iter()
            .map(|argument| match argument {
                uscope::TypeArgument::Type(reference) => {
                    info_of(*reference).map_or_else(|| "<unknown>".to_owned(), qualified_name)
                }
                uscope::TypeArgument::Value(IntegerValue::Signed(value)) => value.to_string(),
                uscope::TypeArgument::Value(IntegerValue::Unsigned(value)) => value.to_string(),
                uscope::TypeArgument::Unknown(text) => text.to_string(),
                _ => "?".to_owned(),
            })
            .map(|argument| renderer.paint(Role::Type, argument).to_string())
            .collect::<Vec<_>>();
        let _ = write!(output, "\narguments: {}", arguments.join(", "));
    }
    output
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
    fn scalars_render_characters_and_special_floats() {
        let character = |value: i128| uscope::scalar_text(&ScalarValue::Signed(value), true);
        assert_eq!(character(65), "65 'A'");
        assert_eq!(character(39), r"39 '\''");
        assert_eq!(character(92), r"92 '\\'");
        assert_eq!(character(-1), "-1");
        assert_eq!(
            uscope::scalar_text(&ScalarValue::Unsigned(66), true),
            "66 'B'"
        );
        assert_eq!(uscope::scalar_text(&ScalarValue::Unsigned(66), false), "66");

        let float = uscope::float_text;
        assert_eq!(
            float(uscope::FloatValue::Binary32(f32::INFINITY.to_bits())),
            "inf"
        );
        assert_eq!(
            float(uscope::FloatValue::Binary64((-0.0_f64).to_bits())),
            "-0"
        );
        assert_eq!(
            float(uscope::FloatValue::X87Extended {
                significand: 0xc800_0000_0000_0000,
                sign_exponent: 0x4000,
            }),
            "3.125"
        );
        // Shortest round-trip digits, with an exponent where plain digits
        // would run to hundreds of zeros.
        let double = |value: f64| float(uscope::FloatValue::Binary64(value.to_bits()));
        assert_eq!(double(3e300), "3e300");
        assert_eq!(double(1e21), "1e21");
        assert_eq!(
            double(123_456_789_012_345_680_000.0),
            "123456789012345680000"
        );
        assert_eq!(double(0.000_001), "0.000001");
        assert_eq!(double(-1.5e-7), "-1.5e-7");
        assert_eq!(double(0.1), "0.1");
        assert_eq!(
            float(uscope::FloatValue::Binary32(f32::MAX.to_bits())),
            "3.4028235e38"
        );
        assert_eq!(
            uscope::scalar_text(
                &ScalarValue::Complex {
                    real: uscope::FloatValue::Binary64(1.5_f64.to_bits()),
                    imaginary: uscope::FloatValue::Binary64((-2.0_f64).to_bits()),
                },
                false
            ),
            "(1.5-2i)"
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
