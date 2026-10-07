//! Requests that read the program: stacks at a stop and source files.

use std::collections::BTreeSet;
use std::path::Path;

use uscope::{DebuggerHandle, FrameKind, LineNumber, StackFrameId, StopContext, UnwindTermination};

use super::describe::{Images, frame_name, hex};
use super::protocol::{
    self, Backtrace, ErrorKind, Frame, FunctionMatch, FunctionQuery, Functions, SourceFiles,
    SourceLine, SourceText,
};
use super::session::Failure;

/// The context of a thread's innermost frame at a stop.
pub const fn innermost(stop: u64, thread: u64) -> StopContext {
    StopContext {
        stop: uscope::StopId::new(stop),
        thread: uscope::ThreadId::new(thread),
        frame: StackFrameId::INNERMOST,
    }
}

/// The context of a thread's frame numbered `frame`, counting from the
/// innermost, at a stop.
pub async fn context(
    handle: &DebuggerHandle,
    stop: u64,
    thread: u64,
    frame: u32,
) -> Result<StopContext, Failure> {
    let mut context = innermost(stop, thread);
    if frame == 0 {
        return Ok(context);
    }
    let trace = handle.at(context).backtrace().await?;
    context.frame = trace
        .frames
        .iter()
        .find(|candidate| candidate.id.get() == frame)
        .map(|candidate| candidate.id)
        .ok_or_else(|| {
            Failure::new(
                ErrorKind::Invalid,
                format!("thread {thread} has no frame {frame} at stop {stop}"),
            )
        })?;
    Ok(context)
}

pub async fn backtrace(
    handle: &DebuggerHandle,
    images: &Images,
    at: protocol::ThreadAt,
) -> Result<Backtrace, Failure> {
    let trace = handle.at(innermost(at.stop, at.thread)).backtrace().await?;
    let mut frames = Vec::with_capacity(trace.frames.len());
    for frame in trace.frames.iter() {
        let image = match frame.module {
            Some(module) => images.get(module).await,
            None => None,
        };
        let source = frame.source.as_ref().and_then(|location| {
            let file = image.as_ref()?.source_file(location.file)?;
            Some(SourceLine {
                path: file.path.display().to_string(),
                line: location.line.get(),
                column: location.column.map(uscope::ColumnNumber::get),
            })
        });
        frames.push(Frame {
            index: frame.id.get(),
            name: frame_name(frame, image.as_deref()),
            kind: match frame.kind {
                FrameKind::Physical => protocol::FrameKind::Physical,
                FrameKind::Inline => protocol::FrameKind::Inline,
                FrameKind::Signal => protocol::FrameKind::Signal,
            },
            address: hex(frame.instruction.get()),
            module: image
                .as_ref()
                .and_then(|image| image.path().file_name())
                .map(|name| name.to_string_lossy().into_owned()),
            source,
        });
    }
    Ok(Backtrace {
        frames,
        incomplete: (trace.termination != UnwindTermination::Complete)
            .then(|| trace.termination.to_string()),
    })
}

pub async fn sources(images: &Images) -> SourceFiles {
    let images = images.with_sources().await;
    let mut files = BTreeSet::new();
    for image in &images {
        for file in image.source_files() {
            files.insert(file.path.display().to_string());
        }
    }
    // The executable's image comes first; Go names its main function
    // main.main.
    let entry = images.first().and_then(|image| {
        let main = ["main", "main.main"]
            .into_iter()
            .find_map(|name| image.function_named(name).ok())?;
        let declared = main.declaration.as_ref()?;
        Some(SourceLine {
            path: image.source_file(declared.file)?.path.display().to_string(),
            line: declared.line.get(),
            column: None,
        })
    });
    SourceFiles {
        files: files.into_iter().collect(),
        entry,
    }
}

/// Functions whose names hold `query`, ASCII case aside: whole names first,
/// then those it begins, holds, or holds the letters of in order, and
/// shorter names before longer ones.
pub async fn functions(images: &Images, query: &FunctionQuery) -> Functions {
    let wanted = query.query.trim().to_ascii_lowercase();
    let limit = query.limit.map_or(50, |limit| limit.min(200)) as usize;
    if wanted.is_empty() {
        return Functions {
            functions: Vec::new(),
            more: false,
        };
    }
    let images = images.with_sources().await;
    // A short query matches most of a large program, so candidates are
    // ranked before any of them is described.
    let mut candidates = Vec::new();
    let mut lower = String::new();
    for (module, image) in images.iter().enumerate() {
        for (index, function) in image.functions().iter().enumerate() {
            lower.clear();
            lower.extend(function.name.chars().map(|char| char.to_ascii_lowercase()));
            if let Some(rank) = rank(&lower, &wanted) {
                candidates.push((rank, function.name.len(), &function.name, module, index));
            }
        }
    }
    // Only the best few are sorted. Repeated declarations can take some of
    // their places, so more than the limit is kept.
    let keep = limit.saturating_mul(8).max(1);
    let mut more = candidates.len() > keep;
    if more {
        candidates.select_nth_unstable(keep);
        candidates.truncate(keep);
    }
    candidates.sort_unstable();
    let mut found = Vec::new();
    for &(_, _, _, module, index) in &candidates {
        let image = &images[module];
        let function = &image.functions()[index];
        let declared = function.declaration.as_ref();
        let described = FunctionMatch {
            name: function.name.to_string(),
            path: declared
                .and_then(|declared| image.source_file(declared.file))
                .map(|file| file.path.display().to_string()),
            line: declared.map(|declared| declared.line.get()),
        };
        // Declarations repeat a function in each unit that names it.
        if found.contains(&described) {
            continue;
        }
        if found.len() == limit {
            more = true;
            break;
        }
        found.push(described);
    }
    Functions {
        functions: found,
        more,
    }
}

/// How well `name` matches `query`, both lowercase, best first, or none.
fn rank(name: &str, query: &str) -> Option<u8> {
    match name.find(query) {
        Some(0) if name.len() == query.len() => Some(0),
        Some(0) => Some(1),
        Some(_) => Some(2),
        None => {
            let mut letters = name.chars();
            query
                .chars()
                .all(|wanted| letters.any(|letter| letter == wanted))
                .then_some(3)
        }
    }
}

/// Reads a file the debug information names, with the lines any module
/// can break at.
pub async fn source(
    handle: &DebuggerHandle,
    images: &Images,
    path: &str,
) -> Result<SourceText, Failure> {
    let recorded = Path::new(path);
    let mut found = None;
    let mut breakable = BTreeSet::new();
    let every = LineNumber::new(1).expect("one")..=LineNumber::new(u64::MAX).expect("nonzero");
    for image in images.with_sources().await {
        let Some(file) = image
            .source_files()
            .iter()
            .find(|file| file.path.as_path() == recorded)
        else {
            continue;
        };
        breakable.extend(image.breakpoint_lines(file.id, every.clone()));
        found.get_or_insert_with(|| file.clone());
    }
    let file = found.ok_or_else(|| {
        Failure::new(
            ErrorKind::Invalid,
            format!("the debug information names no source file {path}"),
        )
    })?;
    let (read, text) = handle.read_source_file(&file).await?;
    Ok(SourceText {
        path: path.to_owned(),
        read: read.display().to_string(),
        text,
        breakable: breakable.into_iter().map(LineNumber::get).collect(),
    })
}
