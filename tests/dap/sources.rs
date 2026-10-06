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
    assert_eq!(labels(&mut dap, "info vi"), [("view".to_owned(), 6, 2)]);
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

/// What a client numbering from `base` sees of one stop: the breakpoint's
/// line, the frame's line and column, the disassembly's location of the
/// stopped instruction, and the lines breakpoints can use there.
fn positions(base: u64) -> Vec<u64> {
    let path = source("c/basic.c");
    let line = line_of(&path, "return uscope_value;") - 1 + base;
    let mut dap = Dap::start(format!("lines from {base}"));
    let mark = dap.mark();
    dap.request(
        "initialize",
        json!({"adapterID": "uscope", "linesStartAt1": base == 1, "columnsStartAt1": base == 1}),
    );
    dap.event(mark, "initialized", |_| true);
    let launch = dap.send("launch", json!({"program": fixture("basic")}));
    let set = dap.request(
        "setBreakpoints",
        json!({"source": {"path": path}, "breakpoints": [{"line": line}]}),
    );
    dap.request("configurationDone", Value::Null);
    dap.success(launch);
    let stop = dap.stopped(launch.mark);
    let frame =
        dap.request("stackTrace", json!({"threadId": stop.thread, "levels": 1}))["stackFrames"][0]
            .clone();
    let instructions = dap.request(
        "disassemble",
        json!({"memoryReference": frame["instructionPointerReference"], "instructionCount": 1}),
    )["instructions"][0]
        .clone();
    let lines = dap.request(
        "breakpointLocations",
        json!({"source": {"path": path}, "line": line}),
    )["breakpoints"][0]["line"]
        .clone();
    dap.finish();
    let number = |value: &Value| value.as_u64().unwrap_or_else(|| panic!("{value}"));
    vec![
        number(&set["breakpoints"][0]["line"]),
        number(&frame["line"]),
        number(&frame["column"]),
        number(&instructions["line"]),
        number(&instructions["column"]),
        number(&lines),
    ]
}

#[test]
fn clients_counting_lines_and_columns_from_zero_see_every_position_one_less() {
    let ones = positions(1);
    assert!(ones.iter().all(|position| *position > 0), "{ones:?}");
    let zeros = positions(0);
    assert_eq!(
        zeros,
        ones.iter().map(|position| position - 1).collect::<Vec<_>>()
    );
}

#[test]
fn completions_inside_expressions_offer_members_registers_and_globals() {
    let path = source("c/command-names.c");
    let mut dap = Dap::start("expression completions");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("command-names"),
        json!({}),
        &Configuration {
            sources: vec![(path.clone(), vec![line_of(&path, "volatile int sink")])],
            ..Configuration::default()
        },
    );
    let stop = dap.stopped(started.mark);
    let frame =
        dap.request("stackTrace", json!({"threadId": stop.thread}))["stackFrames"][0]["id"].clone();
    let complete = |dap: &mut Dap, text: &str| {
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
                    target["type"].as_str().expect("type").to_owned(),
                    target["start"].as_u64().expect("start"),
                    target["length"].as_u64().expect("length"),
                )
            })
            .collect::<Vec<_>>()
    };
    let labels = |targets: &[(String, String, u64, u64)]| {
        targets
            .iter()
            .map(|(label, ..)| label.clone())
            .collect::<Vec<_>>()
    };
    // Members, through a pointer or not, after the operator that selects
    // them, replacing only the member's part.
    for text in ["origin.", "where->", "where.", "1 + origin.", "(*where)."] {
        let targets = complete(&mut dap, text);
        assert_eq!(labels(&targets), ["x", "y"], "{text}");
        let start = u64::try_from(text.chars().count()).expect("small") + 1;
        assert!(
            targets
                .iter()
                .all(|(_, kind, at, length)| kind == "field" && *at == start && *length == 0),
            "{text}: {targets:?}"
        );
    }
    assert_eq!(complete(&mut dap, "where->z"), []);
    assert_eq!(labels(&complete(&mut dap, "origin.y + where->x")), ["x"]);
    // A comparison is no member access.
    assert_eq!(complete(&mut dap, "x >"), []);
    // Values without members offer none.
    assert_eq!(complete(&mut dap, "x."), []);
    assert_eq!(complete(&mut dap, "missing."), []);
    // Registers after `$`.
    let registers = labels(&complete(&mut dap, "$r"));
    for register in ["rax", "rip", "rsp"] {
        assert!(registers.contains(&register.to_owned()), "{registers:?}");
    }
    assert!(registers.iter().all(|register| register.starts_with('r')));
    // Names: the frame's variables and the program's globals.
    let names = complete(&mut dap, "x + cou");
    assert_eq!(names, [("counter".to_owned(), "variable".to_owned(), 5, 3)]);
    let names = labels(&complete(&mut dap, "print sh"));
    assert_eq!(names, ["shadowed"]);
    dap.finish();
}

