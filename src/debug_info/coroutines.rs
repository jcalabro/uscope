//! The state machines compilers generate for code that can suspend, read
//! once from the normalized types into [`CoroutineInfo`].
//!
//! rustc describes the future of an `async fn` or `async` block as a type
//! named `{async_fn_env#N}`, `{async_block_env#N}`, or
//! `{async_closure_env#N}`: a variant part keyed on an artificial
//! `__state` member, each of whose variants holds one member, named by its
//! number, of a record named for the state. `Unresumed` holds the captures
//! alone; `Returned` and `Panicked` hold them too; `SuspendN` holds the
//! variables live across the `N`th await, the future it awaits in
//! `__awaitee`, and the captures, which come last. Each state's member
//! carries the source line the state is at: the function's header before
//! it starts, an await's line, or the closing brace once it has finished.
//!
//! A type that is named as a coroutine but laid out otherwise is refused
//! with the reason, never read by guess.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::{
    CoroutineInfo, CoroutineKind, CoroutineState, CoroutineStateKind, IntegerValue,
    RecordMemberLayout, StateMember, TypeId, TypeInfo, TypeKind, TypeNode, VariantDiscriminant,
    VariantSelection, VariantSelector,
};

/// What kind of coroutine a type's name says it is, for rustc's names.
pub fn coroutine_kind(name: &str) -> Option<CoroutineKind> {
    let name = name.rsplit("::").next().unwrap_or(name);
    let numbered = |prefix: &str| {
        name.strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix('}'))
            .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
    };
    if numbered("{async_fn_env#") {
        Some(CoroutineKind::AsyncFunction)
    } else if numbered("{async_block_env#") {
        Some(CoroutineKind::AsyncBlock)
    } else if numbered("{async_closure_env#") {
        Some(CoroutineKind::AsyncClosure)
    } else {
        None
    }
}

/// Every type its name says is a coroutine, with what it is or why its
/// layout cannot be read as one.
pub fn normalize(types: &[TypeNode]) -> BTreeMap<TypeId, Result<CoroutineInfo, Arc<str>>> {
    let by_id = |id: TypeId| match types.get(id.index()) {
        Some(TypeNode::Resolved(info)) => Some(info),
        _ => None,
    };
    types
        .iter()
        .filter_map(|node| match node {
            TypeNode::Resolved(info) => {
                let kind = coroutine_kind(&info.name)?;
                Some((info.reference.id, coroutine(info, kind, &by_id)))
            }
            TypeNode::Malformed { .. } => None,
        })
        .collect()
}

fn coroutine<'a>(
    info: &TypeInfo,
    kind: CoroutineKind,
    types: &impl Fn(TypeId) -> Option<&'a TypeInfo>,
) -> Result<CoroutineInfo, Arc<str>> {
    let TypeKind::Variant {
        discriminant,
        variants,
        incomplete: false,
        ..
    } = &info.kind
    else {
        return Err("the coroutine has no variant part".into());
    };
    let VariantDiscriminant::Stored(state) = discriminant.as_ref() else {
        return Err("the coroutine stores no state number".into());
    };
    if state.name.as_deref() != Some("__state") || !state.artificial {
        return Err("the coroutine's discriminant is not its `__state`".into());
    }
    let RecordMemberLayout::ByteOffset(offset) = state.layout else {
        return Err("the coroutine's state number has no byte offset".into());
    };
    let size = types(state.type_ref.id)
        .and_then(|ty| ty.byte_size)
        .filter(|size| (1..=8).contains(size))
        .ok_or("the coroutine's state number has no integer size")?;

    let mut states = variants
        .iter()
        .map(|variant| read_state(variant, types))
        .collect::<Result<Vec<_>, Arc<str>>>()?;
    // `Unresumed` holds the captures alone; every other state ends with
    // them, by name.
    let unresumed = states
        .iter_mut()
        .find(|state| state.kind == CoroutineStateKind::Unresumed)
        .ok_or("the coroutine has no `Unresumed` state")?;
    let captures = std::mem::replace(&mut unresumed.saved, Arc::from([]));
    for state in states
        .iter_mut()
        .filter(|state| state.kind != CoroutineStateKind::Unresumed)
    {
        let value = state.value;
        let tail = state
            .saved
            .len()
            .checked_sub(captures.len())
            .ok_or_else(|| format!("state {value} holds fewer members than the captures"))?;
        if state.saved[tail..]
            .iter()
            .zip(captures.iter())
            .any(|(held, captured)| held.name != captured.name)
        {
            return Err(format!("state {value} does not end with the captures").into());
        }
        state.saved = state.saved[..tail].into();
    }
    let mut numbers = states.iter().map(|state| state.value).collect::<Vec<_>>();
    numbers.sort_unstable();
    numbers.dedup();
    if numbers.len() != states.len() {
        return Err("two states share a number".into());
    }
    let mut suspended = states
        .iter()
        .filter_map(|state| match state.kind {
            CoroutineStateKind::Suspended { index } => Some(index),
            _ => None,
        })
        .collect::<Vec<_>>();
    suspended.sort_unstable();
    if suspended
        .iter()
        .enumerate()
        .any(|(at, index)| u32::try_from(at) != Ok(*index))
    {
        return Err("the suspended states are not numbered in order".into());
    }
    Ok(CoroutineInfo {
        kind,
        state: StateMember { offset, size },
        states: states.into(),
        captures,
    })
}

