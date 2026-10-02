//! Launch and attach configurations.
//!
//! Unknown keys, such as the `name`, `type`, and `__sessionId` that clients
//! add, are ignored. Invalid values are reported with the path of the key
//! that holds them.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

use serde::Deserialize;
use serde::de::{self, DeserializeOwned, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use uscope::{AssemblySyntax, CoreDumpOptions, ProcessId, SignalPolicy, SourcePathMap};

/// How the session obtains its target.
#[derive(Debug)]
pub enum Start {
    Launch(Launch),
    Attach {
        process: ProcessId,
        executable: Option<PathBuf>,
    },
    Core(CoreDumpOptions),
}

/// A program to launch.
#[derive(Debug, Clone)]
pub struct Launch {
    pub program: PathBuf,
    pub arguments: Vec<OsString>,
    pub environment: Vec<(OsString, Option<OsString>)>,
    pub working_directory: Option<PathBuf>,
}

/// A validated launch or attach configuration.
#[derive(Debug)]
pub struct Configuration {
    pub start: Start,
    pub stop_on_entry: bool,
    pub source_paths: SourcePathMap,
    pub syntax: AssemblySyntax,
    /// Signal handling applied over the exception filters: each signal's
    /// actions, as the console's `handle` command takes them.
    pub signals: Vec<(u64, Vec<String>)>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Arguments {
    program: Option<PathBuf>,
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<PathBuf>,
    #[serde(default)]
    env: BTreeMap<String, Option<String>>,
    #[serde(default)]
    stop_on_entry: bool,
    #[serde(default, deserialize_with = "source_map")]
    source_map: Vec<[String; 2]>,
    disassembly_syntax: Option<Syntax>,
    #[serde(default)]
    signals: BTreeMap<String, Actions>,
    console: Option<String>,
    pid: Option<Pid>,
    core_file: Option<PathBuf>,
    sysroot: Option<PathBuf>,
    #[serde(default)]
    module_paths: Vec<PathBuf>,
    #[serde(default)]
    allow_module_mismatch: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Syntax {
    Intel,
    Att,
}

/// A process id: VS Code's `${command:pickProcess}` produces a string.
#[derive(Deserialize)]
#[serde(untagged)]
enum Pid {
    Number(u64),
    Text(String),
}

/// One signal action or several.
#[derive(Deserialize)]
#[serde(untagged)]
enum Actions {
    One(String),
    Many(Vec<String>),
}

/// Parses a `launch` request's configuration.
pub fn launch(arguments: Value) -> Result<Configuration, String> {
    let parsed = parse::<Arguments>(arguments, "launch configuration")?;
    if parsed.pid.is_some() || parsed.core_file.is_some() {
        return Err(
            "invalid launch configuration: `pid` and `coreFile` belong to attach \
                    configurations"
                .to_owned(),
        );
    }
    if let Some(console) = parsed
        .console
        .as_deref()
        .filter(|console| *console != "internalConsole")
    {
        return Err(format!(
            "invalid launch configuration at console: \"{console}\" is not supported; \
             program output appears in the debug console"
        ));
    }
    let program = parsed
        .program
        .clone()
        .ok_or_else(|| "invalid launch configuration: missing field `program`".to_owned())?;
    let launch = Launch {
        program,
        arguments: parsed.args.iter().map(OsString::from).collect(),
        environment: parsed
            .env
            .iter()
            .map(|(name, value)| (name.into(), value.as_ref().map(OsString::from)))
            .collect(),
        working_directory: parsed.cwd.clone(),
    };
    common(parsed, Start::Launch(launch), "launch configuration")
}

/// Parses an `attach` request's configuration: a process or a core dump.
pub fn attach(arguments: Value) -> Result<Configuration, String> {
    let parsed = parse::<Arguments>(arguments, "attach configuration")?;
    let start = match (&parsed.pid, &parsed.core_file) {
        (Some(_), Some(_)) => {
            return Err(
                "invalid attach configuration: give either `pid` or `coreFile`, not both"
                    .to_owned(),
            );
        }
        (None, None) => {
            return Err("invalid attach configuration: give `pid` or `coreFile`".to_owned());
        }
        (Some(pid), None) => {
            let number = match pid {
                Pid::Number(number) => Some(*number),
                Pid::Text(text) => text.trim().parse().ok(),
            }
            .filter(|number| *number != 0)
            .ok_or_else(|| {
                "invalid attach configuration at pid: expected a positive process id".to_owned()
            })?;
            Start::Attach {
                process: ProcessId::new(number),
                executable: parsed.program.clone(),
            }
        }
        (None, Some(core)) => Start::Core(CoreDumpOptions {
            core: core.clone(),
            executable: parsed.program.clone(),
            sysroot: parsed.sysroot.clone(),
            module_paths: parsed.module_paths.clone(),
            allow_module_mismatch: parsed.allow_module_mismatch,
        }),
    };
    common(parsed, start, "attach configuration")
}

fn common(parsed: Arguments, start: Start, what: &str) -> Result<Configuration, String> {
    let mut source_paths = SourcePathMap::new();
    for [from, to] in parsed.source_map {
        source_paths
            .push(&from, &to)
            .map_err(|error| format!("invalid {what} at sourceMap: {error}"))?;
    }
    let mut signals = Vec::new();
    for (name, actions) in parsed.signals {
        let code = uscope::signal_named(&name)
            .ok_or_else(|| format!("invalid {what} at signals.{name}: unknown signal"))?;
        let actions = match actions {
            Actions::One(action) => vec![action],
            Actions::Many(actions) => actions,
        };
        let mut policy = SignalPolicy {
            stop: false,
            print: false,
            pass: false,
        };
        for action in &actions {
            crate::cli::commands::apply_signal_action(&mut policy, action)
                .map_err(|error| format!("invalid {what} at signals.{name}: {error}"))?;
        }
        signals.push((code, actions));
    }
    Ok(Configuration {
        start,
        stop_on_entry: parsed.stop_on_entry,
        source_paths,
        syntax: match parsed.disassembly_syntax {
            None | Some(Syntax::Intel) => AssemblySyntax::Intel,
            Some(Syntax::Att) => AssemblySyntax::Att,
        },
        signals,
    })
}

/// Deserializes request arguments, naming the path of an invalid value.
pub fn parse<T: DeserializeOwned>(arguments: Value, what: &str) -> Result<T, String> {
    serde_path_to_error::deserialize(arguments).map_err(|error| {
        let path = error.path().to_string();
        if path == "." {
            format!("invalid {what}: {}", error.inner())
        } else {
            format!("invalid {what} at {path}: {}", error.inner())
        }
    })
}

/// Reads source path rules as `[["from", "to"], …]` or `{"from": "to", …}`,
/// keeping their order: earlier rules are tried first.
fn source_map<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<[String; 2]>, D::Error> {
    struct Rules;

    impl<'de> Visitor<'de> for Rules {
        type Value = Vec<[String; 2]>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a list of [from, to] pairs or an object mapping from to to")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
            let mut rules = Vec::new();
            while let Some(rule) = sequence.next_element::<[String; 2]>()? {
                rules.push(rule);
            }
            Ok(rules)
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut rules = Vec::new();
            while let Some(from) = map.next_key::<String>()? {
                let to = map.next_value::<String>()?;
                rules.push([from, to]);
            }
            Ok(rules)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }
    }

    deserializer.deserialize_any(Rules)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn launch_configurations_read_every_field_and_keep_rule_order() {
        let configuration = launch(json!({
            "type": "uscope", "request": "launch", "name": "x", "__sessionId": "y",
            "program": "/bin/true", "args": ["a", "b c"], "cwd": "/tmp",
            "env": {"KEEP": "1", "DROP": null},
            "stopOnEntry": true,
            "sourceMap": {"/z": "/one", "/a": "/two"},
            "disassemblySyntax": "att",
            "signals": {"SIGUSR1": "nostop", "alrm": ["stop", "nopass"]},
            "console": "internalConsole",
        }))
        .expect("valid configuration");
        let Start::Launch(launch) = &configuration.start else {
            panic!("not a launch");
        };
        assert_eq!(launch.program, PathBuf::from("/bin/true"));
        assert_eq!(launch.arguments, ["a", "b c"]);
        assert_eq!(
            launch.environment,
            [("DROP".into(), None), ("KEEP".into(), Some("1".into()))]
        );
        assert!(configuration.stop_on_entry);
        assert_eq!(configuration.syntax, AssemblySyntax::Att);
        // Object rules keep their written order, not their sorted order.
        assert_eq!(
            configuration
                .source_paths
                .candidates(std::path::Path::new("/z/f.c")),
            [PathBuf::from("/one/f.c"), PathBuf::from("/z/f.c")]
        );
        assert_eq!(configuration.signals.len(), 2);
    }

    #[test]
    fn invalid_configurations_name_the_offending_key() {
        for (arguments, expected) in [
            (
                json!({}),
                "invalid launch configuration: missing field `program`",
            ),
            (
                json!({"program": "p", "env": {"PATH": 1}}),
                "invalid launch configuration at env.PATH: invalid type: integer `1`, expected a string",
            ),
            (
                json!({"program": "p", "args": "a b"}),
                "invalid launch configuration at args: invalid type: string \"a b\", expected a sequence",
            ),
            (
                json!({"program": "p", "signals": {"SIGNOPE": "stop"}}),
                "invalid launch configuration at signals.SIGNOPE: unknown signal",
            ),
            (
                json!({"program": "p", "signals": {"SIGUSR1": "halt"}}),
                "invalid launch configuration at signals.SIGUSR1: unknown signal action 'halt'; use stop, nostop, print, noprint, pass, or nopass",
            ),
            (
                json!({"program": "p", "console": "integratedTerminal"}),
                "invalid launch configuration at console: \"integratedTerminal\" is not supported; program output appears in the debug console",
            ),
            (
                json!({"program": "p", "sourceMap": [["/a"]]}),
                "invalid launch configuration at sourceMap[0]: invalid length 1, expected an array of length 2",
            ),
        ] {
            assert_eq!(launch(arguments).map(|_| ()), Err(expected.to_owned()));
        }
        for (arguments, expected) in [
            (
                json!({}),
                "invalid attach configuration: give `pid` or `coreFile`",
            ),
            (
                json!({"pid": 1, "coreFile": "c"}),
                "invalid attach configuration: give either `pid` or `coreFile`, not both",
            ),
            (
                json!({"pid": "abc"}),
                "invalid attach configuration at pid: expected a positive process id",
            ),
        ] {
            assert_eq!(attach(arguments).map(|_| ()), Err(expected.to_owned()));
        }
        let configuration = attach(json!({"pid": "42"})).expect("string pid");
        assert!(matches!(
            configuration.start,
            Start::Attach { process, executable: None } if process == ProcessId::new(42)
        ));
    }
}
