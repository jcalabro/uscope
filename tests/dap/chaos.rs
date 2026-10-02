//! A client that sends random requests in every state, valid or not.
//!
//! Each run is seeded and deterministic in what it sends; the seed is in
//! the scenario's name, which a failure prints. Whatever is sent, every
//! request must get exactly one well-formed response, the adapter must keep
//! working, and the session must end cleanly. `CHAOS_SEEDS=n` runs `n`
//! other seeds and `CHAOS_ROUNDS=n` sends more bursts, for longer runs.

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Profile, fixture, source};

/// A small deterministic generator, so no dependency decides the sequence.
struct Random(u64);

impl Random {
    const fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    const fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[usize::try_from(self.below(items.len() as u64)).expect("small index")]
    }

    /// A small number, or now and then an extreme one.
    fn number(&mut self) -> Value {
        match self.below(8) {
            0 => json!(-1),
            1 => json!(i64::from(i32::MAX)),
            2 => json!(0),
            3 => json!("12"),
            _ => json!(self.below(40) + 1),
        }
    }
}

fn request(random: &mut Random, thread: Option<i64>) -> (&'static str, Value) {
    let thread = thread.map_or_else(|| random.number(), Value::from);
    let path = source("c/hot-calls.c");
    match random.below(24) {
        0 => ("threads", Value::Null),
        1 => (
            "stackTrace",
            json!({"threadId": thread, "startFrame": random.number(), "levels": random.number()}),
        ),
        2 => ("scopes", json!({"frameId": random.number()})),
        3 => (
            "variables",
            json!({"variablesReference": random.number(), "start": random.number(), "count": random.number()}),
        ),
        4 => (
            "evaluate",
            json!({"expression": random.pick(&["hot_count", "hot_stop", "bt", "info signals", "x 0x0", "((", "threads"]), "context": random.pick(&["watch", "repl", "hover"]), "frameId": random.number()}),
        ),
        5 => ("continue", json!({"threadId": thread})),
        6 => (
            "next",
            json!({"threadId": thread, "granularity": random.pick(&["line", "instruction"])}),
        ),
        7 => ("stepIn", json!({"threadId": thread})),
        8 => ("stepOut", json!({"threadId": thread})),
        9 => ("pause", json!({"threadId": thread})),
        10 => (
            "setBreakpoints",
            json!({"source": {"path": path}, "breakpoints": [{"line": random.number()}]}),
        ),
        11 => (
            "setFunctionBreakpoints",
            json!({"breakpoints": [{"name": random.pick(&["hot_function", "caller", "nope", ""])}]}),
        ),
        12 => ("setFunctionBreakpoints", json!({"breakpoints": []})),
        13 => (
            "setExceptionBreakpoints",
            json!({"filters": [random.pick(&["fatal", "other", "routine", "bogus"])]}),
        ),
        14 => (
            "disassemble",
            json!({"memoryReference": random.pick(&["0x0", "0x401000", "junk"]), "instructionOffset": random.number(), "instructionCount": random.below(30)}),
        ),
        15 => (
            "readMemory",
            json!({"memoryReference": random.pick(&["0x0", "0x401000", "junk"]), "count": random.below(100)}),
        ),
        16 => ("modules", json!({})),
        17 => ("loadedSources", Value::Null),
        18 => ("completions", json!({"text": "inf", "column": 4})),
        19 => (
            "dataBreakpointInfo",
            json!({"name": "hot_count", "frameId": random.number()}),
        ),
        20 => (
            "setDataBreakpoints",
            json!({"breakpoints": [{"dataId": random.pick(&["data-1", "bogus"])}]}),
        ),
        21 => ("exceptionInfo", json!({"threadId": thread})),
        22 => ("configurationDone", Value::Null),
        _ => (
            *random.pick(&["frobnicate", "source", "restartFrame", "goto"]),
            json!({"x": random.number()}),
        ),
    }
}

#[test]
fn random_requests_in_every_state_are_answered_once_and_harm_nothing() {
    let seeds: Vec<u64> = std::env::var("CHAOS_SEEDS").map_or_else(
        |_| vec![0x05ee_d001, 0x05ee_d002, 0x05ee_d003, 0x05ee_d004],
        |count| {
            (1..=count.parse::<u64>().expect("count"))
                .map(|seed| seed * 0x9e37_79b9)
                .collect()
        },
    );
    for seed in seeds {
        let mut random = Random(seed);
        let mut dap = Dap::start(format!("chaos seed {seed:#x}"));
        dap.relax_ordering_checks();
        let started = dap.launch(
            Profile::VsCode,
            &fixture("hot-calls"),
            json!({}),
            &Configuration::default(),
        );
        dap.event(started.mark, "process", |_| true);
        let main_thread = dap.process_id().map(i64::from);
        let rounds =
            std::env::var("CHAOS_ROUNDS").map_or(60, |rounds| rounds.parse().expect("rounds"));
        for _ in 0..rounds {
            // Requests go out in bursts, without waiting, as editors send them.
            let burst = (0..=random.below(3))
                .map(|_| {
                    let thread = (random.below(2) == 0).then_some(main_thread).flatten();
                    let (command, arguments) = request(&mut random, thread);
                    dap.send(command, arguments)
                })
                .collect::<Vec<_>>();
            for sent in burst {
                let response = dap.response(sent);
                assert!(response["success"].is_boolean(), "{response}");
            }
        }
        // The adapter still works after all of that.
        let threads = dap.request("threads", Value::Null);
        assert!(!threads["threads"].as_array().expect("threads").is_empty());
        dap.finish();
    }
}
