//! A record of what the debugger did, kept by development builds only.
//!
//! Every client request, native control call, wait status, classification,
//! and published event is appended to a bounded in-memory ring as one line,
//! stamped with the time since the first record and the thread that made it.
//! Embedders decide where the ring goes: the test harness writes it to a file
//! when a test fails, and the `uscope` binary streams every line to a file as
//! it is recorded, so the recording survives even a killed process.
//!
//! Recordings live under [`directory`], `target/flight-recorder` of the
//! source tree that built the binary. Release builds compile none of this:
//! the module is absent and every `record!` expands to dead code.

use std::collections::VecDeque;
use std::fmt::{self, Write as _};
use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, Once, OnceLock, PoisonError};
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// How many lines the ring keeps before dropping the oldest.
const CAPACITY: usize = 16 * 1024;
/// The longest line kept; longer ones, such as large event payloads, are cut.
const MAX_LINE: usize = 1024;
/// How large a streamed recording may grow before streaming stops.
const MAX_STREAM_BYTES: u64 = 64 * 1024 * 1024;
/// How many recordings of past runs [`stream_run`] keeps.
const KEPT_RUNS: usize = 20;

struct Recorder {
    lines: VecDeque<String>,
    dropped: u64,
    stream: Option<Stream>,
}

struct Stream {
    file: File,
    written: u64,
}

static RECORDER: Mutex<Recorder> = Mutex::new(Recorder {
    lines: VecDeque::new(),
    dropped: 0,
    stream: None,
});
static START: OnceLock<Instant> = OnceLock::new();

fn recorder() -> MutexGuard<'static, Recorder> {
    // Pushing a line cannot leave the ring inconsistent.
    RECORDER.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Appends one line. Use the crate's `record!` macro, which compiles away in
/// release builds.
pub(crate) fn record(message: fmt::Arguments<'_>) {
    let line = format_line(message);
    recorder().push(line);
}

fn format_line(message: fmt::Arguments<'_>) -> String {
    let elapsed = START.get_or_init(Instant::now).elapsed();
    let current = thread::current();
    let thread = current.name().map_or("unnamed", |name| {
        name.strip_prefix("uscope-").unwrap_or(name)
    });
    let mut line = format!("{:>11.6} {thread:<10} ", elapsed.as_secs_f64());
    let _ = line.write_fmt(message);
    if line.len() > MAX_LINE {
        let mut end = MAX_LINE;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
        line.push_str(" …");
    }
    line.push('\n');
    line
}

impl Recorder {
    fn push(&mut self, line: String) {
        if let Some(stream) = self.stream.as_mut() {
            stream.write(&line);
        }
        if self.lines.len() == CAPACITY {
            self.lines.pop_front();
            self.dropped += 1;
        }
        self.lines.push_back(line);
    }

    fn contents(&self, path: &Path) -> String {
        let mut text = format!(
            "# uscope flight recording {}\n# process {}, written at unix time {}\n",
            path.display(),
            std::process::id(),
            unix_seconds(),
        );
        if self.dropped > 0 {
            let _ = writeln!(text, "# the ring dropped the {} oldest lines", self.dropped);
        }
        text.push_str("# each line: seconds since the first record, thread, what happened\n");
        for line in &self.lines {
            text.push_str(line);
        }
        text
    }
}

impl Stream {
    fn write(&mut self, line: &str) {
        if self.written >= MAX_STREAM_BYTES {
            return;
        }
        // One write per line, so a killed process loses nothing it recorded.
        let _ = self.file.write_all(line.as_bytes());
        self.written += line.len() as u64;
        if self.written >= MAX_STREAM_BYTES {
            let _ = self
                .file
                .write_all(b"# the recording reached its size limit and stops here\n");
        }
    }
}

/// The directory recordings are written under: `target/flight-recorder` of
/// the source tree this build came from, which git ignores.
#[must_use]
pub fn directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("target/flight-recorder")
}

/// Writes the lines the ring holds to `path`, replacing any file there.
pub fn dump(path: &Path) -> io::Result<()> {
    let text = recorder().contents(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, text)
}

/// Writes the lines the ring holds to `path`, then every line recorded from
/// now on as it is recorded.
pub fn stream_to(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = File::create(path)?;
    // Holding the ring while writing it keeps any line from falling between
    // the copy and the stream.
    let mut recorder = recorder();
    let text = recorder.contents(path);
    file.write_all(text.as_bytes())?;
    recorder.stream = Some(Stream {
        file,
        written: text.len() as u64,
    });
    drop(recorder);
    Ok(())
}

/// Streams this process's recording to a new file under `runs/`, points
/// `latest.log` at it, and removes all but the newest recordings of past
/// runs. Returns the new recording's path.
pub fn stream_run() -> io::Result<PathBuf> {
    let runs = directory().join("runs");
    fs::create_dir_all(&runs)?;
    prune(&runs, KEPT_RUNS - 1)?;
    let path = runs.join(format!("{}-{}.log", unix_seconds(), std::process::id()));
    stream_to(&path)?;

    // Replace the link atomically, since concurrent runs race for it.
    let latest = directory().join("latest.log");
    let staged = directory().join(format!(".latest-{}.log", std::process::id()));
    let _ = fs::remove_file(&staged);
    std::os::unix::fs::symlink(&path, &staged)?;
    fs::rename(&staged, &latest)?;
    Ok(path)
}

/// Removes the oldest recordings in `runs` until at most `keep` remain.
/// Names start with a timestamp, so name order is age order.
fn prune(runs: &Path, keep: usize) -> io::Result<()> {
    let mut recordings = fs::read_dir(runs)?
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|path| path.extension().is_some_and(|extension| extension == "log"))
        .collect::<Vec<_>>();
    recordings.sort();
    let excess = recordings.len().saturating_sub(keep);
    for path in &recordings[..excess] {
        // Another run may be pruning the same file.
        let _ = fs::remove_file(path);
    }
    Ok(())
}

/// Records every panic, on any thread, before the previous hook reports it,
/// so a recording shows where a crashed controller stopped. Installs the
/// hook once per process.
pub fn record_panics() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // A panic while recording holds the lock this would wait for.
            if let Ok(mut recorder) = RECORDER.try_lock() {
                recorder.push(format_line(format_args!("panic: {info}")));
            }
            previous(info);
        }));
    });
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}
