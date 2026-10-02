//! Modules, source files, breakpoint lines, completions, and progress.

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Profile, fixture, line_of, source};

#[test]
fn modules_and_sources_describe_the_loaded_program() {
    let mut dap = Dap::start("modules");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("basic"),
        json!({"stopOnEntry": true}),
        &Configuration::default(),
    );
    // Loading reports progress to a client that shows it.
    dap.event(started.mark, "progressStart", |body| {
        body["title"]
            .as_str()
            .is_some_and(|title| title.starts_with("Loading "))
    });
    dap.event(started.mark, "progressEnd", |_| true);
    dap.stopped(started.mark);
    // The program itself is announced, though no load event reports it.
    let program = dap.event(started.mark, "module", |body| {
        body["module"]["name"] == "basic"
    });
    assert_eq!(program["reason"], "new");
    let modules = dap.request("modules", json!({}));
    let names = modules["modules"]
        .as_array()
        .expect("modules")
        .iter()
        .map(|module| {
            (
                module["name"].as_str().expect("name").to_owned(),
                module["symbolStatus"].clone(),
            )
        })
        .collect::<Vec<_>>();
    assert!(
        names.contains(&("basic".to_owned(), json!("debug information loaded"))),
        "{names:?}"
    );
    assert!(
        names.iter().any(|(name, _)| name.starts_with("ld-linux")),
        "{names:?}"
    );
    assert_eq!(modules["totalModules"], names.len());
    let page = dap.request("modules", json!({"startModule": 1, "moduleCount": 1}));
    assert_eq!(page["modules"].as_array().expect("page").len(), 1);
    assert_eq!(page["modules"][0], modules["modules"][1]);

    let sources = dap.request("loadedSources", Value::Null);
    assert!(
        sources["sources"]
            .as_array()
            .expect("sources")
            .iter()
            .any(|source| source["path"] == source_path("c/basic.c"))
    );
    dap.finish();
}

fn source_path(path: &str) -> String {
    source(path).display().to_string()
}

#[test]
fn breakpoint_locations_list_the_lines_with_code() {
    let path = source("c/line-sliding.c");
    let mut dap = Dap::start("breakpoint locations");
    let started = dap.launch(
        Profile::Neovim,
        &fixture("line-sliding"),
        json!({"stopOnEntry": true}),
        &Configuration::default(),
    );
    dap.stopped(started.mark);
    let first = line_of(&path, "int first_function(int value)");
    let last = line_of(&path, "return doubled;");
    let locations = dap.request(
        "breakpointLocations",
        json!({"source": {"path": path}, "line": first, "endLine": last}),
    );
    let lines = locations["breakpoints"]
        .as_array()
        .expect("locations")
        .iter()
        .map(|location| location["line"].as_u64().expect("line"))
        .collect::<Vec<_>>();
    let comment = line_of(&path, "// a comment inside the function");
    assert!(
        lines.contains(&line_of(&path, "sliding_sink = doubled;")),
        "{lines:?}"
    );
    assert!(lines.contains(&last));
    assert!(
        !lines.contains(&comment),
        "comments have no code: {lines:?}"
    );
    let none = dap.request(
        "breakpointLocations",
        json!({"source": {"path": "/elsewhere/x.c"}, "line": 3}),
    );
    assert_eq!(none["breakpoints"], json!([]));
    dap.finish();
}

#[test]
fn completions_offer_commands_subcommands_and_variables() {
    let mut dap = Dap::start("completions");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("variables-gcc-o0"),
        json!({}),
        &Configuration {
            functions: vec!["pointer_target".to_owned()],
            ..Configuration::default()
        },
    );
    let stop = dap.stopped(started.mark);
    let frame =
        dap.request("stackTrace", json!({"threadId": stop.thread}))["stackFrames"][0]["id"].clone();
    let labels = |dap: &mut Dap, text: &str| {
        let body = dap.request(
            "completions",
            json!({"text": text, "column": text.chars().count() + 1, "frameId": frame}),
        );
        body["targets"]
            .as_array()
            .expect("targets")
            .iter()
            .map(|target| {
                (
                    target["label"].as_str().expect("label").to_owned(),
                    target["start"].as_u64().expect("start"),
                    target["length"].as_u64().expect("length"),
                )
            })
            .collect::<Vec<_>>()
    };
    let commands = labels(&mut dap, "wat");
    assert!(
        commands.contains(&("watch".to_owned(), 1, 3)),
        "{commands:?}"
    );
    assert!(
        commands.contains(&("watchpoints".to_owned(), 1, 3)),
        "{commands:?}"
    );
    assert_eq!(labels(&mut dap, "info sig"), [("signals".to_owned(), 6, 3)]);
    let variables = labels(&mut dap, "print poin");
    assert!(
        variables
            .iter()
            .any(|(label, start, _)| label == "pointee" && *start == 7),
        "{variables:?}"
    );
    assert!(
        variables
            .iter()
            .any(|(label, ..)| label == "pointer_parameter"),
        "{variables:?}"
    );
    dap.finish();
}

#[test]
fn stack_frames_show_the_parameters_line_and_module_a_client_asks_for() {
    let mut dap = Dap::start("frame format");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("variables-gcc-o0"),
        json!({}),
        &Configuration {
            functions: vec!["parameter_target".to_owned()],
            ..Configuration::default()
        },
    );
    let stop = dap.stopped(started.mark);
    let trace = dap.request(
        "stackTrace",
        json!({"threadId": stop.thread, "levels": 1, "format": {"parameters": true, "parameterTypes": true, "line": true, "module": true}}),
    );
    let line = trace["stackFrames"][0]["line"].clone();
    assert_eq!(
        trace["stackFrames"][0]["name"],
        format!("parameter_target(int parameter = 4) Line {line} [variables-gcc-o0]")
    );
    let plain = dap.request("stackTrace", json!({"threadId": stop.thread, "levels": 1}));
    assert_eq!(plain["stackFrames"][0]["name"], "parameter_target");
    dap.finish();
}
