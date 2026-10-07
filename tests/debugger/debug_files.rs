//! Debug information in separate files, as distributions ship it: found by
//! `.gnu_debuglink`, by build-id under a debug directory, or downloaded
//! from a debuginfod server.

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use uscope::DebugFileOptions;

use super::libraries::pending_function;
use super::*;
use crate::support::ScratchDir;

fn split(name: &str) -> PathBuf {
    Scenario::fixture(&format!("split/{name}"))
}

/// The debug directory `basic-build-id`'s debug file is filed under.
fn debug_root() -> PathBuf {
    split("debug-root")
}

/// `basic-build-id`'s debug file and the build-id naming it.
fn build_id_debug_file() -> (PathBuf, String) {
    let ids = debug_root().join(".build-id");
    let directory = fs::read_dir(&ids)
        .expect("the build-id directory")
        .map(|entry| entry.expect("an entry").path())
        .find(|path| path.is_dir())
        .expect("a build-id directory");
    let file = fs::read_dir(&directory)
        .expect("the build-id files")
        .map(|entry| entry.expect("an entry").path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "debug")
        })
        .expect("a debug file");
    let prefix = directory.file_name().expect("a name").to_string_lossy();
    let rest = file.file_stem().expect("a name").to_string_lossy();
    (file.clone(), format!("{prefix}{rest}"))
}

/// Runs `basic`'s copy to `breakpoint_target` and checks its debug
/// information came from `debug_file`: the stop names the function and its
/// line, which only DWARF knows in a stripped program.
async fn stops_with_debug_information(mut scenario: Scenario, debug_file: &Path) {
    let image = Arc::clone(scenario.handle().module_image());
    assert_eq!(
        image.debug_file().map(|path| path.as_path()),
        Some(debug_file),
        "the debug file"
    );
    assert!(!image.functions().is_empty());
    let breakpoint = scenario
        .operation(
            "add breakpoint",
            scenario
                .handle()
                .add_breakpoint(BreakpointSpec::Function("breakpoint_target".to_owned())),
        )
        .await;
    let reason = scenario.run_to_stop().await;
    let StopReason::Breakpoint { hits, .. } = &reason else {
        panic!("stopped for {reason:?}");
    };
    assert_eq!(hits[0].breakpoint, breakpoint.id);
    let location = scenario
        .operation("location", scenario.handle().current_location())
        .await;
    assert!(location_line(&location).is_some(), "{location:?}");
    // The stripped file's own symbol table is gone; the debug file's names
    // the function.
    assert!(
        image
            .symbols()
            .iter()
            .any(|symbol| &*symbol.name == "breakpoint_target"),
        "the debug file's symbols"
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn a_debug_link_beside_the_program_describes_it() {
    let fixture = split("basic-debuglink");
    let scenario = Scenario::new("debuglink", &fixture);
    let debug_file = split(".debug/basic-debuglink.debug")
        .canonicalize()
        .expect("the debug file");
    stops_with_debug_information(scenario, &debug_file).await;
}

/// A debug link whose file differs from the one linked, as one left from
/// an older build, is refused by its checksum.
#[tokio::test]
async fn a_debug_link_to_a_file_from_another_build_is_refused() {
    let scratch = ScratchDir::new("stale-debuglink");
    let program = scratch.path().join("basic-debuglink");
    fs::copy(split("basic-debuglink"), &program).expect("copy the program");
    fs::create_dir(scratch.path().join(".debug")).expect("the .debug directory");
    // Another build's debug information under the linked name.
    fs::copy(
        build_id_debug_file().0,
        scratch.path().join(".debug/basic-debuglink.debug"),
    )
    .expect("copy a debug file");
    let scenario = Scenario::new("stale-debuglink", &program);
    let image = scenario.handle().module_image();
    assert_eq!(image.debug_file(), None);
    assert!(image.functions().is_empty());
    scenario.shutdown().await;
}

#[tokio::test]
async fn a_debug_directory_holds_the_debug_file_a_build_id_names() {
    let fixture = split("basic-build-id");
    // Without the directory, nothing describes the program.
    let scenario = Scenario::new("build-id-missing", &fixture);
    assert!(scenario.handle().module_image().functions().is_empty());
    scenario.shutdown().await;

    let options = DebugFileOptions {
        directories: vec![debug_root()],
        ..DebugFileOptions::default()
    };
    let scenario = Scenario::with_debug_files("build-id", &fixture, &options);
    stops_with_debug_information(scenario, &build_id_debug_file().0).await;
}

/// A shared library stripped of its debug information takes it from its
/// separate file as it loads.
#[tokio::test]
async fn a_library_loads_its_debug_information_from_its_debug_link() {
    let mut scenario = Scenario::new("library-debuglink", split("module-frames"));
    let breakpoint = pending_function(&scenario, "dso_apply").await;
    let reason = scenario.run_to_stop().await;
    let StopReason::Breakpoint { hits, .. } = &reason else {
        panic!("stopped for {reason:?}");
    };
    assert_eq!(hits[0].breakpoint, breakpoint.id);
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let frame = &trace.frames[0];
    assert_eq!(
        frame.function.as_ref().map(|function| &*function.name),
        Some("dso_apply")
    );
    assert!(frame.source.is_some(), "{frame:?}");
    let image = scenario
        .operation(
            "library image",
            scenario
                .handle()
                .loaded_module_image(frame.module.expect("a module")),
        )
        .await;
    let debug_file = split(".debug/libmodule-frames.so.debug")
        .canonicalize()
        .expect("the debug file");
    assert_eq!(
        image.debug_file().map(|path| path.as_path()),
        Some(debug_file.as_path())
    );
    scenario.shutdown().await;
}

/// A debuginfod server that answers every request for `id` with `body`,
/// which it counts, and anything else, such as the C library's debug file,
/// with 404.
struct Server {
    url: String,
    requests: Arc<AtomicUsize>,
}

impl Server {
    fn start(id: String, body: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a port");
        let url = format!("http://{}", listener.local_addr().expect("an address"));
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    continue;
                };
                let mut request = String::new();
                let mut reader = BufReader::new(&stream);
                if reader.read_line(&mut request).is_err() {
                    continue;
                }
                // The headers end at an empty line.
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|read| read > 2) {
                    line.clear();
                }
                let wanted = format!("GET /buildid/{id}/debuginfo ");
                let response = if request.starts_with(&wanted) {
                    counted.fetch_add(1, Ordering::SeqCst);
                    let mut response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    response.extend_from_slice(&body);
                    response
                } else {
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_vec()
                };
                let _ = stream.write_all(&response);
            }
        });
        Self { url, requests }
    }
}

