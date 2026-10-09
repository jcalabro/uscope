//! A future's chain of awaits, read from memory: from a task's root future
//! through each coroutine's awaited future to the leaf it waits on.
//!
//! The walk follows no runtime's scheduling. It knows the shapes futures
//! take in debug information: a coroutine, whose state says where it waits
//! and which of its members it awaits; a pointer to a future, such as a
//! `Box`; a pinned one, as Rust's `Pin` wraps it; a trait object, whose
//! vtable says what it holds; a record whose only member is a coroutine,
//! which can await nothing but that coroutine; and the few records known
//! by name to hold a future beside what is none, such as tracing's
//! `Instrumented`. Any other future is a leaf, which the runtime or a view
//! describes. Every way the walk can end is said, never guessed past.

use std::collections::BTreeSet;
use std::sync::Arc;

use super::{RuntimeImage, RuntimeStop, records};
use crate::{
    CoroutineStateKind, ImageAddress, RecordMemberLayout, SourceLanguage, SourceLocation, TypeKind,
    TypeReference, VirtualAddress,
};

/// The most futures one chain is followed through, so that corrupted
/// memory cannot make the walk run on.
pub const MAX_DEPTH: usize = 256;
/// The most wrappers followed to reach a type's representation.
const MAX_WRAPPERS: usize = 16;
/// The Rust records that hold a future beside what is no future, by their
/// path and name, with the member that holds it: the spans tracing keeps
/// around futures. In a build with `tokio_unstable` and tokio's `tracing`
/// feature, as `tokio-console` needs, tracing's `Instrumented` wraps every
/// task tokio spawns, keeping its future in std's `ManuallyDrop` (which
/// recent releases of Rust lay out around a `MaybeDangling`), and tokio's
/// `InstrumentedAsyncOp` wraps what its locks, semaphores, and barriers
/// are awaited through.
const HOLDERS: [(&[&str], &str, &str); 5] = [
    (&["tracing", "instrument"], "Instrumented", "inner"),
    (&["tracing", "instrument"], "WithDispatch", "inner"),
    (&["core", "mem", "manually_drop"], "ManuallyDrop", "value"),
    (&["core", "mem", "maybe_dangling"], "MaybeDangling", "__0"),
    (&["tokio", "util", "trace"], "InstrumentedAsyncOp", "inner"),
];

/// One future of a chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsyncFrame {
    /// Where the future is.
    pub object: VirtualAddress,
    pub ty: TypeReference,
    pub kind: AsyncFrameKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AsyncFrameKind {
    /// A coroutine, in the state with this number: where it waits, has
    /// not begun, or has ended.
    Coroutine {
        state: u64,
        kind: CoroutineStateKind,
        location: Option<SourceLocation>,
    },
    /// A future that is no coroutine, which the chain waits on.
    Leaf,
}

/// A chain of futures, the innermost first, and why it ends where it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwaitChain {
    pub frames: Vec<AsyncFrame>,
    pub end: ChainEnd,
}

/// Why a chain ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainEnd {
    /// At a leaf, the future the chain waits on.
    Leaf,
    /// At a coroutine that has not begun, and so awaits nothing.
    Unresumed,
    /// At a coroutine that has returned or panicked.
    Finished,
    /// At a suspended coroutine whose awaited future its debug information
    /// does not name.
    NoAwaitee,
    /// The chain could not be followed further, for this reason.
    Broken(Arc<str>),
    /// The chain went on past [`MAX_DEPTH`] futures.
    TooDeep,
    /// The chain came back to a future it had passed.
    Cycle,
}

/// The chain of awaits that begins at the future of type `ty` at `future`.
pub fn walk(
    image: &dyn RuntimeImage,
    stop: &dyn RuntimeStop,
    future: VirtualAddress,
    ty: TypeReference,
) -> AwaitChain {
    let mut frames = Vec::new();
    let end = follow(image, stop, future.get(), ty, &mut frames);
    frames.reverse();
    AwaitChain { frames, end }
}

