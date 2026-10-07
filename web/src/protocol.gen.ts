// Generated from src/web/protocol.rs by `cargo test`; do not edit.

export const PROTOCOL_VERSION = 1;

export type Role = "control" | "view";

export type Envelope = { 
/**
 * Chosen by the tab; the answer carries it back.
 */
id: number, } & ({ "method": "setName", "params": SetName } | { "method": "share", "params": Share } | { "method": "completePath", "params": CompletePath } | { "method": "processes" } | { "method": "launch", "params": Launch } | { "method": "attach", "params": Attach } | { "method": "openCore", "params": OpenCore } | { "method": "end" } | { "method": "continue", "params": Continue } | { "method": "pause" } | { "method": "kill" } | { "method": "restart" });

export type Request = { "method": "setName", "params": SetName } | { "method": "share", "params": Share } | { "method": "completePath", "params": CompletePath } | { "method": "processes" } | { "method": "launch", "params": Launch } | { "method": "attach", "params": Attach } | { "method": "openCore", "params": OpenCore } | { "method": "end" } | { "method": "continue", "params": Continue } | { "method": "pause" } | { "method": "kill" } | { "method": "restart" };

export type SetName = { name: string, };

export type Share = { role: Role, 
/**
 * The page path the link opens, such as `/s/k7q2`.
 */
to: string, };

export type CompletePath = { 
/**
 * The path typed so far, relative to the server's directory or `~`.
 */
text: string, };

export type Launch = { program: string, arguments: Array<string>, 
/**
 * The program's working directory, instead of the server's.
 */
cwd: string | null, 
/**
 * Variables set in the program's environment, in order.
 */
environment: Array<[string, string]>, 
/**
 * Stop at the program's first instruction when it starts.
 */
stopAtEntry: boolean, 
/**
 * Start the program at once instead of waiting for a continue.
 */
run: boolean, 
/**
 * End a current session to make way. Without it, a current session
 * fails the request with `busy`.
 */
replace: boolean, };

export type Attach = { pid: number, replace: boolean, };

export type OpenCore = { core: string, 
/**
 * The executable that wrote the dump, when the recorded one is wrong.
 */
executable: string | null, replace: boolean, };

export type Continue = { 
/**
 * The stop being continued, which must still be current. Absent to
 * start a program that has not started.
 */
stop: number | null, };

export type ServerMessage = { "type": "hello" } & Hello | { "type": "state" } & State | { "type": "result", id: number, result: JsonValue, } | { "type": "error", id: number, error: ErrorBody, } | { "type": "output" } & Output | { "type": "presence" } & Presence | { "type": "notice" } & Notice;

export type Hello = { version: number, 
/**
 * This connection's number, unique while the server runs.
 */
connection: number, role: Role, 
/**
 * The name others see until the tab chooses one.
 */
name: string, 
/**
 * The server's working directory, which relative paths start from.
 */
cwd: string, };

export type State = { 
/**
 * Identifies what is being debugged; a new program, process, or core
 * dump gets a new one. Absent when nothing is.
 */
session: string | null, 
/**
 * What is being debugged.
 */
target: Target | null, 
/**
 * Slow work under way, such as loading debug information.
 */
busy: string | null, 
/**
 * The debugger revision this state reflects.
 */
revision: number, inferior: Inferior, threads: Array<Thread>, };

export type Target = { kind: TargetKind, 
/**
 * The executable's path.
 */
program: string, 
/**
 * A launched program's arguments.
 */
arguments: Array<string>, 
/**
 * An attached process, or the process a core dump recorded.
 */
pid: number | null, };

export type TargetKind = "launch" | "attach" | "core";

export type Inferior = { "state": "notStarted" } | { "state": "running", pid: number, } | { "state": "stopped", pid: number, 
/**
 * The stop's number, which requests about it carry.
 */
stop: number, 
/**
 * The thread whose event caused the stop.
 */
thread: number, reason: StopReason, } | { "state": "exited", description: string, } | { "state": "detached", pid: number, };

export type StopReason = { 
/**
 * A short machine-readable kind, such as `breakpoint` or `pause`.
 */
kind: string, 
/**
 * The reason as the CLI says it.
 */
description: string, };

export type Thread = { id: number, name: string | null, stopped: boolean, };

export type ErrorBody = { kind: ErrorKind, message: string, };

export type ErrorKind = "staleStop" | "notStopped" | "forbidden" | "busy" | "invalid" | "unsupported" | "failed";

export type Output = { stream: Stream, text: string, };

export type Stream = "stdout" | "stderr";

export type Presence = { people: Array<Person>, };

export type Person = { connection: number, name: string, role: Role, };

export type Notice = { 
/**
 * Who did it.
 */
connection: number, name: string, 
/**
 * What they did, such as `continued`.
 */
text: string, };

export type PathCompletions = { entries: Array<PathEntry>, };

export type PathEntry = { 
/**
 * The completed text, ending in `/` for a directory.
 */
text: string, kind: PathKind, };

export type PathKind = "directory" | "executable" | "file";

export type Processes = { processes: Array<Process>, 
/**
 * Yama's `ptrace_scope`, when the kernel has it: at 1 or more,
 * attaching to a process that is not a child may be refused.
 */
ptraceScope: number | null, };

export type Process = { pid: number, 
/**
 * The command line, or the process's name when it has none.
 */
command: string, };

export type ShareLink = { url: string, };
