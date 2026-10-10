//! Drawing values for the page: the inputs a view's `visualize` hands its
//! renderer, the bytes sent beside them, and the renderers themselves.

use serde_json::{Value, json};

use crate::support::{Scenario, ScratchDir};
use crate::web::{Client, Web};

fn fixture(name: &str) -> String {
    Scenario::fixture(name).display().to_string()
}

fn source(path: &str) -> String {
    format!("{}/tests/fixtures/{path}", env!("CARGO_MANIFEST_DIR"))
}

/// Stops at `location` and returns that stop's innermost frame.
async fn stop_at(tab: &mut Client, location: &str) -> Value {
    tab.state("the program loaded", |state| state["session"].is_string())
        .await;
    tab.ok("addBreakpoint", json!({"location": location})).await;
    continue_to_stop(tab).await
}

async fn continue_to_stop(tab: &mut Client) -> Value {
    let before = tab
        .latest_state()
        .map(|state| state["inferior"]["stop"].clone())
        .filter(|stop| !stop.is_null());
    let params = before
        .as_ref()
        .map_or_else(|| json!({}), |stop| json!({"stop": stop}));
    tab.ok("continue", params).await;
    let stopped = tab
        .state("the next stop", |state| {
            state["inferior"]["state"] == "stopped"
                && Some(&state["inferior"]["stop"]) != before.as_ref()
        })
        .await;
    let inferior = &stopped["inferior"];
    json!({"stop": inferior["stop"], "thread": inferior["thread"], "frame": 0})
}

fn with(frame: &Value, extra: &Value) -> Value {
    let mut params = frame.clone();
    for (key, value) in extra.as_object().expect("an object") {
        params[key] = value.clone();
    }
    params
}

/// The input named `name` of a drawing.
fn input<'a>(drawing: &'a Value, name: &str) -> &'a Value {
    &drawing["inputs"]
        .as_array()
        .unwrap_or_else(|| panic!("no inputs in {drawing}"))
        .iter()
        .find(|input| input["name"] == name)
        .unwrap_or_else(|| panic!("no input {name} in {drawing}"))["value"]
}

/// Little-endian 64-bit words.
fn words(bytes: &[u8]) -> Vec<u64> {
    bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|word| u64::from_le_bytes(*word))
        .collect()
}

fn names(inputs: &Value) -> Vec<&str> {
    inputs
        .as_array()
        .expect("inputs")
        .iter()
        .map(|input| input["name"].as_str().unwrap_or_default())
        .collect()
}