#[test]
fn library_sources_come_and_go_with_their_library() {
    let library = source("c/shared/library.c");
    let line = line_of(&library, "return *dso_pointer");
    let mut dap = Dap::start("library sources");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("globals-shared"),
        json!({"stopOnEntry": true}),
        &Configuration {
            sources: vec![(library.clone(), vec![line])],
            functions: vec!["after_unload".to_owned()],
            ..Configuration::default()
        },
    );
    // The library is not loaded yet, so its line waits for it.
    assert_eq!(started.source_breakpoints[0][0]["reason"], "pending");
    let is_library = |body: &Value| {
        body["source"]["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("shared/library.c"))
    };
    // Asking for the loaded sources has the client told as they change.
    let entry = dap.stopped(started.mark);
    let sources = dap.request("loadedSources", Value::Null);
    let listed = sources["sources"].as_array().expect("sources");
    assert!(
        listed.iter().any(|source| source["name"] == "main.c"),
        "{sources}"
    );
    assert!(
        !listed.iter().any(|source| source["path"] == json!(library)),
        "{sources}"
    );
    let resumed = dap.send("continue", json!({"threadId": entry.thread}));
    dap.success(resumed);
    let started = crate::dap::Started {
        mark: resumed.mark,
        ..started
    };
    let lines = |dap: &mut Dap| {
        dap.request(
            "breakpointLocations",
            json!({"source": {"path": library}, "line": line}),
        )["breakpoints"]
            .clone()
    };
    let mut mark = started.mark;
    for round in 0..2 {
        let stop = dap.stopped(mark);
        assert_eq!(stop.reason, "breakpoint", "round {round}");
        let new = dap.event(mark, "loadedSource", |body| {
            body["reason"] == "new" && is_library(body)
        });
        assert_eq!(new["source"]["name"], "library.c");
        let module = dap.event(mark, "module", |body| {
            body["module"]["name"] == "libglobals.so"
        });
        assert!(
            module["module"]["addressRange"]
                .as_str()
                .is_some_and(|range| range.starts_with("0x") && range.contains('-')),
            "{module}"
        );
        assert_eq!(module["module"]["symbolFilePath"], module["module"]["path"]);
        let frame =
            dap.request("stackTrace", json!({"threadId": stop.thread, "levels": 1}))["stackFrames"]
                [0]
            .clone();
        assert!(is_library(&frame), "{frame}");
        assert_eq!(lines(&mut dap), json!([{"line": line}]));
        let sources = dap.request("loadedSources", Value::Null);
        assert!(
            sources["sources"]
                .as_array()
                .expect("sources")
                .iter()
                .any(|source| source["path"] == json!(library)),
            "{sources}"
        );
        let resumed = dap.send("continue", json!({"threadId": stop.thread}));
        dap.success(resumed);
        if round == 0 {
            let stop = dap.stopped(resumed.mark);
            assert_eq!(stop.reason, "function breakpoint");
            dap.event(resumed.mark, "loadedSource", |body| {
                body["reason"] == "removed" && is_library(body)
            });
            assert_eq!(lines(&mut dap), json!([]));
            let resumed = dap.send("continue", json!({"threadId": stop.thread}));
            dap.success(resumed);
            mark = resumed.mark;
        } else {
            assert_eq!(
                dap.event(resumed.mark, "exited", |_| true),
                json!({"exitCode": 0})
            );
        }
    }
    dap.finish();
}
