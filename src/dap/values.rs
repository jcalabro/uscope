//! Values as the client's variables.
//!
//! A value's text is the same summary the console's `print` shows for a
//! variable. Values without one, such as optimized-out variables, are still
//! variables whose text explains why. Aggregates expand to their children,
//! and pointers expand to what they point to.

use std::fmt::Write as _;
use std::sync::Arc;

use serde_json::{Map, Value, json};
use uscope::{
    ImageAddress, IntegerValue, ModuleImage, PresentedShape, RegisterValue, ScalarValue,
    StopContext, SymbolKind, TypeInfo, ValueChild, ValueChildRelationship, ValueChildren,
    VariableState, VariableValue, VariableValueSource,
};

use super::handles::{Exhausted, Location, References, Variables};
use crate::cli::format::register_bytes;
use crate::cli::value::summary;

/// What the client accepts in a variable, and how it wants values shown.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Whether to include each value's type.
    pub types: bool,
    /// Whether to include memory references.
    pub memory: bool,
    /// Whether to show integers in hexadecimal.
    pub hex: bool,
}

/// How values show when a request does not say.
#[derive(Debug, Clone, Copy, Default)]
pub struct Display {
    /// Whether integers show in hexadecimal.
    pub hex: bool,
}

/// One value to present.
pub struct Item<'a> {
    pub name: &'a str,
    /// How to evaluate the value again, when it can be.
    pub path: Option<uscope::Expression>,
    /// Whether the value is `[raw]`, the value as stored, which its path
    /// would evaluate as its view presents it: its own children are reached
    /// through the path, but it is not named by it.
    pub raw: bool,
    pub type_info: Option<&'a TypeInfo>,
    pub state: &'a VariableState,
    /// Where the value's variable is declared, in a module's sources.
    pub declaration: Option<(uscope::ModuleId, uscope::SourceLocation)>,
}

/// The loaded modules' code, to tell which addresses enter a function.
#[derive(Debug, Default)]
pub struct Code {
    /// Each module's load bias and image.
    modules: Vec<(u64, Arc<ModuleImage>)>,
}

impl Code {
    pub const fn new(modules: Vec<(u64, Arc<ModuleImage>)>) -> Self {
        Self { modules }
    }

    /// Each module's load bias and image.
    pub fn modules(&self) -> &[(u64, Arc<ModuleImage>)] {
        &self.modules
    }

    /// The image and image address of the function an address enters, when
    /// it is a function's first instruction rather than any other address.
    pub fn function_entry(&self, address: u64) -> Option<(&Arc<ModuleImage>, ImageAddress)> {
        self.modules.iter().find_map(|(bias, image)| {
            let address = ImageAddress::new(address.checked_sub(*bias)?);
            if !image.contains_address(address) {
                return None;
            }
            let symbol = image.symbolize(address)?;
            (symbol.offset == 0
                && matches!(
                    symbol.kind,
                    SymbolKind::Function | SymbolKind::IndirectFunction
                ))
            .then_some((image, address))
        })
    }
}