#[tokio::test]
async fn a_rust_programs_own_views_draw_its_board_with_typed_inputs() {
    let web = Web::start("chess", &[&fixture("chess")]);
    let mut tab = web.control("tab").await;
    // After the game's last move, white's castling.
    let frame = stop_at(&mut tab, "main.rs:147").await;

    let board = tab
        .ok("evaluate", with(&frame, &json!({"expression": "board"})))
        .await;
    assert_eq!(board["drawings"], json!(["chess-board", "bits"]), "{board}");

    let (drawing, bytes) = tab
        .draw(with(
            &frame,
            &json!({"path": "board", "renderer": "chess-board"}),
        ))
        .await
        .expect("draw the board");
    assert_eq!(drawing["offered"], true);
    assert_eq!(drawing["problem"], Value::Null);
    assert_eq!(drawing["renderer"]["name"], "chess-board");
    // The renderer came in the program's own views section.
    let origin = drawing["renderer"]["origin"].as_str().expect("an origin");
    assert!(origin.contains("chess"), "{origin}");
    assert!(bytes.is_empty());
    assert_eq!(names(&drawing["inputs"]), ["squares", "turn"]);

    // Options are sums whose payload is the one field, records of named
    // members, and enumerations with their enumerator's name.
    let squares = input(&drawing, "squares")["items"]
        .as_array()
        .expect("64 squares")
        .clone();
    assert_eq!(squares.len(), 64);
    let piece = |color: &str, kind: &str, number: i64| {
        json!({"t": "sum", "variant": "Some", "value": {"t": "record", "members": [
            ["color", {"t": "enum", "name": color, "value": {"t": "int", "i": i64::from(color == "Black")}}],
            ["kind", {"t": "enum", "name": kind, "value": {"t": "int", "i": number}}],
        ]}})
    };
    let empty = json!({"t": "sum", "variant": "None", "value": null});
    // e1g1 moved the king and the rook beside it.
    assert_eq!(squares[4], empty, "e1");
    assert_eq!(squares[5], piece("White", "Rook", 3), "f1");
    assert_eq!(squares[6], piece("White", "King", 5), "g1");
    assert_eq!(squares[7], empty, "h1");
    assert_eq!(squares[28], piece("White", "Pawn", 0), "e4");
    assert_eq!(squares[42], piece("Black", "Pawn", 0), "c6, where d7 took");
    assert_eq!(
        input(&drawing, "turn"),
        &json!({"t": "enum", "name": "Black", "value": {"t": "int", "i": 1}})
    );

    // An array of numbers is sent as raw bytes beside the answer, which
    // agree with the mailbox, and a string is handed over as written.
    let (drawing, bytes) = tab
        .draw(with(&frame, &json!({"path": "board", "renderer": "bits"})))
        .await
        .expect("draw the bitboards");
    assert_eq!(drawing["renderer"]["origin"], "built-in");
    assert_eq!(
        input(&drawing, "values"),
        &json!({"t": "numbers", "kind": "u64", "offset": 0, "count": 2})
    );
    assert_eq!(
        input(&drawing, "origin"),
        &json!({"t": "text", "s": "bottom-left"})
    );
    let colors = words(&bytes);
    for (square, contents) in squares.iter().enumerate() {
        let color = contents["value"]["members"][0][1]["name"].as_str();
        assert_eq!(
            colors[0] >> square & 1 == 1,
            color == Some("White"),
            "white at {square}"
        );
        assert_eq!(
            colors[1] >> square & 1 == 1,
            color == Some("Black"),
            "black at {square}"
        );
    }

    // Draw as…: any renderer draws a value its view offers no drawing of,
    // as its `values`.
    let (drawing, bytes) = tab
        .draw(with(
            &frame,
            &json!({"path": "board.pieces", "renderer": "bar-chart"}),
        ))
        .await
        .expect("draw the pieces");
    assert_eq!(drawing["offered"], false);
    assert_eq!(names(&drawing["inputs"]), ["values"]);
    assert_eq!(
        input(&drawing, "values"),
        &json!({"t": "numbers", "kind": "u64", "offset": 0, "count": 6})
    );
    let pieces = words(&bytes).into_iter().fold(0, |all, kind| all | kind);
    assert_eq!(pieces, colors[0] | colors[1]);

    // The page fetches a renderer's JavaScript by the digest it was told.
    let digest = drawing["renderer"]["digest"].clone();
    let renderer = tab.ok("renderer", json!({"digest": digest})).await;
    assert_eq!(renderer["name"], "bar-chart");
    assert!(
        renderer["source"]
            .as_str()
            .is_some_and(|source| source.contains("uscope.draw")),
        "{renderer}"
    );
    let (kind, _) = tab
        .request("renderer", json!({"digest": "0".repeat(32)}))
        .await
        .expect_err("no renderer has that digest");
    // As after a reload, which the page answers by asking again.
    assert_eq!(kind, "staleStop");
}

/// Conway's rules on the fixture's 64 by 48 torus.
fn life_step(cells: &[u8]) -> Vec<u8> {
    let (rows, columns) = (48_usize, 64_usize);
    let mut next = vec![0; cells.len()];
    for row in 0..rows {
        for column in 0..columns {
            let mut around = 0;
            for dr in [rows - 1, 0, 1] {
                for dc in [columns - 1, 0, 1] {
                    if (dr, dc) != (0, 0) {
                        around += cells[(row + dr) % rows * columns + (column + dc) % columns];
                    }
                }
            }
            let alive = cells[row * columns + column] == 1;
            next[row * columns + column] = u8::from(around == 3 || (around == 2 && alive));
        }
    }
    next
}

