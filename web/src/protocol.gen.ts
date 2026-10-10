// Generated from src/web/protocol.rs by `cargo test`; do not edit.

export const PROTOCOL_VERSION = 1;

export type Role = "control" | "view";

export type Envelope = { 
/**
 * Chosen by the tab; the answer carries it back.
 */
id: number, } & ({ "method": "setName", "params": SetName } | { "method": "share", "params": Share } | { "method": "completePath", "params": CompletePath } | { "method": "processes" } | { "method": "launch", "params": Launch } | { "method": "attach", "params": Attach } | { "method": "openCore", "params": OpenCore } | { "method": "end" } | { "method": "continue", "params": Continue } | { "method": "pause" } | { "method": "kill" } | { "method": "restart" } | { "method": "step", "params": Step } | { "method": "stepTargets", "params": ThreadAt } | { "method": "jump", "params": Jump } | { "method": "setFocus", "params": SetFocus } | { "method": "backtrace", "params": ThreadAt } | { "method": "sources" } | { "method": "source", "params": SourcePath } | { "method": "addBreakpoint", "params": AddBreakpoint } | { "method": "editBreakpoint", "params": EditBreakpoint } | { "method": "removeBreakpoint", "params": BreakpointRef } | { "method": "input", "params": Input } | { "method": "scopes", "params": FrameAt } | { "method": "children", "params": ChildrenOf } | { "method": "evaluate", "params": Evaluate } | { "method": "setValue", "params": SetValue } | { "method": "complete", "params": Complete } | { "method": "console", "params": ConsoleLine } | { "method": "disassemble", "params": Disassemble } | { "method": "readMemory", "params": ReadMemory } | { "method": "writeMemory", "params": WriteMemory } | { "method": "registers", "params": FrameAt } | { "method": "addWatchpoint", "params": AddWatchpoint } | { "method": "editWatchpoint", "params": EditWatchpoint } | { "method": "removeWatchpoint", "params": WatchpointRef } | { "method": "signals" } | { "method": "setSignal", "params": SignalPolicy } | { "method": "modules" } | { "method": "functions", "params": FunctionQuery } | { "method": "tasks", "params": StopAt } | { "method": "draw", "params": Draw } | { "method": "renderer", "params": RendererRef } | { "method": "renderers" } | { "method": "reloadViews" });

export type Request = { "method": "setName", "params": SetName } | { "method": "share", "params": Share } | { "method": "completePath", "params": CompletePath } | { "method": "processes" } | { "method": "launch", "params": Launch } | { "method": "attach", "params": Attach } | { "method": "openCore", "params": OpenCore } | { "method": "end" } | { "method": "continue", "params": Continue } | { "method": "pause" } | { "method": "kill" } | { "method": "restart" } | { "method": "step", "params": Step } | { "method": "stepTargets", "params": ThreadAt } | { "method": "jump", "params": Jump } | { "method": "setFocus", "params": SetFocus } | { "method": "backtrace", "params": ThreadAt } | { "method": "sources" } | { "method": "source", "params": SourcePath } | { "method": "addBreakpoint", "params": AddBreakpoint } | { "method": "editBreakpoint", "params": EditBreakpoint } | { "method": "removeBreakpoint", "params": BreakpointRef } | { "method": "input", "params": Input } | { "method": "scopes", "params": FrameAt } | { "method": "children", "params": ChildrenOf } | { "method": "evaluate", "params": Evaluate } | { "method": "setValue", "params": SetValue } | { "method": "complete", "params": Complete } | { "method": "console", "params": ConsoleLine } | { "method": "disassemble", "params": Disassemble } | { "method": "readMemory", "params": ReadMemory } | { "method": "writeMemory", "params": WriteMemory } | { "method": "registers", "params": FrameAt } | { "method": "addWatchpoint", "params": AddWatchpoint } | { "method": "editWatchpoint", "params": EditWatchpoint } | { "method": "removeWatchpoint", "params": WatchpointRef } | { "method": "signals" } | { "method": "setSignal", "params": SignalPolicy } | { "method": "modules" } | { "method": "functions", "params": FunctionQuery } | { "method": "tasks", "params": StopAt } | { "method": "draw", "params": Draw } | { "method": "renderer", "params": RendererRef } | { "method": "renderers" } | { "method": "reloadViews" };

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
 * The task stepped, which runs on the thread.
 */