fn follow(
    image: &dyn RuntimeImage,
    stop: &dyn RuntimeStop,
    mut object: u64,
    mut ty: TypeReference,
    frames: &mut Vec<AsyncFrame>,
) -> ChainEnd {
    let mut seen = BTreeSet::new();
    for _ in 0..MAX_DEPTH {
        if !seen.insert((object, ty)) {
            return ChainEnd::Cycle;
        }
        let Some((representation, kind)) = representation(image, ty) else {
            return ChainEnd::Broken(format!("type {:?} has no layout", ty.id).into());
        };
        if let Some(coroutine) = image.coroutine(representation) {
            let coroutine = match coroutine {
                Ok(coroutine) => coroutine,
                Err(reason) => return ChainEnd::Broken(Arc::clone(reason)),
            };
            let Ok(size) = usize::try_from(coroutine.state.size) else {
                return ChainEnd::Broken("a coroutine's state is too large".into());
            };
            let at = object.wrapping_add(coroutine.state.offset);
            let Some(number) = records::read(stop, at, size) else {
                return ChainEnd::Broken(format!("the future at {object:#x} is unreadable").into());
            };
            let Some(state) = coroutine.state(number) else {
                return ChainEnd::Broken(
                    format!("the future at {object:#x} is in no state, {number}").into(),
                );
            };
            frames.push(AsyncFrame {
                object: VirtualAddress::new(object),
                ty,
                kind: AsyncFrameKind::Coroutine {
                    state: number,
                    kind: state.kind,
                    location: state.location.clone(),
                },
            });
            match state.kind {
                CoroutineStateKind::Unresumed => return ChainEnd::Unresumed,
                CoroutineStateKind::Returned | CoroutineStateKind::Panicked => {
                    return ChainEnd::Finished;
                }
                CoroutineStateKind::Suspended { .. } => {}
            }
            let Some(awaitee) = state.awaitee() else {
                return ChainEnd::NoAwaitee;
            };
            let RecordMemberLayout::ByteOffset(offset) = awaitee.layout else {
                return ChainEnd::Broken("an awaited future is not at a byte offset".into());
            };
            let Some(inner) = object.checked_add(offset) else {
                return ChainEnd::Broken(
                    format!("the future at {object:#x} awaits no address").into(),
                );
            };
            (object, ty) = (inner, awaitee.type_ref);
            continue;
        }
        match kind {
            Shape::Pointer(target) => {
                let Some(pointer) = records::word(stop, object) else {
                    return ChainEnd::Broken(
                        format!("the pointer at {object:#x} is unreadable").into(),
                    );
                };
                (object, ty) = (pointer, target);
            }
            Shape::Pinned { pointer, layout } => {
                let RecordMemberLayout::ByteOffset(offset) = layout else {
                    return ChainEnd::Broken("a pinned pointer is not at a byte offset".into());
                };
                (object, ty) = (object.wrapping_add(offset), pointer);
            }
            Shape::Wrapper { future, offset } => {
                (object, ty) = (object.wrapping_add(offset), future);
            }
            Shape::TraitObject { data, vtable } => match held(image, stop, object, data, vtable) {
                Ok(held) => (object, ty) = held,
                Err(end) => return end,
            },
            Shape::Other => {
                frames.push(AsyncFrame {
                    object: VirtualAddress::new(object),
                    ty,
                    kind: AsyncFrameKind::Leaf,
                });
                return ChainEnd::Leaf;
            }
        }
    }
    ChainEnd::TooDeep
}

/// The future the trait object at `object` holds, with its data pointer
/// and vtable at those offsets, and its type, which the vtable says.
fn held(
    image: &dyn RuntimeImage,
    stop: &dyn RuntimeStop,
    object: u64,
    data: u64,
    vtable: u64,
) -> Result<(u64, TypeReference), ChainEnd> {
    let (Some(pointer), Some(table)) = (
        records::word(stop, object.wrapping_add(data)),
        records::word(stop, object.wrapping_add(vtable)),
    ) else {
        return Err(ChainEnd::Broken(
            format!("the trait object at {object:#x} is unreadable").into(),
        ));
    };
    let held = ImageAddress::new(table.wrapping_sub(stop.load_bias()));
    let Some(held) = image.trait_object_type(held) else {
        return Err(ChainEnd::Broken(
            format!("the vtable at {table:#x} is no future's the program describes").into(),
        ));
    };
    Ok((pointer, held))
}

/// How the walk passes through a future of some type.
enum Shape {
    /// A pointer to the future of the target type.
    Pointer(TypeReference),
    /// A pin around a pointer of this type, which lies there.
    Pinned {
        pointer: TypeReference,
        layout: RecordMemberLayout,
    },
    /// A trait object: where its data pointer and its vtable lie.
    TraitObject { data: u64, vtable: u64 },
    /// A record that holds a future of this type at this offset: one that
    /// holds only a coroutine, or one of [`HOLDERS`].
    Wrapper { future: TypeReference, offset: u64 },
    /// Anything else, which is a leaf unless it is a coroutine.
    Other,
}

