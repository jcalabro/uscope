//! An in-memory code source for tests and fuzzing.

use std::collections::BTreeMap;

use super::{BoundaryEvidence, CodeRead, CodeSource};
use crate::{
    AddressDescription, AddressRange, MemoryReadUnavailableReason, Result, SourceLocation,
    VirtualAddress,
};

/// Bytes at a base address, readable only within the given ranges, with
/// known instruction starts.
pub struct FakeSource {
    pub base: u64,
    pub bytes: Vec<u8>,
    /// Readable address ranges; every other address is unreadable.
    pub readable: Vec<AddressRange<u64>>,
    pub starts: BTreeMap<VirtualAddress, BoundaryEvidence>,
}

impl FakeSource {
    /// Makes every byte readable.
    pub fn new(base: u64, bytes: Vec<u8>) -> Self {
        let end = base + bytes.len() as u64;
        Self {
            base,
            bytes,
            readable: vec![AddressRange { start: base, end }],
            starts: BTreeMap::new(),
        }
    }

    pub fn start(mut self, address: u64, evidence: BoundaryEvidence) -> Self {
        self.starts.insert(VirtualAddress::new(address), evidence);
        self
    }

    pub fn byte(&self, address: u64) -> Option<u8> {
        if !self.readable.iter().any(|range| range.contains(address)) {
            return None;
        }
        let offset = usize::try_from(address.checked_sub(self.base)?).ok()?;
        self.bytes.get(offset).copied()
    }
}

impl CodeSource for FakeSource {
    fn read(&mut self, address: VirtualAddress, size: usize) -> Result<CodeRead> {
        let bytes = (0..size as u64)
            .map_while(|offset| self.byte(address.get().checked_add(offset)?))
            .collect::<Vec<_>>();
        let unreadable = (bytes.len() < size).then_some(MemoryReadUnavailableReason::Inaccessible);
        Ok(CodeRead { bytes, unreadable })
    }

    fn instruction_starts(
        &self,
        range: AddressRange<VirtualAddress>,
    ) -> BTreeMap<VirtualAddress, BoundaryEvidence> {
        self.starts
            .range(range.start..range.end)
            .map(|(address, evidence)| (*address, *evidence))
            .collect()
    }

    fn describe(&self, address: VirtualAddress) -> AddressDescription {
        AddressDescription {
            address,
            module: None,
        }
    }

    fn source_location(&self, _address: VirtualAddress) -> Option<SourceLocation> {
        None
    }
}