task?: TaskKey | null, 
/**
 * The frame a step out leaves; every other step starts from the
 * innermost frame.
 */
frame?: number, kind: StepKind, 
/**
 * For a step into, the one call of the line to go into, in
 * hexadecimal, as `stepTargets` lists it; the line's other calls run
 * to their returns.
 */
call?: string | null, };

export type Jump = { 
/**
 * The stop the thread is moved at, which must still be current.
 */
stop: number, thread: number, task?: TaskKey | null, 
/**
 * `FILE:LINE`, or another location a breakpoint takes.
 */
location: string, };

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

export type ThreadAt = { stop: number, thread: number, task?: TaskKey | null, };

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
stops: Array<StopEntry>, 
/**
 * Counts changes made to the program's values, which make values read
 * earlier at the same stop out of date.
 */
writes: number, watchpoints: Array<Watchpoint>, 
/**
 * Counts changes to settings that publish nothing else, such as
 * signal policies.
 */
settings: number, };

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
 * The instruction or return address, in hexadecimal; for a suspended
 * task's frame, where it resumes, if that is known.
 */
address: string | null, 
/**
 * The file name of the module the code is in.
 */
module: string | null, source: SourceLine | null, 
/**
 * For a frame that drives a future whose awaits the stack does not
 * show in full, why.
 */
unfollowed: string | null, };

export type FrameKind = "physical" | "inline" | "signal" | "tailCall" | "async" | "awaited";

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

export type FunctionQuery = { query: string, 
/**
 * How many to return; 50 when absent, and never more than 200.
 */
limit?: number | null, };

export type Functions = { functions: Array<FunctionMatch>, 
/**
 * Whether more functions match than were returned.
 */
more: boolean, };

export type FunctionMatch = { name: string, 
/**
 * Where it is declared, when the debug information says.
 */
path: string | null, line: number | null, };

export type SourceText = { path: string, 
/**
 * The file read, which a source map may have moved.
 */
read: string, text: string, 
/**
 * The lines a breakpoint can stop at, in order.
 */
breakable: Array<number>, };

export type Added = { id: number, };

export type FrameAt = { stop: number, thread: number, task?: TaskKey | null, frame: number, };

export type ChildrenOf = { 
/**
 * A row's `children.handle`, which belongs to this connection and the
 * row's stop.
 */
handle: number, start: number, count: number, };

export type Evaluate = { expression: string, stop: number, thread: number, task?: TaskKey | null, frame: number, };

export type SetValue = { 
/**
 * The expression that names what changes, a row's `path`.
 */
path: string, 
/**
 * An expression for the new value.
 */
value: string, stop: number, thread: number, task?: TaskKey | null, frame: number, };

export type Complete = { 
/**
 * The line up to the cursor.
 */
text: string, 
/**
 * The frame whose names complete; absent when nothing is stopped.
 */
stop?: number | null, thread?: number | null, task?: TaskKey | null, frame?: number | null, };

export type ConsoleLine = { line: string, 
/**
 * The frame the line runs in; absent when nothing is stopped.
 */
stop?: number | null, thread?: number | null, task?: TaskKey | null, frame?: number | null, };

export type Scopes = { scopes: Array<Scope>, };

export type Scope = { key: ScopeKey, name: string, rows: Array<Row>, 
/**
 * Why the scope's rows could not be read, when they could not.
 */
problem: string | null, };

export type ScopeKey = "args" | "locals" | "statics";

export type Row = { name: string, 
/**
 * The value's summary, or why there is none.
 */
text: string, type: string | null, 
/**
 * An expression that evaluates the value again.
 */
path: string | null, children: Children | null, 
/**
 * Whether `setValue` can change it.
 */
editable: boolean, 
/**
 * The address of its natural memory, as hexadecimal.
 */
memory: string | null, 
/**
 * How many bytes the value occupies there, when it is stored there.
 */
memoryBytes: number | null, 
/**
 * A row that only says where reading stopped short.
 */
truncated: boolean, 
/**
 * The renderers of the drawings the value's view offers, each once.
 */
drawings: Array<string>, };