/// The fixture's first generation: a glider and a blinker.
fn life_seed() -> Vec<u8> {
    let mut cells = vec![0; 48 * 64];
    for (row, column) in [
        (1, 2),
        (2, 3),
        (3, 1),
        (3, 2),
        (3, 3),
        (20, 30),
        (20, 31),
        (20, 32),
    ] {
        cells[row * 64 + column] = 1;
    }
    cells
}

#[tokio::test]
async fn a_session_views_file_draws_memory_read_at_once_at_each_stop() {
    let views = source("c/life/life.views");
    let web = Web::start("life", &["--views", &views, &fixture("life")]);
    let mut tab = web.control("tab").await;
    let first = stop_at(&mut tab, "life.c:59").await;

    let life = tab
        .ok("evaluate", with(&first, &json!({"expression": "life"})))
        .await;
    assert_eq!(life["drawings"], json!(["life"]), "{life}");
    let renderers = tab.ok("renderers", json!(null)).await;
    let listed = renderers["renderers"].as_array().expect("renderers");
    let life_renderer = listed
        .iter()
        .find(|renderer| renderer["name"] == "life")
        .expect("the session's renderer");
    assert_eq!(life_renderer["origin"], source("c/life/life.js"));

    let draw = |frame: &Value| with(frame, &json!({"path": "life", "renderer": "life"}));
    let (drawing, bytes) = tab.draw(draw(&first)).await.expect("draw");
    assert_eq!(
        names(&drawing["inputs"]),
        ["cells", "columns", "generation"]
    );
    assert_eq!(
        input(&drawing, "cells"),
        &json!({"t": "bytes", "offset": 0, "length": 48 * 64})
    );
    assert_eq!(input(&drawing, "columns"), &json!({"t": "int", "i": 64}));
    assert_eq!(input(&drawing, "generation"), &json!({"t": "int", "i": 1}));
    let generation = life_step(&life_seed());
    assert_eq!(bytes, generation, "the first generation");

    // At the next stop the drawing is of the next generation, and the
    // last stop's can no longer be drawn.
    let second = continue_to_stop(&mut tab).await;
    let (drawing, bytes) = tab.draw(draw(&second)).await.expect("draw");
    assert_eq!(input(&drawing, "generation"), &json!({"t": "int", "i": 2}));
    assert_eq!(bytes, life_step(&generation), "the second generation");
    let (kind, _) = tab.draw(draw(&first)).await.expect_err("a stale stop");
    assert_eq!(kind, "staleStop");

    // Draw as…: rows of numbers nest, each read with the rest at once.
    let (drawing, bytes) = tab
        .draw(with(
            &second,
            &json!({"path": "life.cells", "renderer": "heatmap"}),
        ))
        .await
        .expect("draw the rows");
    let rows = input(&drawing, "values")["items"].as_array().expect("rows");
    assert_eq!(rows.len(), 48);
    assert_eq!(
        rows[47],
        json!({"t": "numbers", "kind": "u8", "offset": 47 * 64, "count": 64})
    );
    assert_eq!(bytes.len(), 48 * 64);

    let (kind, message) = tab
        .draw(with(
            &second,
            &json!({"path": "life", "renderer": "nowhere"}),
        ))
        .await
        .expect_err("no renderer is named nowhere");
    assert_eq!(kind, "invalid", "{message}");
    assert!(message.contains("nowhere"), "{message}");
}