/// Presents a value as a `Variable`, with a reference that expands it.
pub fn variable(
    item: Item<'_>,
    context: StopContext,
    options: Options,
    references: &mut References,
    code: &Code,
) -> Result<Map<String, Value>, Exhausted> {
    let mut variable = Map::new();
    variable.insert("name".to_owned(), item.name.into());
    variable.insert(
        "value".to_owned(),
        text(item.type_info, item.state, options.hex).into(),
    );
    if options.types {
        variable.insert(
            "type".to_owned(),
            item.type_info
                .map_or("<unknown type>", |type_info| &type_info.name)
                .into(),
        );
    }
    if let Some((module, location)) = item.declaration {
        variable.insert(
            "declarationLocationReference".to_owned(),
            references
                .location(Location::Declared { module, location })?
                .into(),
        );
    }
    let path = item.path;
    let named = path.is_some();
    let whole = path.as_ref().is_some_and(uscope::Expression::is_name);
    if let Some(path) = path.as_ref().filter(|_| !item.raw) {
        variable.insert("evaluateName".to_owned(), path.to_string().into());
    }
    let mut reference = 0;
    if let VariableState::Available {
        source,
        value,
        dereference,
        children,
        presentation,
        ..
    } = item.state
    {
        // A value a view presents expands to its elements, its fields, and
        // `[raw]`, the value as stored.
        let presented = presentation
            .as_deref()
            .filter(|presentation| presentation.shape != PresentedShape::Raw);
        let children = presented.map_or(children, |presentation| &presentation.children);
        if let ValueChildren::Available(children) = children {
            let indexed = insert_counts(&mut variable, children, value);
            reference = references.variables(Variables::Children {
                context,
                reference: children.clone(),
                path,
                indexed,
            })?;
        } else if let uscope::DereferenceState::Available(dereference) = dereference {
            reference = references.variables(Variables::Pointee {
                context,
                reference: dereference.clone(),
                name: item.name.into(),
                path,
            })?;
        }
        // A pointer to a function, or a function value, leads to the
        // function's code.
        if let Some(function) = called(value)
            && code.function_entry(function.get()).is_some()
        {
            variable.insert(
                "valueLocationReference".to_owned(),
                references.location(Location::Code(function.get()))?.into(),
            );
        }
        if options.memory {
            // A pointer's natural memory is what it points to.
            let address = match (value, source) {
                (VariableValue::Address(address), _) => Some(address.address),
                (_, VariableValueSource::Memory(address)) => Some(*address),
                _ => None,
            };
            if let Some(address) = address {
                variable.insert(
                    "memoryReference".to_owned(),
                    format!("{:#x}", address.get()).into(),
                );
            }
        }
        let mut attributes = attributes(source, value, context, whole, named);
        if presented.is_some_and(|presentation| presentation.shape == PresentedShape::Text) {
            attributes.push("rawString");
        }
        let kind = match value {
            VariableValue::Record | VariableValue::Union | VariableValue::Variant { .. } => "class",
            _ => "data",
        };
        variable.insert(
            "presentationHint".to_owned(),
            json!({"kind": kind, "attributes": attributes}),
        );
    }
    variable.insert("variablesReference".to_owned(), reference.into());
    Ok(variable)
}

/// The address a pointer or function value would call, if it is one.
const fn called(value: &VariableValue) -> Option<uscope::VirtualAddress> {
    match value {
        VariableValue::Address(address) => Some(address.address),
        VariableValue::Function {
            code: Some(code), ..
        } => Some(*code),
        _ => None,
    }
}

/// How many children a value has, and whether they are all elements
/// (`Some(true)`), all named (`Some(false)`), or a view's elements, which
/// are indexed, followed by its fields and `[raw]`, which are named
/// (`None`). An array's or slice's children are elements, and anything
/// else's named.
fn insert_counts(
    variable: &mut Map<String, Value>,
    children: &uscope::ValueChildrenReference,
    value: &VariableValue,
) -> Option<bool> {
    let total = children.total();
    match (children.elements(), value) {
        (Some(elements), _) => {
            if elements == 0 {
                variable.insert("namedVariables".to_owned(), total.into());
                return Some(false);
            }
            variable.insert("indexedVariables".to_owned(), elements.into());
            variable.insert(
                "namedVariables".to_owned(),
                total.saturating_sub(elements).into(),
            );
            None
        }
        (None, VariableValue::Array { .. } | VariableValue::Slice { .. }) => {
            variable.insert("indexedVariables".to_owned(), total.into());
            Some(true)
        }
        (None, _) => {
            variable.insert("namedVariables".to_owned(), total.into());
            Some(false)
        }
    }
}