export type Children = { handle: number, 
/**
 * How many children are elements, when it is known.
 */
indexed: number | null, 
/**
 * How many are named, when it is known.
 */
named: number | null, };

export type Rows = { rows: Array<Row>, };

export type Completions = { 
/**
 * Where the completed part starts, in characters.
 */
start: number, items: Array<Completion>, };

export type Completion = { label: string, 
/**
 * `keyword`, `value`, `variable`, or `field`.
 */
kind: string, };

export type ConsoleResult = { output: string | null, row: Row | null, };

export type Disassemble = { 
/**
 * The address to show, as hexadecimal; the frame's own code when
 * absent.
 */
address?: string | null, 
/**
 * Intel unless AT&T is asked for.
 */
syntax?: Syntax | null, stop: number, thread: number, task?: TaskKey | null, frame: number, };

export type ReadMemory = { 
/**
 * The stop the bytes are read at, which must be current.
 */
stop: number, 
/**
 * As hexadecimal, such as `0x7ffff7a3e010`.
 */
address: string, count: number, };

export type WriteMemory = { stop: number, address: string, 
/**
 * The bytes, as pairs of hexadecimal digits.
 */
bytes: string, };

export type AddWatchpoint = { 
/**
 * An expression in the frame, or `0xADDRESS:BYTES`.
 */
target: string, access: WatchAccess, 
/**
 * The frame an expression is resolved in; an address range needs none.
 */
stop?: number | null, thread?: number | null, task?: TaskKey | null, frame?: number | null, condition?: string | null, hitCondition?: string | null, };

export type EditWatchpoint = { id: number, condition?: string | null, hitCondition?: string | null, };

export type WatchpointRef = { id: number, };

export type WatchAccess = "change" | "write" | "readWrite" | "read";

export type Syntax = "intel" | "att";

export type SignalPolicy = { signal: number, 
/**
 * Such as `SIGUSR1`; filled in by the server.
 */
name: string, stop: boolean, print: boolean, pass: boolean, };

export type Watchpoint = { id: number, access: WatchAccess, 
/**
 * The expression it watches, when it was given one.
 */
expression: string | null, address: string, bytes: number, 
/**
 * Such as `thread 41872's frame at 0x7ffe…, until it returns` for a
 * local, which ends with its frame; empty for a global.
 */
scope: string, condition: string | null, hitCondition: string | null, hits: number, };

export type Disassembled = { 
/**
 * The function shown, when the code shown is one.
 */
function: string | null, 
/**
 * The frame's instruction: its program counter, or, in a caller, the
 * call it returns to after.
 */
marked: string | null, instructions: Array<Instruction>, 
/**
 * What the code shown leaves out, such as unreadable memory.
 */
notes: Array<string>, };

export type Instruction = { address: string, 
/**
 * Its bytes, as hexadecimal pairs separated by spaces.
 */
bytes: string, 
/**
 * Its text in pieces; empty when the bytes decode to nothing.
 */
tokens: Array<Token>, 
/**
 * Why there is no instruction, when there is none.
 */
invalid: string | null, 
/**
 * Where a branch goes, which a page can follow.
 */
target: BranchTarget | null, 
/**
 * What its operands name, such as a function or a global.
 */
comment: string | null, 
/**
 * The symbol it is in, with its offset, such as `main+12`.
 */
symbol: string | null, 
/**
 * Its source line, when it starts one.
 */
source: SourceLine | null, };

export type Token = { 
/**
 * `mnemonic`, `prefix`, `keyword`, `register`, `number`, `address`,
 * `punctuation`, or `text`.
 */
kind: string, text: string, };

export type BranchTarget = { address: string, name: string | null, };

export type Memory = { address: string, 
/**
 * The bytes read, as hexadecimal pairs with no separator.
 */
bytes: string, 
/**
 * The first address that could not be read, when the read stopped
 * short.
 */
unreadable: string | null, };

export type StepTargets = { calls: Array<StepCall>, };

export type StepCall = { 
/**
 * The call instruction's address, in hexadecimal, which names it to a
 * step.
 */
call: string, 
/**
 * The function it calls, when something names it.
 */
callee: string | null, 
/**
 * The address a direct call calls, in hexadecimal; none for an
 * indirect call.
 */
target: string | null, };