/// The coroutine a `Pin<&mut C>` parameter points to, as the body of an
/// `async fn` is passed its future.
pub fn pinned_coroutine(types: &[TypeNode], ty: TypeId) -> Option<TypeId> {
    let resolved = |id: TypeId| match types.get(id.index()) {
        Some(TypeNode::Resolved(info)) => Some(info),
        _ => None,
    };
    let pin = resolved(ty)?;
    if !pin.name.starts_with("Pin<") {
        return None;
    }
    let TypeKind::Record { members, .. } = &pin.kind else {
        return None;
    };
    let [pointer] = members.as_ref() else {
        return None;
    };
    let target = match &resolved(pointer.type_ref.id)?.kind {
        TypeKind::Pointer {
            target: Some(target),
            ..
        }
        | TypeKind::Reference { target, .. } => *target,
        _ => return None,
    };
    coroutine_kind(&resolved(target.id)?.name).map(|_| target.id)
}

/// One state, with every member it holds, laid out from the coroutine's
/// start.
fn read_state<'a>(
    variant: &crate::Variant,
    types: &impl Fn(TypeId) -> Option<&'a TypeInfo>,
) -> Result<CoroutineState, Arc<str>> {
    let value = match &variant.selection {
        VariantSelection::Selectors(selectors) => match selectors.as_ref() {
            [VariantSelector::Value(IntegerValue::Unsigned(value))] => {
                u64::try_from(*value).map_err(|_| "a state number is too large")?
            }
            [VariantSelector::Value(IntegerValue::Signed(value))] => {
                u64::try_from(*value).map_err(|_| "a state number is negative")?
            }
            _ => return Err("a state is not selected by one number".into()),
        },
        VariantSelection::Default => return Err("a state is the default variant".into()),
    };
    let [member] = variant.members.as_ref() else {
        return Err(format!("state {value} does not hold exactly one member").into());
    };
    let RecordMemberLayout::ByteOffset(base) = member.layout else {
        return Err(format!("state {value} has no byte offset").into());
    };
    let record = types(member.type_ref.id).ok_or("a state's type is unresolved")?;
    let kind = state_kind(&record.name)
        .ok_or_else(|| format!("state {value} is named `{}`", record.name))?;
    let TypeKind::Record { members, .. } = &record.kind else {
        return Err(format!("state {value} is not a record").into());
    };
    let members = members
        .iter()
        .map(|inner| {
            let RecordMemberLayout::ByteOffset(at) = inner.layout else {
                return Err(Arc::from(format!(
                    "a member of state {value} has no byte offset"
                )));
            };
            let mut moved = inner.clone();
            moved.layout = RecordMemberLayout::ByteOffset(
                base.checked_add(at).ok_or("a member's offset overflows")?,
            );
            Ok(moved)
        })
        .collect::<Result<Vec<_>, Arc<str>>>()?;
    Ok(CoroutineState {
        value,
        kind,
        location: member.declaration.clone(),
        saved: members.into(),
    })
}

/// What a state's record name says it is.
fn state_kind(name: &str) -> Option<CoroutineStateKind> {
    match name {
        "Unresumed" => Some(CoroutineStateKind::Unresumed),
        "Returned" => Some(CoroutineStateKind::Returned),
        "Panicked" => Some(CoroutineStateKind::Panicked),
        _ => {
            let index = name.strip_prefix("Suspend")?;
            (!index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()))
                .then(|| index.parse().ok())
                .flatten()
                .map(|index| CoroutineStateKind::Suspended { index })
        }
    }
}

/// The name a debugger shows for the function that runs a coroutine,
/// which rustc names `{async_fn#N}`, `{async_block#N}`, or
/// `{async_closure#N}` within the namespace of the function that wrote it:
/// an `async fn`'s body is that function, and a block or closure is
/// numbered within it. `namespace` is the enclosing names, outermost
/// first.
pub fn body_name(name: &str, namespace: &[Arc<str>]) -> Option<Arc<str>> {
    let numbered = |prefix: &str| {
        name.strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix('}'))
            .filter(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
    };
    let enclosing = namespace.last()?;
    if numbered("{async_fn#").is_some() {
        Some(Arc::clone(enclosing))
    } else if let Some(number) = numbered("{async_block#") {
        Some(format!("{enclosing}::{{async block#{number}}}").into())
    } else {
        numbered("{async_closure#")
            .map(|number| format!("{enclosing}::{{async closure#{number}}}").into())
    }
}

/// Whether rustc names a function as the body of an `async fn`.
pub fn is_async_fn_body(name: &str) -> bool {
    name.strip_prefix("{async_fn#")
        .and_then(|rest| rest.strip_suffix('}'))
        .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies_are_named_for_the_functions_that_wrote_them() {
        let namespace = ["steps".into(), "leaf".into()];
        assert_eq!(
            body_name("{async_fn#0}", &namespace).as_deref(),
            Some("leaf")
        );
        assert_eq!(
            body_name("{async_block#2}", &namespace).as_deref(),
            Some("leaf::{async block#2}")
        );
        assert_eq!(
            body_name("{async_closure#0}", &namespace).as_deref(),
            Some("leaf::{async closure#0}")
        );
        assert_eq!(body_name("{closure#0}", &namespace), None);
        assert_eq!(body_name("{async_fn#}", &namespace), None);
        assert_eq!(body_name("{async_fn#0}", &[]), None);
        assert_eq!(
            coroutine_kind("{async_block_env#1}"),
            Some(CoroutineKind::AsyncBlock)
        );
        assert_eq!(coroutine_kind("{closure_env#1}"), None);
    }
}