#[tokio::test]
async fn a_drawing_names_the_input_it_could_not_read() {
    let scratch = ScratchDir::new("drawing-problems");
    let views = scratch.path().join("problems.views");
    std::fs::write(
        &views,
        r#"uscope-views 1

extend c life {
    visualize "bitmap" {
        pixels = bytes(0x10, 64)
        columns = 8
    }
    visualize "heatmap" {
        values = bytes(&cells[0][0], 17000000)
    }
    visualize "bits" { values = bytes(0, 8) }
    visualize "nowhere" { values = generation }
}
"#,
    )
    .expect("write the views");
    let views = views.display().to_string();
    let web = Web::start("problems", &["--views", &views, &fixture("life")]);
    let mut tab = web.control("tab").await;
    let frame = stop_at(&mut tab, "life.c:59").await;

    // A drawing whose renderer does not exist is not offered.
    let life = tab
        .ok("evaluate", with(&frame, &json!({"expression": "life"})))
        .await;
    assert_eq!(life["drawings"], json!(["bitmap", "heatmap", "bits"]));

    for (renderer, expected) in [
        ("bitmap", "0x10"),
        ("heatmap", "an input may read at most 16777216"),
        ("bits", "null pointer"),
    ] {
        let (drawing, bytes) = tab
            .draw(with(&frame, &json!({"path": "life", "renderer": renderer})))
            .await
            .expect("a drawing with a problem");
        assert_eq!(drawing["inputs"], Value::Null, "{drawing}");
        assert!(bytes.is_empty());
        let problem = drawing["problem"].as_str().expect("a problem");
        assert!(
            problem.contains(expected),
            "{renderer}: {problem} names {expected}"
        );
    }
}

#[tokio::test]
async fn reloading_views_reads_the_files_and_renderers_again() {
    let scratch = ScratchDir::new("reload-views");
    let views = scratch.path().join("life.views");
    let renderer = scratch.path().join("life.js");
    std::fs::copy(source("c/life/life.views"), &views).expect("copy the views");
    std::fs::copy(source("c/life/life.js"), &renderer).expect("copy the renderer");
    let path = views.display().to_string();
    let web = Web::start("reload", &["--views", &path, &fixture("life")]);
    let mut tab = web.control("tab").await;
    let frame = stop_at(&mut tab, "life.c:59").await;
    let digest = |renderers: &Value| {
        renderers["renderers"]
            .as_array()
            .expect("renderers")
            .iter()
            .find(|renderer| renderer["name"] == "life")
            .map(|renderer| renderer["digest"].clone())
    };
    let before = tab.ok("renderers", json!(null)).await;
    // Each name is listed once: the session's, then the built-ins.
    let listed = before["renderers"]
        .as_array()
        .expect("renderers")
        .iter()
        .map(|renderer| renderer["name"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(
        listed,
        [
            "life",
            "bar-chart",
            "bitmap",
            "bits",
            "box-plot",
            "donut-chart",
            "flame-graph",
            "heatmap",
            "histogram",
            "line-plot",
            "scatter-plot"
        ]
    );
    let old = digest(&before).expect("life");

    std::fs::write(
        &renderer,
        "uscope.draw(() => uscope.picture({width: 1, height: 1, shapes: []}));\n",
    )
    .expect("change the renderer");
    std::fs::write(
        &views,
        "uscope-views 1\nextend c life { visualize \"life\" { cells = bytes(&cells[0][0], 64) } }\n",
    )
    .expect("change the views");
    tab.ok("reloadViews", json!(null)).await;
    tab.expect("the reload's notice", |message| {
        message["type"] == "notice" && message["text"] == "reloaded the views"
    })
    .await;

    let after = tab.ok("renderers", json!(null)).await;
    let new = digest(&after).expect("life");
    assert_ne!(old, new);
    let (kind, _) = tab
        .request("renderer", json!({"digest": old}))
        .await
        .expect_err("the old renderer is gone");
    assert_eq!(kind, "staleStop");
    let (drawing, _) = tab
        .draw(with(&frame, &json!({"path": "life", "renderer": "life"})))
        .await
        .expect("draw");
    assert_eq!(names(&drawing["inputs"]), ["cells"]);
    assert_eq!(
        input(&drawing, "cells"),
        &json!({"t": "bytes", "offset": 0, "length": 64})
    );
}
