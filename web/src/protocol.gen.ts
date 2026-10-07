// Generated from src/web/protocol.rs by `cargo test`; do not edit.

export const PROTOCOL_VERSION = 1;

export type Role = "control" | "view";

export type Envelope = { 
/**
 * Chosen by the tab; the answer carries it back.
 */
id: number, } & ({ "method": "setName", "params": SetName } | { "method": "share", "params": Share } | { "method": "completePath", "params": CompletePath } | { "method": "processes" } | { "method": "launch", "params": Launch } | { "method": "attach", "params": Attach } | { "method": "openCore", "params": OpenCore } | { "method": "end" } | { "method": "continue", "params": Continue } | { "method": "pause" } | { "method": "kill" } | { "method": "restart" } | { "method": "step", "params": Step } | { "method": "setFocus", "params": SetFocus } | { "method": "backtrace", "params": ThreadAt } | { "method": "sources" } | { "method": "source", "params": SourcePath } | { "method": "addBreakpoint", "params": AddBreakpoint } | { "method": "editBreakpoint", "params": EditBreakpoint } | { "method": "removeBreakpoint", "params": BreakpointRef } | { "method": "input", "params": Input });

export type Request = { "method": "setName", "params": SetName } | { "method": "share", "params": Share } | { "method": "completePath", "params": CompletePath } | { "method": "processes" } | { "method": "launch", "params": Launch } | { "method": "attach", "params": Attach } | { "method": "openCore", "params": OpenCore } | { "method": "end" } | { "method": "continue", "params": Continue } | { "method": "pause" } | { "method": "kill" } | { "method": "restart" } | { "method": "step", "params": Step } | { "method": "setFocus", "params": SetFocus } | { "method": "backtrace", "params": ThreadAt } | { "method": "sources" } | { "method": "source", "params": SourcePath } | { "method": "addBreakpoint", "params": AddBreakpoint } | { "method": "editBreakpoint", "params": EditBreakpoint } | { "method": "removeBreakpoint", "params": BreakpointRef } | { "method": "input", "params": Input };

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

export type Step = { 
/**
 * The stop being stepped from, which must still be current.
 */
stop: number, thread: number, 
/**
 * The frame a step out leaves; every other step starts from the
 * innermost frame.
 */
frame?: number, kind: StepKind, };

export type StepKind = "over" | "into" | "out" | "instruction" | "overInstruction";

export type SetFocus = { 
/**
 * Absent when the tab looks at nothing in particular.
 */
focus: Focus | null, };

export type Focus = { 
/**
 * The page path and query, such as `/s/k7q2/stop/12/t/41872/f/1`.
 */
url: string, 
/**
 * Such as `frame 1, serve_conn`.
 */
label: string, };

export type ThreadAt = { stop: number, thread: number, };

export type SourcePath = { 
/**
 * A path as the debug information records it.
 */
path: string, };

export type AddBreakpoint = { 
/**
 * A function, `FILE:LINE`, `FILE:FUNCTION`, or `0xADDRESS`.
 */
location: string, condition?: string | null, 
/**
 * Which hits stop, such as `>=5` or `%10`.
 */
hitCondition?: string | null, 
/**
 * A message to log instead of stopping, with expressions in braces.
 */
logMessage?: string | null, };

export type EditBreakpoint = { id: number, 
/**
 * Absent to remove.
 */
condition?: string | null, hitCondition?: string | null, logMessage?: string | null, };

export type BreakpointRef = { id: number, };

export type Input = { text: string, 
/**
 * Close the input after the text, so the program reads its end.
 */
eof: boolean, };

export type ServerMessage = { "type": "hello" } & Hello | { "type": "state" } & State | { "type": "result", id: number, result: unknown, } | { "type": "error", id: number, error: ErrorBody, } | { "type": "output" } & Output | { "type": "presence" } & Presence | { "type": "notice" } & Notice;

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
revision: number, inferior: Inferior, threads: Array<Thread>, breakpoints: Array<Breakpoint>, 
/**
 * The latest stops of this session, oldest first.
 */
stops: Array<StopEntry>, };

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
thread: number, reason: StopReason, 
/**
 * Where that thread stopped.
 */
place: Place | null, } | { "state": "exited", description: string, } | { "state": "detached", pid: number, };

export type StopReason = { 
/**
 * A short machine-readable kind, such as `breakpoint` or `pause`.
 */
kind: string, 
/**
 * The reason as the CLI says it.
 */
description: string, };

export type Place = { 
/**
 * The instruction's address, in hexadecimal.
 */
address: string, function: string | null, 
/**
 * The source file, as the debug information records it.
 */
path: string | null, line: number | null, };

export type StopEntry = { stop: number, thread: number, reason: StopReason, place: Place | null, 
/**
 * Who ran the program to it, when someone did.
 */
by: string | null, 
/**
 * What they did, such as `stepped over`.
 */
action: string | null, };

export type Breakpoint = { id: number, 
/**
 * What it was set at, as it was asked for.
 */
location: string, condition: string | null, hitCondition: string | null, logMessage: string | null, 
/**
 * Hits in the current process, including those that did not stop.
 */
hits: number, 
/**
 * Where it resolved; none while no loaded code has it.
 */
places: Array<Place>, };

export type Thread = { id: number, name: string | null, stopped: boolean, };

export type ErrorBody = { kind: ErrorKind, message: string, };

export type ErrorKind = "staleStop" | "notStopped" | "forbidden" | "busy" | "invalid" | "unsupported" | "failed";

export type Output = { stream: Stream, text: string, };

export type Stream = "stdout" | "stderr" | "log";

export type Presence = { people: Array<Person>, };

export type Person = { connection: number, name: string, role: Role, focus: Focus | null, };

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

export type Backtrace = { frames: Array<Frame>, 
/**
 * Why the stack ends early, when the unwinder could not finish it.
 */
incomplete: string | null, };

export type Frame = { 
/**
 * Its number, counting from the innermost frame.
 */
index: number, 
/**
 * The function, symbol, or address.
 */
name: string, kind: FrameKind, 
/**
 * The instruction or return address, in hexadecimal.
 */
address: string, 
/**
 * The file name of the module the code is in.
 */
module: string | null, source: SourceLine | null, };

export type FrameKind = "physical" | "inline" | "signal";

export type SourceLine = { path: string, line: number, column: number | null, };

export type SourceFiles = { 
/**
 * Paths as the debug information records them, sorted.
 */
files: Array<string>, 
/**
 * Where the program's main function is declared, when it has one.
 */
entry: SourceLine | null, };

export type SourceText = { path: string, 
/**
 * The file read, which a source map may have moved.
 */
read: string, text: string, 
/**
 * The lines a breakpoint can stop at, in order.
 */
breakable: Array<number>, };

export type BreakpointAdded = { id: number, };
