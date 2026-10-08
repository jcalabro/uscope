//! The variables of a running async body. rustc keeps a variable in the
//! body's future only across the awaits it is live across; elsewhere the
//! variable is on the stack, or in a part of the future other states
//! reuse. While the body runs, its future's state number still says which
//! await this poll resumed from, until the body suspends again: a variable
//! last written before that await, which the await's state does not keep,
//! holds whatever other polls left in its storage.

use crate::debug_info::VariableRuntime;
use crate::inspection::InspectionBudget;
use crate::model::ValueStorage;
use crate::{
    CodeInstanceId, CoroutineState, CoroutineStateKind, ImageAddress, RecordMemberLayout, Variable,
    VariableState, VariableUnavailableReason, VariableValueSource, VirtualAddress,
};

use super::codec::unsigned_value;
use super::evaluate::FrameBaseCache;
use super::{CatalogDataObject, CatalogFunction, DwarfVariableInfo};

/// The await a running async body resumed from.
pub(super) struct Resumption<'a> {
    /// Where its future begins.
    object: VirtualAddress,
    /// The future's size.
    size: u64,
    /// The suspended state the poll began in.
    state: &'a CoroutineState,
}

impl DwarfVariableInfo {
    /// The await the async body running at `address` resumed from, or
    /// `None` when the code there runs no async body, the poll began at the
    /// body's start, or the future cannot be read.
    pub(super) fn resumption(
        &self,
        function: &CatalogFunction,
        address: ImageAddress,
        selected: Option<CodeInstanceId>,
        runtime: &mut dyn VariableRuntime,
        frame_base: &mut FrameBaseCache,
        budget: &mut InspectionBudget,
    ) -> Option<Resumption<'_>> {
        let (future, ty) = function.objects.iter().find_map(|&index| {
            let object = &self.objects[index];
            (object.instance == selected
                && object.ranges.iter().any(|range| range.contains(address)))
            .then_some(())?;
            Some((object, object.coroutine?))
        })?;
        let coroutine = self.coroutines.get(&ty)?;
        let size = self.type_info(ty).ok()?.byte_size?;
        let ValueStorage::Memory(object) = self
            .located_data_object(future, Some(address), runtime, frame_base, budget)
            .ok()?
        else {
            return None;
        };
        let state_size = usize::try_from(coroutine.state.size).ok()?;
        budget.consume_memory(state_size).ok()?;
        let raw = runtime
            .read_memory(
                VirtualAddress::new(object.get().checked_add(coroutine.state.offset)?),
                state_size,
            )
            .ok()?;
        let value = u64::try_from(unsigned_value(&raw, self.target.byte_order).ok()?).ok()?;
        let state = coroutine.states.iter().find(|state| state.value == value)?;
        matches!(state.kind, CoroutineStateKind::Suspended { .. }).then_some(Resumption {
            object,
            size,
            state,
        })
    }
}

impl Resumption<'_> {
    /// Marks `variable` as holding no value when the body last wrote it
    /// before the await it resumed from, and the await did not keep it:
    /// it is declared on an earlier line of the await's file, and is either
    /// in the future where the await's state holds no variable of its name,
    /// or in a stack slot whose one location covers the whole function.
    pub(super) fn check(&self, catalog: &CatalogDataObject, variable: &mut Variable) {
        let (Some(resumed), Some(declared)) = (&self.state.location, &catalog.declaration) else {
            return;
        };
        if declared.file != resumed.file || declared.line >= resumed.line {
            return;
        }
        let (VariableState::Available { source, .. } | VariableState::Invalid { source, .. }) =
            &variable.state
        else {
            return;
        };
        let stale = match source {
            VariableValueSource::Memory(address)
                if address.get() >= self.object.get()
                    && address.get() - self.object.get() < self.size =>
            {
                let offset = address.get() - self.object.get();
                !self.state.saved.iter().any(|member| {
                    member.name.as_deref() == Some(&*catalog.name)
                        && member.layout == RecordMemberLayout::ByteOffset(offset)
                })
            }
            // A location list says where the variable is at each address,
            // which the compiler knows better than this does.
            _ => catalog.single_location(),
        };
        if stale {
            variable.state =
                VariableState::Unavailable(VariableUnavailableReason::NotSavedAcrossAwait {
                    line: resumed.line,
                });
        }
    }
}
