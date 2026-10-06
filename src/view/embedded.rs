//! The view files a module carries in its `.debug_uscope_views` section.
//!
//! The section holds records, each a kind byte, a format byte, a 32-bit
//! little-endian length, and that many bytes. Zero bytes between records
//! are padding, as compilers and linkers leave between the contributions
//! of separate objects. A record of view text is kind 1, format 1; kind 2
//! is reserved for kernels.

use std::sync::Arc;

use super::ViewSet;
use super::syntax::{Error, MAX_FILE_BYTES};

/// The section's name.
pub const SECTION: &str = ".debug_uscope_views";

/// The kind of a record of view text, and the format this uscope reads.
pub const VIEW_TEXT: u8 = 1;
pub const VIEW_TEXT_FORMAT: u8 = 1;

/// The kind of a record reserved for kernels.
const KERNEL: u8 = 2;

/// How many records one module may carry.
const MAX_RECORDS: usize = 256;

/// The views of a module named `module` whose section holds `bytes`, with
/// why any record was left out. Each record is a file named
/// `MODULE.views[N]`, counting records from 0.
#[must_use]
pub fn view_set(module: &str, bytes: &[u8]) -> ViewSet {
    let (files, mut errors) = files(module, bytes);
    let mut set = ViewSet::new(
        files
            .iter()
            .map(|(name, text)| (name.as_str(), text.as_str())),
    );
    errors.extend(set.errors().iter().cloned());
    set.set_errors(errors);
    set
}

/// The view files in a section's records, and why any was left out.
fn files(module: &str, bytes: &[u8]) -> (Vec<(String, String)>, Vec<Error>) {
    let mut files = Vec::new();
    let mut errors = Vec::new();
    let mut position = 0;
    let mut index = 0;
    let error = |index: usize, message: String| Error {
        source: Arc::from(format!("{module}.views[{index}]")),
        line: 0,
        column: 0,
        message,
    };
    while position < bytes.len() {
        if bytes[position] == 0 {
            position += 1;
            continue;
        }
        if index == MAX_RECORDS {
            errors.push(error(
                index,
                format!("a module may carry at most {MAX_RECORDS} records of views"),
            ));
            break;
        }
        let Some(header) = bytes.get(position..position + 6) else {
            errors.push(error(index, "the record's header is cut short".to_owned()));
            break;
        };
        let kind = header[0];
        let format = header[1];
        let length = u32::from_le_bytes([header[2], header[3], header[4], header[5]]);
        let start = position + 6;
        let Some(payload) = usize::try_from(length)
            .ok()
            .and_then(|length| bytes.get(start..start.checked_add(length)?))
        else {
            errors.push(error(
                index,
                format!("the record claims {length} bytes, past the section's end"),
            ));
            break;
        };
        position = start + payload.len();
        match (kind, format) {
            // A record is no larger than a view file may be, so a module
            // cannot make the debugger copy more.
            (VIEW_TEXT, VIEW_TEXT_FORMAT) if payload.len() > MAX_FILE_BYTES => errors.push(error(
                index,
                format!("the record's views are larger than a view file may be, {MAX_FILE_BYTES} bytes"),
            )),
            (VIEW_TEXT, VIEW_TEXT_FORMAT) => match std::str::from_utf8(payload) {
                Ok(text) => files.push((format!("{module}.views[{index}]"), text.to_owned())),
                Err(_) => errors.push(error(index, "the record's views are not UTF-8".to_owned())),
            },
            (VIEW_TEXT, format) => errors.push(error(
                index,
                format!("the record's views are in format {format}, and this uscope reads format {VIEW_TEXT_FORMAT}"),
            )),
            // Kernels come later; a module may carry them already.
            (KERNEL, _) => {}
            (kind, _) => errors.push(error(
                index,
                format!("the record is of kind {kind}, which this uscope does not read"),
            )),
        }
        index += 1;
    }
    (files, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(kind: u8, format: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![kind, format];
        bytes.extend(u32::try_from(payload.len()).expect("small").to_le_bytes());
        bytes.extend(payload);
        bytes
    }

    #[test]
    fn records_are_read_past_padding_and_bad_ones_are_named() {
        let view = b"uscope-views 1\nview c intvec {\n    show empty(\"v\")\n}\n";
        let mut bytes = vec![0, 0];
        bytes.extend(record(VIEW_TEXT, VIEW_TEXT_FORMAT, view));
        bytes.extend([0; 3]);
        bytes.extend(record(KERNEL, 1, b"\0asm"));
        bytes.extend(record(VIEW_TEXT, 2, view));
        bytes.extend(record(9, 1, b""));
        bytes.extend(record(VIEW_TEXT, VIEW_TEXT_FORMAT, view));
        bytes.extend(record(
            VIEW_TEXT,
            VIEW_TEXT_FORMAT,
            &vec![b' '; MAX_FILE_BYTES + 1],
        ));
        bytes.extend([VIEW_TEXT, VIEW_TEXT_FORMAT, 200, 0, 0, 0, b'u']);
        let set = view_set("app", &bytes);
        assert_eq!(set.views().len(), 2);
        assert_eq!(&*set.views()[0].source, "app.views[0]");
        assert_eq!(&*set.views()[1].source, "app.views[4]");
        let errors = set
            .errors()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert_eq!(
            errors,
            [
                "app.views[2]:0:0: the record's views are in format 2, and this uscope reads format 1",
                "app.views[3]:0:0: the record is of kind 9, which this uscope does not read",
                "app.views[5]:0:0: the record's views are larger than a view file may be, 262144 bytes",
                "app.views[6]:0:0: the record claims 200 bytes, past the section's end",
            ]
        );
    }
}
