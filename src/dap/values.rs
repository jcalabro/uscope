//! Values as the client's variables.
//!
//! A value's text is the same summary the console's `print` shows for a
//! variable. Values without one, such as optimized-out variables, are still
//! variables whose text explains why. Aggregates expand to their children,
//! and pointers expand to what they point to.

use std::fmt::Write as _;

use serde_json::{Map, Value, json};
use uscope::{
    IntegerValue, RegisterValue, ScalarValue, StopContext, TypeInfo, ValueChild,
    ValueChildRelationship, ValueChildren, VariableState, VariableValue, VariableValueSource,
};

use super::handles::{Exhausted, References, Variables};
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

/// One value to present.
pub struct Item<'a> {
    pub name: &'a str,
    /// How to evaluate the value again, when it can be.
    pub path: Option<uscope::Expression>,
    pub type_info: Option<&'a TypeInfo>,
    pub state: &'a VariableState,
}

/// Presents a value as a `Variable`, with a reference that expands it.
pub fn variable(
    item: Item<'_>,
    context: StopContext,
    options: Options,
    references: &mut References,
) -> Result<Map<String, Value>, Exhausted> {
    let mut variable = Map::new();
    variable.insert("name".to_owned(), item.name.into());
    let text = options
        .hex
        .then(|| {
            hex(
                item.type_info.and_then(|type_info| type_info.byte_size),
                item.state,
            )
        })
        .flatten()
        .unwrap_or_else(|| summary(item.type_info, item.state));
    variable.insert("value".to_owned(), text.into());
    if options.types {
        variable.insert(
            "type".to_owned(),
            item.type_info
                .map_or("<unknown type>", |type_info| &type_info.name)
                .into(),
        );
    }
    let path = item.path;
    let named = path.is_some();
    let whole = path.as_ref().is_some_and(uscope::Expression::is_name);
    if let Some(path) = &path {
        variable.insert("evaluateName".to_owned(), path.to_string().into());
    }
    let mut reference = 0;
    if let VariableState::Available {
        source,
        value,
        dereference,
        children,
        ..
    } = item.state
    {
        if let ValueChildren::Available(children) = children {
            let total = children.total();
            match value {
                VariableValue::Array { .. } | VariableValue::Slice { .. } => {
                    variable.insert("indexedVariables".to_owned(), total.into());
                }
                _ => {
                    variable.insert("namedVariables".to_owned(), total.into());
                }
            }
            reference = references.variables(Variables::Children {
                context,
                reference: children.clone(),
                path,
            })?;
        } else if let uscope::DereferenceState::Available(dereference) = dereference {
            reference = references.variables(Variables::Pointee {
                context,
                reference: dereference.clone(),
                name: item.name.into(),
                path,
            })?;
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
        let attributes = attributes(source, value, context, whole, named);
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

/// Presents a register, which never expands.
pub fn register(value: &RegisterValue, byte_order: uscope::ByteOrder) -> Map<String, Value> {
    let mut variable = Map::new();
    variable.insert("name".to_owned(), value.register.name.as_ref().into());
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
        ValueChildRelationship::SliceElement { index } => format!("[{index}]"),
        ValueChildRelationship::Member(member) => {
            member.name.as_deref().unwrap_or("<anonymous>").to_owned()
        }
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
        ValueChildRelationship::SliceElement { index } => parent.indexed(&[i128::from(*index)]),
        ValueChildRelationship::Member(member) => parent.member(member.name.as_deref()?),
        _ => None,
    }
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
        };
        let signed = state(VariableValue::Scalar(ScalarValue::Signed(-1)));
        assert_eq!(hex(Some(4), &signed).as_deref(), Some("0xffffffff"));
        assert_eq!(hex(Some(1), &signed).as_deref(), Some("0xff"));
        let unsigned = state(VariableValue::Scalar(ScalarValue::Unsigned(255)));
        assert_eq!(hex(None, &unsigned).as_deref(), Some("0xff"));
        let boolean = state(VariableValue::Scalar(ScalarValue::Boolean(true)));
        assert_eq!(hex(Some(1), &boolean), None);
    }
}