export type Registers = { registers: Array<Register>, };

export type Register = { name: string, 
/**
 * As hexadecimal; absent where a caller's frame did not save it.
 */
value: string | null, bits: number, 
/**
 * `pc`, `sp`, or `fp`.
 */
role: string | null, };

export type Signals = { signals: Array<SignalPolicy>, };

export type Modules = { modules: Array<Module>, };

export type Module = { id: number, name: string, path: string, 
/**
 * Where it is loaded, as hexadecimal.
 */
start: string | null, end: string | null, 
/**
 * `debug` with debug information, `symbols` with only a symbol table,
 * or `none`.
 */
symbols: string, 
/**
 * The separate file its debug information came from, when its own
 * file was stripped of it.
 */
debugFile: string | null, 
/**
 * Why the separate debug file found for it could not be used.
 */
debugFileProblem: string | null, 
/**
 * Why not all of its debug information could be read.
 */
debugInformationProblem: string | null, };

export type StopAt = { stop: number, };

export type TaskKey = { runtime: number, number: number, };

export type TaskList = { 
/**
 * What the runtimes call a task, such as `task` or `goroutine`.
 */
noun: string | null, tasks: Array<Task>, 
/**
 * Whether there are more tasks than those listed, which the list
 * leaves out to stay small.
 */
more: boolean, 
/**
 * Why the list may be missing tasks, or describe some wrongly.
 */
gaps: Array<string>, };

export type Task = { key: TaskKey, state: TaskState, 
/**
 * The code the program wrote that it is in, as the CLI says it.
 */
place: string, 
/**
 * The frame of that code in the task's stack, which choosing the task
 * shows; absent for a task that runs only its runtime's code.
 */
frame: Frame | null, 
/**
 * The runtime's own words for what it does or waits for.
 */
detail: string | null, 
/**
 * Labels the program or runtime gave it, such as `{runtime: "local
 * set 3"}`.
 */
labels: string | null, 
/**
 * The thread running it.
 */
thread: number | null, };

export type TaskState = "running" | "runnable" | "blocked" | "exited" | "unknown";

export type Draw = { 
/**
 * The value's `path`, which evaluates it.
 */
path: string, 
/**
 * The renderer: one whose drawing the value's view offers, or else any
 * renderer, which then draws the value itself as its `values`.
 */
renderer: string, stop: number, thread: number, task?: TaskKey | null, frame: number, };

export type RendererRef = { digest: string, };

export type RendererInfo = { name: string, 
/**
 * The file or module record it was loaded from, or `built-in`.
 */
origin: string, digest: string, };

export type RendererSource = { source: string, name: string, 
/**
 * The file or module record it was loaded from, or `built-in`.
 */
origin: string, digest: string, };

export type RendererList = { renderers: Array<RendererInfo>, };

export type Drawing = { renderer: RendererInfo, 
/**
 * Whether the value's view offers the drawing; otherwise the renderer
 * draws the value itself as its `values`.
 */
offered: boolean, 
/**
 * Each input, by name, in order; none when a problem kept any part of
 * them from being read.
 */
inputs: Array<DrawInput> | null, 
/**
 * Why the inputs could not be read whole: the first part that could
 * not, and why.
 */
problem: string | null, 
/**
 * How many bytes the binary frame sent just before this answer holds:
 * the bytes and numbers the inputs' `bytes` and `numbers` lie in.
 */
bytes: number, };

export type DrawInput = { name: string, value: Datum, };

export type Datum = { "t": "bool", b: boolean, } | { "t": "int", i: number, } | { "t": "big", big: string, } | { "t": "float", f: string, } | { "t": "text", s: string, } | { "t": "enum", name: string | null, value: Datum, } | { "t": "sum", variant: string, value: Datum | null, } | { "t": "record", members: Array<[string, Datum]>, } | { "t": "list", items: Array<Datum>, } | { "t": "numbers", kind: NumberKind, offset: number, count: number, } | { "t": "bytes", offset: number, length: number, } | { "t": "entries", entries: Array<[Datum, Datum]>, } | { "t": "null" };

export type NumberKind = "i8" | "u8" | "i16" | "u16" | "i32" | "u32" | "i64" | "u64" | "f32" | "f64";