/// A value's presentation attributes: whether it can be changed, which
/// numbers, enumerations, and pointers in memory, or whole variables in the
/// innermost frame's registers, can; and whether it is a constant.
fn attributes(
    source: &VariableValueSource,
    value: &VariableValue,
    context: StopContext,
    whole: bool,
    named: bool,
) -> Vec<&'static str> {
    let leaf = matches!(
        value,
        VariableValue::Scalar(_) | VariableValue::Enumeration { .. } | VariableValue::Address(_)
    );
    let storage = match source {
        VariableValueSource::Memory(_) => true,
        VariableValueSource::Register(_) => {
            context.frame == uscope::StackFrameId::INNERMOST && whole
        }
        _ => false,
    };
    let mut attributes = Vec::new();
    if !(leaf && storage && named) {
        attributes.push("readOnly");
    }
    if matches!(source, VariableValueSource::Constant) {
        attributes.push("constant");
    }
    attributes
}

/// Presents a register, which never expands and which expressions read but
/// cannot assign.
pub fn register(value: &RegisterValue, byte_order: uscope::ByteOrder) -> Map<String, Value> {
    let mut variable = Map::new();
    let name = value.register.name.as_ref();
    variable.insert("name".to_owned(), name.into());
    if name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        variable.insert("evaluateName".to_owned(), format!("${name}").into());
    }
    variable.insert(
        "presentationHint".to_owned(),
        json!({"kind": "data", "attributes": ["readOnly"]}),
    );
    variable.insert(
        "value".to_owned(),
        value
            .bytes
            .as_ref()
            .map_or_else(
                || "<not saved>".to_owned(),
                |bytes| register_bytes(bytes, byte_order),
            )
            .into(),
    );
    variable.insert("variablesReference".to_owned(), 0.into());
    variable
}

/// A row that marks where an inspection stopped short, such as at its
/// memory limit.
pub fn truncation(description: String) -> Map<String, Value> {
    let mut variable = Map::new();
    variable.insert("name".to_owned(), "<truncated>".into());
    variable.insert("value".to_owned(), description.into());
    variable.insert("variablesReference".to_owned(), 0.into());
    variable.insert(
        "presentationHint".to_owned(),
        json!({"kind": "virtual", "attributes": ["readOnly"]}),
    );
    variable
}

/// The name a child is shown with.
pub fn child_name(child: &ValueChild) -> String {
    match &child.relationship {
        ValueChildRelationship::ArrayElement { indices, .. } => {
            indices.iter().fold(String::new(), |mut name, index| {
                let _ = write!(name, "[{index}]");
                name
            })
        }
        ValueChildRelationship::SliceElement { index }
        | ValueChildRelationship::Element { index } => {
            format!("[{index}]")
        }
        // A map's entry is named by its key.
        ValueChildRelationship::Entry { key, .. } => summary(Some(&key.type_info), &key.state),
        ValueChildRelationship::Member(member) => {
            member.name.as_deref().unwrap_or("<anonymous>").to_owned()
        }
        ValueChildRelationship::Field { name } => name.to_string(),
        ValueChildRelationship::Raw => "[raw]".to_owned(),
        ValueChildRelationship::Base(_) => format!("<base {}>", child.type_info.name),
        _ => "<child>".to_owned(),
    }
}

/// Whether a child is shown: compiler-generated members are hidden, as the
/// console hides them.
pub const fn shown(child: &ValueChild) -> bool {
    !matches!(&child.relationship, ValueChildRelationship::Member(member) if member.artificial)
}