/// The type that lays out a value of `ty`, through names and qualifiers,
/// and how the walk passes through it.
fn representation(
    image: &dyn RuntimeImage,
    mut ty: TypeReference,
) -> Option<(TypeReference, Shape)> {
    for _ in 0..MAX_WRAPPERS {
        let info = image.type_info(ty)?;
        let shape = match &info.kind {
            TypeKind::Named { target, .. } => {
                ty = (*target)?;
                continue;
            }
            TypeKind::Modified { target, .. } => {
                ty = *target;
                continue;
            }
            TypeKind::Pointer {
                target: Some(target),
                ..
            }
            | TypeKind::Reference { target, .. } => Shape::Pointer(*target),
            TypeKind::Record { members, .. } if holder(info).is_some() => members
                .iter()
                .find(|member| member.name.as_deref() == holder(info))
                .and_then(|member| match member.layout {
                    RecordMemberLayout::ByteOffset(offset) => Some(Shape::Wrapper {
                        future: member.type_ref,
                        offset,
                    }),
                    _ => None,
                })
                .unwrap_or(Shape::Other),
            TypeKind::Record { members, .. } => {
                let pinned = is_pin(info);
                let member = |name: &str| {
                    members
                        .iter()
                        .find(|member| member.name.as_deref() == Some(name))
                };
                match (pinned, &members[..], member("pointer"), member("vtable")) {
                    (true, [only], ..) => Shape::Pinned {
                        pointer: only.type_ref,
                        layout: only.layout,
                    },
                    (false, [only], ..) => match only.layout {
                        RecordMemberLayout::ByteOffset(offset)
                            if is_coroutine(image, only.type_ref) =>
                        {
                            Shape::Wrapper {
                                future: only.type_ref,
                                offset,
                            }
                        }
                        _ => Shape::Other,
                    },
                    (false, [_, _], Some(pointer), Some(vtable)) => {
                        match (
                            pointer.layout,
                            vtable.layout,
                            points_to_dyn(image, pointer.type_ref),
                        ) {
                            (
                                RecordMemberLayout::ByteOffset(data),
                                RecordMemberLayout::ByteOffset(vtable),
                                true,
                            ) => Shape::TraitObject { data, vtable },
                            _ => Shape::Other,
                        }
                    }
                    _ => Shape::Other,
                }
            }
            _ => Shape::Other,
        };
        return Some((ty, shape));
    }
    None
}

/// Whether a value of type `ty` is a coroutine, through names and
/// qualifiers.
fn is_coroutine(image: &dyn RuntimeImage, mut ty: TypeReference) -> bool {
    for _ in 0..MAX_WRAPPERS {
        match image.type_info(ty).map(|info| &info.kind) {
            Some(
                TypeKind::Named {
                    target: Some(target),
                    ..
                }
                | TypeKind::Modified { target, .. },
            ) => ty = *target,
            Some(_) => return image.coroutine(ty).is_some(),
            None => return false,
        }
    }
    false
}

/// Whether a value of type `ty` is a pinned pointer to a future, which
/// stays where it is until it is dropped: what a variable that holds a
/// future being polled must be, since the memory a future moved from goes
/// on looking like a future.
pub fn pinned(image: &dyn RuntimeImage, ty: TypeReference) -> bool {
    representation(image, ty)
        .and_then(|(ty, _)| image.type_info(ty))
        .is_some_and(is_pin)
}

/// Whether a type is Rust's `Pin`.
fn is_pin(info: &crate::TypeInfo) -> bool {
    info.identity.as_ref().is_some_and(|identity| {
        identity.language == SourceLanguage::Rust
            && identity.base.as_ref() == "Pin"
            && identity.path.iter().map(AsRef::as_ref).eq(["core", "pin"])
    })
}

/// The member that holds the future, if a type is one of [`HOLDERS`].
fn holder(info: &crate::TypeInfo) -> Option<&'static str> {
    let identity = info.identity.as_ref()?;
    if identity.language != SourceLanguage::Rust {
        return None;
    }
    let path = identity.path.iter().map(AsRef::as_ref);
    HOLDERS.iter().find_map(|&(within, base, member)| {
        (identity.base.as_ref() == base && path.clone().eq(within.iter().copied()))
            .then_some(member)
    })
}

/// Whether a pointer type points to a trait object's data.
fn points_to_dyn(image: &dyn RuntimeImage, ty: TypeReference) -> bool {
    let Some(TypeKind::Pointer {
        target: Some(target),
        ..
    }) = image.type_info(ty).map(|info| &info.kind)
    else {
        return false;
    };
    image
        .type_info(*target)
        .is_some_and(|target| target.name.starts_with("dyn "))
}

#[cfg(test)]
mod tests;
