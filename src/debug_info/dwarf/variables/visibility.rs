//! Where Go's variables are visible. Go describes a local's scope by its
//! block's code, which unoptimized builds do not divide into blocks, so a
//! local would seem to exist before its declaration has run. As Delve
//! does, a Go local is visible only where the line executing is past the
//! line declaring it, and a block's code includes its nested blocks'.

use std::collections::HashMap;

use crate::model::LineEntry;
use crate::{
    AddressRange, CodeInstanceId, CodeInstanceInfo, CodeInstanceKind, ImageAddress, SourceLocation,
};

/// The source line each address executes, by address.
pub(super) struct LineIndex {
    /// Non-empty line ranges sorted by start.
    rows: Vec<(AddressRange<ImageAddress>, SourceLocation)>,
}

impl LineIndex {
    pub(super) fn new(lines: &[LineEntry]) -> Self {
        let mut rows = lines
            .iter()
            .filter(|line| line.range.start < line.range.end)
            .map(|line| (line.range, line.location.clone()))
            .collect::<Vec<_>>();
        rows.sort_by_key(|(range, _)| range.start);
        Self { rows }
    }

    fn at(&self, address: ImageAddress) -> Option<&SourceLocation> {
        let after = self
            .rows
            .partition_point(|(range, _)| range.start <= address);
        let (range, location) = self.rows.get(after.checked_sub(1)?)?;
        range.contains(address).then_some(location)
    }

    /// The row boundaries within `range`.
    fn boundaries(&self, range: AddressRange<ImageAddress>) -> impl Iterator<Item = ImageAddress> {
        let first = self.rows.partition_point(|(row, _)| row.end <= range.start);
        self.rows[first..]
            .iter()
            .take_while(move |(row, _)| row.start < range.end)
            .flat_map(|(row, _)| [row.start, row.end])
    }
}

/// The code of calls inlined into one instance, with each call's location.
type Calls = Vec<(AddressRange<ImageAddress>, Option<SourceLocation>)>;

/// The calls inlined directly into each code instance.
pub(super) struct InlineCalls {
    calls: HashMap<CodeInstanceId, Calls>,
}

impl InlineCalls {
    pub(super) fn new(instances: &[CodeInstanceInfo]) -> Self {
        let mut calls = HashMap::<CodeInstanceId, Vec<_>>::new();
        for instance in instances {
            let (Some(parent), CodeInstanceKind::Inline { call_site }) =
                (instance.parent, &instance.kind)
            else {
                continue;
            };
            let entry = calls.entry(parent).or_default();
            for range in instance.ranges.iter() {
                entry.push((*range, call_site.clone()));
            }
        }
        Self { calls }
    }

    pub(super) fn within(
        &self,
        instance: Option<CodeInstanceId>,
    ) -> &[(AddressRange<ImageAddress>, Option<SourceLocation>)] {
        instance
            .and_then(|instance| self.calls.get(&instance))
            .map_or(&[], Vec::as_slice)
    }
}

/// The parts of `ranges` where the line executing in the variable's
/// function is past `declaration`'s: the call's line within a call
/// inlined there, and otherwise the line of the code itself. Code whose
/// line is unknown, or in another file, stays visible: hiding it would be
/// a guess.
pub(super) fn after_declaration(
    ranges: &[AddressRange<ImageAddress>],
    declaration: &SourceLocation,
    lines: &LineIndex,
    calls: &[(AddressRange<ImageAddress>, Option<SourceLocation>)],
) -> Vec<AddressRange<ImageAddress>> {
    let mut visible = Vec::<AddressRange<ImageAddress>>::new();
    for range in ranges {
        let mut boundaries = lines
            .boundaries(*range)
            .chain(calls.iter().flat_map(|(call, _)| [call.start, call.end]))
            .filter(|address| range.start < *address && *address < range.end)
            .chain([range.start, range.end])
            .collect::<Vec<_>>();
        boundaries.sort_unstable();
        boundaries.dedup();
        for segment in boundaries.windows(2) {
            let (start, end) = (segment[0], segment[1]);
            let line = calls
                .iter()
                .find(|(call, _)| call.contains(start))
                .map_or_else(|| lines.at(start), |(_, site)| site.as_ref());
            let shown = line
                .is_none_or(|line| line.file != declaration.file || line.line > declaration.line);
            if !shown {
                continue;
            }
            match visible.last_mut() {
                Some(last) if last.end == start => last.end = end,
                _ => visible.push(AddressRange { start, end }),
            }
        }
    }
    visible
}

/// Ranges merged into the fewest sorted ranges covering them.
pub(super) fn fused(
    mut ranges: Vec<AddressRange<ImageAddress>>,
) -> Vec<AddressRange<ImageAddress>> {
    ranges.sort_by_key(|range| range.start);
    let mut fused = Vec::<AddressRange<ImageAddress>>::with_capacity(ranges.len());
    for range in ranges {
        match fused.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => fused.push(range),
        }
    }
    fused
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LineNumber, SourceFileId};

    fn range(start: u64, end: u64) -> AddressRange<ImageAddress> {
        AddressRange {
            start: ImageAddress::new(start),
            end: ImageAddress::new(end),
        }
    }

    fn at(file: u32, line: u64) -> SourceLocation {
        SourceLocation {
            file: SourceFileId::new(file),
            line: LineNumber::new(line).expect("lines are one-based"),
            column: None,
        }
    }

    #[test]
    fn a_go_local_is_visible_only_past_its_declarations_line() {
        let row = |start, end, location| LineEntry {
            range: range(start, end),
            location,
            statement: true,
        };
        // Lines 10, 11 (the declaration), 12, a call inlined at line 9, a
        // row of another file, line 13, and code with no line.
        let lines = LineIndex::new(&[
            row(0x10, 0x20, at(0, 10)),
            row(0x20, 0x30, at(0, 11)),
            row(0x30, 0x40, at(0, 12)),
            row(0x40, 0x50, at(1, 70)),
            row(0x50, 0x60, at(0, 3)),
            row(0x60, 0x70, at(0, 13)),
        ]);
        let calls = [
            (range(0x40, 0x48), Some(at(0, 9))),
            (range(0x50, 0x60), Some(at(0, 14))),
        ];
        let visible = after_declaration(&[range(0x10, 0x80)], &at(0, 11), &lines, &calls);
        assert_eq!(
            visible,
            [range(0x30, 0x40), range(0x48, 0x80)],
            "line 12, the other file's row past the call, the call at line \
             14 though its own code is at line 3, line 13, and code with no line"
        );
    }
}