/// How to evaluate a child again, when its parent can be and the child has
/// a name.
pub fn child_path(
    parent: Option<&uscope::Expression>,
    child: &ValueChild,
) -> Option<uscope::Expression> {
    let parent = parent?;
    match &child.relationship {
        ValueChildRelationship::ArrayElement { indices, .. } => parent.indexed(indices),
        ValueChildRelationship::SliceElement { index }
        | ValueChildRelationship::Element { index } => parent.indexed(&[i128::from(*index)]),
        ValueChildRelationship::Member(member) => parent.member(member.name.as_deref()?),
        // The value as stored is the parent's value.
        ValueChildRelationship::Raw => Some(parent.clone()),
        // Maps are not indexed by key yet, so an entry's value is named by
        // where it is.
        ValueChildRelationship::Entry { .. } => match &child.state {
            VariableState::Available {
                source: VariableValueSource::Memory(address),
                ..
            } => uscope::Expression::at(&child.type_info.name, address.get()),
            _ => None,
        },
        _ => None,
    }
}

/// A value's text: its summary, with integers in hexadecimal when asked.
/// A value a view presents is shown as the view presents it.
pub fn text(type_info: Option<&TypeInfo>, state: &VariableState, hexadecimal: bool) -> String {
    let presented = matches!(
        state,
        VariableState::Available {
            presentation: Some(_),
            ..
        }
    );
    (hexadecimal && !presented)
        .then(|| hex(type_info.and_then(|type_info| type_info.byte_size), state))
        .flatten()
        .unwrap_or_else(|| summary(type_info, state))
}

/// An integer value in hexadecimal, in the width of its type's size.
fn hex(byte_size: Option<u64>, state: &VariableState) -> Option<String> {
    let VariableState::Available { value, .. } = state else {
        return None;
    };
    let bits = byte_size
        .filter(|size| (1..=16).contains(size))
        .map_or(128, |size| size * 8);
    let mask = if bits == 128 {
        u128::MAX
    } else {
        (1_u128 << bits) - 1
    };
    let number = match value {
        VariableValue::Scalar(ScalarValue::Signed(value))
        | VariableValue::Enumeration {
            value: IntegerValue::Signed(value),
            ..
        } => value.cast_unsigned() & mask,
        VariableValue::Scalar(ScalarValue::Unsigned(value))
        | VariableValue::Enumeration {
            value: IntegerValue::Unsigned(value),
            ..
        } => *value & mask,
        _ => return None,
    };
    Some(format!("{number:#x}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hexadecimal_integers_use_their_types_width() {
        let state = |value| VariableState::Available {
            source: VariableValueSource::Computed,
            raw: None,
            value,
            dereference: uscope::DereferenceState::NotApplicable,
            children: ValueChildren::NotApplicable,
            text: None,
            presentation: None,
        };
        let signed = state(VariableValue::Scalar(ScalarValue::Signed(-1)));
        assert_eq!(hex(Some(4), &signed).as_deref(), Some("0xffffffff"));
        assert_eq!(hex(Some(1), &signed).as_deref(), Some("0xff"));
        let unsigned = state(VariableValue::Scalar(ScalarValue::Unsigned(255)));
        assert_eq!(hex(None, &unsigned).as_deref(), Some("0xff"));
        let boolean = state(VariableValue::Scalar(ScalarValue::Boolean(true)));
        assert_eq!(hex(Some(1), &boolean), None);
    }

    /// An integer a view presents shows as the view presents it, even in
    /// hexadecimal: the stored number is one step away, as `[raw]`.
    #[test]
    fn hexadecimal_never_hides_what_a_view_presents() {
        let presentation = uscope::Presentation {
            view: Arc::new(uscope::ViewName {
                source: Arc::from("app.views"),
                line: 2,
                header: Arc::from("c status"),
                extend: false,
            }),
            shape: PresentedShape::Empty,
            count: None,
            summary: Arc::from("ok"),
            children: ValueChildren::NotApplicable,
            problem: None,
        };
        let state = VariableState::Available {
            source: VariableValueSource::Computed,
            raw: None,
            value: VariableValue::Scalar(ScalarValue::Signed(0)),
            dereference: uscope::DereferenceState::NotApplicable,
            children: ValueChildren::NotApplicable,
            text: None,
            presentation: Some(Arc::new(presentation)),
        };
        assert_eq!(text(None, &state, true), text(None, &state, false));
    }
}