/// A debug file no directory holds is downloaded from a debuginfod server
/// and kept in its cache, from which a later session reads it without
/// asking. A file the server sends for another build is refused.
#[tokio::test]
async fn debuginfod_downloads_debug_files_into_its_cache() {
    let fixture = split("basic-build-id");
    let (debug_file, id) = build_id_debug_file();
    let cache = ScratchDir::new("debuginfod-cache");
    let options = |url: &str| DebugFileOptions {
        debuginfod: true,
        debuginfod_urls: Some(vec![url.to_owned()]),
        debuginfod_cache: Some(cache.path().to_path_buf()),
        ..DebugFileOptions::default()
    };

    // Another build's debug file under this build's id.
    let wrong = Server::start(
        id.clone(),
        fs::read(split(".debug/basic-debuglink.debug")).expect("a debug file"),
    );
    let scenario = Scenario::with_debug_files("debuginfod-wrong", &fixture, &options(&wrong.url));
    assert!(scenario.handle().module_image().functions().is_empty());
    assert_eq!(wrong.requests.load(Ordering::SeqCst), 1);
    scenario.shutdown().await;

    let server = Server::start(id.clone(), fs::read(&debug_file).expect("the debug file"));
    let cached = cache.path().join(&id).join("debuginfo");
    let scenario = Scenario::with_debug_files("debuginfod", &fixture, &options(&server.url));
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        fs::read(&cached).expect("the cached file"),
        fs::read(&debug_file).expect("the debug file")
    );
    stops_with_debug_information(scenario, &cached).await;

    // The cache answers before any server is asked.
    let scenario = Scenario::with_debug_files("debuginfod-cached", &fixture, &options(&wrong.url));
    stops_with_debug_information(scenario, &cached).await;
    assert_eq!(wrong.requests.load(Ordering::SeqCst), 1);
}

/// A debug file that shares its debug information through a dwz
/// supplementary file, as distributions' do, is refused with its reason,
/// and the program is described as its own file describes it.
#[tokio::test]
async fn a_debug_file_needing_a_supplementary_file_is_refused_with_its_reason() {
    let options = DebugFileOptions {
        directories: vec![split("altlink-root")],
        ..DebugFileOptions::default()
    };
    let mut scenario = Scenario::with_debug_files("altlink", split("basic-build-id"), &options);
    let image = Arc::clone(scenario.handle().module_image());
    assert!(image.functions().is_empty());
    assert_eq!(image.debug_file(), None);
    let Some(uscope::DebugFile::Unusable { path, reason }) = image.separate_debug_file() else {
        panic!("{:?}", image.separate_debug_file());
    };
    assert!(path.starts_with(split("altlink-root")), "{path:?}");
    assert!(reason.contains("dwz supplementary file"), "{reason}");
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Exited(_)
    ));
    scenario.shutdown().await;
}
