// User acceptance test: VS Code itself drives `uscope dap` through the
// extension, as a user would from the editor, and every message each
// session exchanges is recorded in the adapter log's format.
//
// Each scenario drives VS Code's own commands where VS Code has one for
// what a user does: breakpoints, stepping, the debug console, the watch
// view, hovers, run to cursor, the disassembly view, restart. It falls
// back on a session's requests only for what has no command, and checks
// what VS Code asks the adapter and what it is answered, which is what a
// user sees.
//
// Run by run.sh through VS Code's extension test mode.

'use strict';

const assert = require('assert');
const childProcess = require('child_process');
const fs = require('fs');
const path = require('path');
const vscode = require('vscode');

const root = process.env.USCOPE_UAT_ROOT;
const record = process.env.USCOPE_UAT_RECORD;
const fixture = (name) => path.join(root, 'build/test-programs', name);
const source = (name) => path.join(root, 'tests/fixtures/c', name);
const extension = () => require(path.join(root, 'editors/vscode/extension.js'));
const TIMEOUT = 15000;

// Every message of every session, and the waiters for adapter messages.
const transcripts = new Map();
let waiters = [];

vscode.debug.registerDebugAdapterTrackerFactory('uscope', {
    createDebugAdapterTracker(session) {
        const lines = [];
        // A restart that starts another adapter for the session keeps both
        // transcripts.
        let name = session.configuration.name;
        while (transcripts.has(name)) {
            name = `${name}.restarted`;
        }
        transcripts.set(name, lines);
        const notify = (direction, message) => {
            waiters = waiters.filter((waiter) => !waiter(session, message, direction));
        };
        return {
            onWillReceiveMessage: (message) => {
                lines.push(`<- ${JSON.stringify(message)}`);
                notify('client', message);
            },
            onDidSendMessage: (message) => {
                lines.push(`-> ${JSON.stringify(message)}`);
                notify('adapter', message);
            },
            onError: (error) => lines.push(`!! ${error}`),
        };
    },
});

/** Waits for a message the predicate accepts, from the adapter or, with `from`, the client. */
function message(description, predicate, from = 'adapter') {
    return new Promise((resolve, reject) => {
        const timer = setTimeout(
            () => reject(new Error(`timed out waiting for ${description}`)),
            TIMEOUT,
        );
        waiters.push((session, sent, direction) => {
            if (direction !== from || !predicate(session, sent)) {
                return false;
            }
            clearTimeout(timer);
            resolve({ session, message: sent });
            return true;
        });
    });
}

const event = (name, predicate = () => true) =>
    message(`a ${name} event`, (_, sent) =>
        sent.type === 'event' && sent.event === name && predicate(sent.body ?? {}));

const response = (command, predicate = () => true) =>
    message(`a ${command} response`, (_, sent) =>
        sent.type === 'response' && sent.command === command && predicate(sent));

/** Waits for a request VS Code sends the adapter. */
const request = (command, predicate = () => true) =>
    message(`VS Code to send ${command}`, (_, sent) =>
        sent.type === 'request' && sent.command === command && predicate(sent.arguments ?? {}), 'client');

/** Waits for the response to the next request VS Code sends that the predicate accepts. */
async function answer(command, predicate = () => true) {
    let seq;
    const sent = request(command, (arguments_) => predicate(arguments_));
    const answered = message(`the answer to ${command}`, (_, reply) =>
        reply.type === 'response' && reply.command === command && seq !== undefined && reply.request_seq === seq);
    seq = (await sent).message.seq;
    return (await answered).message;
}

/** Bounds a wait. */
function within(description, promise) {
    let timer;
    const timeout = new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error(`timed out waiting for ${description}`)), TIMEOUT);
    });
    return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

/** Polls until the predicate holds, or the wait times out. */
function until(description, predicate) {
    return within(description, new Promise((resolve) => {
        const poll = () => (predicate() ? resolve() : setTimeout(poll, 20));
        poll();
    }));
}

/** Waits for a session to end: the one with the name, if given. */
function terminated(name) {
    return within('the session to end', new Promise((resolve) => {
        const listener = vscode.debug.onDidTerminateDebugSession((session) => {
            if (name === undefined || session.name === name) {
                listener.dispose();
                resolve(session);
            }
        });
    }));
}

/** Saves each session's transcript, named after its configuration. */
function save() {
    fs.mkdirSync(record, { recursive: true });
    for (const [name, lines] of transcripts) {
        const file = name.replace(/[^A-Za-z0-9._-]+/g, '-');
        fs.writeFileSync(path.join(record, `${file}.log`), `${lines.join('\n')}\n`);
    }
}

/** Removes breakpoints and waits until the adapter has them. */
async function remove(breakpoint) {
    const command = breakpoint instanceof vscode.FunctionBreakpoint ? 'setFunctionBreakpoints' : 'setBreakpoints';
    const applied = response(command);
    vscode.debug.removeBreakpoints([breakpoint]);
    await applied;
}

let focused;

/**
 * Waits for VS Code to focus a frame of a new stop, after which its
 * commands act on that stop. A session never reuses a frame id.
 */
function focus() {
    return new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error('timed out waiting for a focused frame')), TIMEOUT);
        const check = (item) => {
            if (!(item instanceof vscode.DebugStackFrame) || `${item.session.id} ${item.frameId}` === focused) {
                return false;
            }
            focused = `${item.session.id} ${item.frameId}`;
            clearTimeout(timer);
            resolve(item);
            return true;
        };
        if (check(vscode.debug.activeStackItem)) {
            return;
        }
        const listener = vscode.debug.onDidChangeActiveStackItem((item) => {
            if (check(item)) {
                listener.dispose();
            }
        });
    });
}

function lineOf(file, marker) {
    const index = fs.readFileSync(file, 'utf8').split('\n').findIndex((line) => line.includes(marker));
    assert.notStrictEqual(index, -1, `${marker} in ${file}`);
    return index;
}

function sourceBreakpoint(file, marker, options = {}) {
    const location = new vscode.Location(vscode.Uri.file(file), new vscode.Position(lineOf(file, marker), 0));
    return new vscode.SourceBreakpoint(location, true, options.condition, options.hitCondition, options.logMessage);
}

async function topFrame(session, threadId) {
    const trace = await session.customRequest('stackTrace', { threadId, startFrame: 0, levels: 1 });
    return trace.stackFrames[0];
}

const workspace = () => vscode.workspace.workspaceFolders[0];

/** Starts a session and waits for its first stop. */
async function launchToStop(configuration, reason) {
    const stopped = event('stopped', (body) => reason === undefined || body.reason === reason);
    assert.ok(await vscode.debug.startDebugging(undefined, { type: 'uscope', request: 'launch', cwd: root, ...configuration }));
    const { session, message: sent } = await stopped;
    // VS Code focuses a frame with source, or else the stopped thread.
    const frame = reason === 'entry' ? await focusThread(sent.body.threadId) : await focus();
    return { session, threadId: sent.body.threadId, frame, body: sent.body };
}

/** Waits for VS Code to focus a thread, or a frame of it. */
function focusThread(threadId) {
    return until('a focused thread', () => {
        const item = vscode.debug.activeStackItem;
        return (item instanceof vscode.DebugThread || item instanceof vscode.DebugStackFrame)
            && item.threadId === threadId;
    });
}

/** Continues the focused session, removes every breakpoint first, and waits for it to end. */
async function finish(exitCode = 0) {
    for (const breakpoint of vscode.debug.breakpoints) {
        await remove(breakpoint);
    }
    const exited = event('exited');
    const ended = terminated();
    await vscode.commands.executeCommand('workbench.action.debug.continue');
    assert.strictEqual((await exited).message.body.exitCode, exitCode);
    await ended;
}

/** Ends a session by disconnecting from it. */
async function stop(session) {
    const ended = terminated(session.name);
    await vscode.debug.stopDebugging(session);
    await ended;
}

/**
 * Runs a VS Code command on text selected in a scratch editor, as a user
 * selecting text and choosing "Evaluate in Debug Console" or "Add to Watch"
 * does, and returns the adapter's answer to the evaluation it causes.
 */
async function onSelection(command, text, context) {
    const document = await vscode.workspace.openTextDocument({ content: text });
    const editor = await vscode.window.showTextDocument(document, { preview: true });
    editor.selection = new vscode.Selection(0, 0, 0, text.length);
    const answered = answer('evaluate', (arguments_) => arguments_.expression === text && arguments_.context === context);
    await vscode.commands.executeCommand(command);
    const result = await answered;
    await vscode.commands.executeCommand('workbench.action.revertAndCloseActiveEditor');
    return result;
}

/**
 * Evaluates a line in the debug console. Commands' output is styled for
 * VS Code, which shows ANSI styling; the result is returned without it.
 */
async function inConsole(text) {
    const reply = await onSelection('editor.debug.action.selectionToRepl', text, 'repl');
    // eslint-disable-next-line no-control-regex
    const plain = (styled) => styled?.replace(/\x1b\[[0-9;]*m/g, '');
    if (reply.body?.result !== undefined) {
        reply.body.result = plain(reply.body.result);
    }
    reply.message = plain(reply.message);
    return reply;
}

/** Pressing F5 in a folder without a launch.json creates one to fill in. */
async function createALaunchJson() {
    const folder = workspace().uri.fsPath;
    const program = path.join(folder, 'main.c');
    const launchJson = path.join(folder, '.vscode', 'launch.json');
    fs.writeFileSync(program, 'int main(void) { return 0; }\n');
    try {
        // The C file makes uscope the only debugger to guess.
        await vscode.window.showTextDocument(vscode.Uri.file(program));
        await vscode.commands.executeCommand('workbench.action.debug.start');
        await until('launch.json', () => fs.existsSync(launchJson));
        // VS Code writes its own comments at the top of the file.
        const text = fs.readFileSync(launchJson, 'utf8').replace(/^\s*\/\/.*$/gm, '');
        const { configurations } = JSON.parse(text);
        assert.deepStrictEqual(configurations.map(({ type, request: kind }) => ({ type, request: kind })), [
            { type: 'uscope', request: 'launch' },
        ]);
        assert.strictEqual(vscode.debug.activeDebugSession, undefined);
    } finally {
        await vscode.commands.executeCommand('workbench.action.closeAllEditors');
        fs.rmSync(path.join(folder, '.vscode'), { recursive: true, force: true });
        fs.rmSync(program, { force: true });
    }
}

/** The uscope.path setting chooses the adapter, and a missing one is named. */
async function findTheAdapterFromTheSetting() {
    const folder = workspace();
    const wrapper = path.join(folder.uri.fsPath, 'uscope-wrapper');
    const marker = `${wrapper}.used`;
    fs.writeFileSync(wrapper, `#!/bin/sh\ntouch '${marker}'\nexec uscope "$@"\n`, { mode: 0o755 });
    const settings = vscode.workspace.getConfiguration('uscope');
    const configuration = (name) => ({ type: 'uscope', request: 'launch', name, program: fixture('basic'), cwd: root });
    try {
        await settings.update('path', '${workspaceFolder}/missing-uscope', vscode.ConfigurationTarget.Global);
        await assert.rejects(
            vscode.debug.startDebugging(folder, configuration('vscode-missing-adapter')),
            /Cannot find uscope at "\$\{workspaceFolder\}\/missing-uscope"/,
        );

        await settings.update('path', '${workspaceFolder}/uscope-wrapper', vscode.ConfigurationTarget.Global);
        const exited = event('exited');
        const ended = terminated('vscode-setting');
        assert.ok(await vscode.debug.startDebugging(folder, configuration('vscode-setting')));
        assert.strictEqual((await exited).message.body.exitCode, 0);
        await ended;
        assert.ok(fs.existsSync(marker), 'the session ran the configured adapter');
    } finally {
        await settings.update('path', undefined, vscode.ConfigurationTarget.Global);
        fs.rmSync(wrapper, { force: true });
        fs.rmSync(marker, { force: true });
    }
}

/** The uscope.logFile setting has the adapter log every message. */
async function logTheProtocolToAFile() {
    const log = path.join(workspace().uri.fsPath, 'uscope-dap.log');
    const settings = vscode.workspace.getConfiguration('uscope');
    try {
        await settings.update('logFile', '${workspaceFolder}/uscope-dap.log', vscode.ConfigurationTarget.Global);
        const exited = event('exited');
        const ended = terminated('vscode-log');
        assert.ok(await vscode.debug.startDebugging(workspace(), {
            type: 'uscope', request: 'launch', name: 'vscode-log', program: fixture('basic'),
        }));
        await exited;
        await ended;
        const text = fs.readFileSync(log, 'utf8');
        assert.match(text, /"command":"initialize"/);
        assert.match(text, /"event":"exited"/);
    } finally {
        await settings.update('logFile', undefined, vscode.ConfigurationTarget.Global);
        fs.rmSync(log, { force: true });
    }
}

async function launchStepInspectAndRestart() {
    const breakpoint = sourceBreakpoint(source('basic.c'), 'return uscope_value;');
    vscode.debug.addBreakpoints([breakpoint]);
    const stopped = event('stopped', (body) => body.reason === 'breakpoint');
    assert.ok(await vscode.debug.startDebugging(undefined, {
        type: 'uscope', request: 'launch', name: 'vscode-launch', program: fixture('basic'), cwd: root,
    }));
    const { session, message: sent } = await stopped;
    const threadId = sent.body.threadId;
    // VS Code focuses the stopped frame itself.
    await focus();
    let frame = await topFrame(session, threadId);
    assert.strictEqual(frame.name, 'breakpoint_target');
    assert.strictEqual(frame.line, lineOf(source('basic.c'), 'return uscope_value;') + 1);
    const value = await session.customRequest('evaluate', {
        expression: 'uscope_value', frameId: frame.id, context: 'watch',
    });
    assert.strictEqual(value.result, '1234605616436508552');

    // Step out with the toolbar's command, then over with the keyboard's.
    let step = event('stopped', (body) => body.reason === 'step');
    await vscode.commands.executeCommand('workbench.action.debug.stepOut');
    await step;
    await focus();
    frame = await topFrame(session, threadId);
    assert.strictEqual(frame.name, 'main');
    step = event('stopped', (body) => body.reason === 'step');
    await vscode.commands.executeCommand('workbench.action.debug.stepOver');
    await step;
    await focus();

    // The disassembly view loads instructions around the current one.
    const disassembled = response('disassemble', (reply) => reply.success && reply.body.instructions.length > 0);
    await vscode.commands.executeCommand('debug.action.openDisassemblyView');
    await disassembled;

    // Restarting runs the program again to its breakpoint.
    const again = event('stopped', (body) => body.reason === 'breakpoint');
    await vscode.commands.executeCommand('workbench.action.debug.restart');
    await again;
    await focus();

    await remove(breakpoint);
    const exited = event('exited');
    const ended = terminated();
    await vscode.commands.executeCommand('workbench.action.debug.continue');
    assert.strictEqual((await exited).message.body.exitCode, 0);
    await ended;
}

/**
 * Stepping into a call, and with the disassembly view focused, by
 * instruction; run to cursor stops where the cursor is.
 */
async function stepIntoByInstructionAndRunToCursor() {
    const file = source('basic.c');
    const { session, threadId } = await launchToStop({
        name: 'vscode-stepping', program: fixture('basic'), stopOnEntry: true,
    }, 'entry');
    const editor = await vscode.window.showTextDocument(vscode.Uri.file(file));
    const line = lineOf(file, 'uint64_t second = breakpoint_target();');
    editor.selection = new vscode.Selection(line, 4, line, 4);
    let stopped = event('stopped');
    await vscode.commands.executeCommand('editor.debug.action.runToCursor');
    await stopped;
    await focus();
    assert.strictEqual((await topFrame(session, threadId)).line, line + 1);

    // Stepping in enters the call on the line.
    const into = request('stepIn');
    stopped = event('stopped', (body) => body.reason === 'step');
    await vscode.commands.executeCommand('workbench.action.debug.stepInto');
    assert.notStrictEqual((await into).message.arguments.granularity, 'instruction');
    await stopped;
    await focus();
    assert.strictEqual((await topFrame(session, threadId)).name, 'breakpoint_target');

    // With the disassembly view focused, steps are by instruction.
    const disassembled = response('disassemble', (reply) => reply.success);
    await vscode.commands.executeCommand('debug.action.openDisassemblyView');
    await disassembled;
    const before = (await topFrame(session, threadId)).instructionPointerReference;
    const next = request('next');
    stopped = event('stopped', (body) => body.reason === 'step');
    await vscode.commands.executeCommand('workbench.action.debug.stepOver');
    assert.strictEqual((await next).message.arguments.granularity, 'instruction');
    await stopped;
    await focus();
    const after = await topFrame(session, threadId);
    assert.notStrictEqual(after.instructionPointerReference, before);
    assert.strictEqual(after.name, 'breakpoint_target');
    await vscode.commands.executeCommand('workbench.action.closeAllEditors');
    await finish();
}

/**
 * Conditions, hit counts, and logpoints stop and print where they say,
 * and breakpoints that cannot be placed say why.
 */
async function conditionsHitCountsLogpointsAndRefusals() {
    const file = source('hit-counts.c');
    const breakpoints = [
        sourceBreakpoint(file, 'last_call = call;', { condition: 'call == 5' }),
        sourceBreakpoint(file, 'counted(call);', { logMessage: 'calling {call} after {last_call}' }),
        new vscode.FunctionBreakpoint('caller', true, undefined, '>= 39'),
        // Each of these says why it is not placed.
        sourceBreakpoint(file, 'shared(call);', { condition: 'call ==' }),
        sourceBreakpoint(file, 'shared(call + SECOND_SITE_OFFSET);', { condition: 'call = 3' }),
        sourceBreakpoint(file, 'for (uint64_t call = 1', { hitCondition: '5' }),
        new vscode.FunctionBreakpoint('no_such_function'),
    ];
    vscode.debug.addBreakpoints(breakpoints);
    const placed = answer('setBreakpoints', (arguments_) => arguments_.source.path === file);
    const functions = answer('setFunctionBreakpoints');
    const logged = event('output', (body) => body.output === 'calling 5 after 4\n');
    const { session, threadId, frame } = await launchToStop({
        name: 'vscode-breakpoints', program: fixture('hit-counts-gcc-o0'),
    }, 'breakpoint');
    const lines = (await placed).body.breakpoints;
    assert.deepStrictEqual(lines.slice(0, 2).map((breakpoint) => breakpoint.verified), [true, true]);
    for (const [breakpoint, reason] of [
        [lines[2], /invalid condition: expected an operand/],
        [lines[3], /assign/],
        [lines[4], /a bare count is ambiguous; write ==5 .* or >=5/],
    ]) {
        assert.strictEqual(breakpoint.verified, false, JSON.stringify(breakpoint));
        assert.match(breakpoint.message, reason);
    }
    const [caller, missing] = (await functions).body.breakpoints;
    assert.strictEqual(caller.verified, true);
    assert.strictEqual(missing.verified, false);
    assert.match(missing.message, /no_such_function/);

    // The condition held at the fifth call, after logging each before it.
    await logged;
    const call = await session.customRequest('evaluate', { expression: 'call', frameId: frame.frameId, context: 'watch' });
    assert.strictEqual(call.result, '5');
    await remove(breakpoints[0]);
    await remove(breakpoints[1]);
    // The function breakpoint stops from its 39th hit on.
    const hit = event('stopped', (body) => body.reason === 'function breakpoint');
    await vscode.commands.executeCommand('workbench.action.debug.continue');
    await hit;
    const at = await focus();
    const count = await session.customRequest('evaluate', { expression: 'call', frameId: at.frameId, context: 'watch' });
    assert.strictEqual(count.result, '39');
    assert.strictEqual((await topFrame(session, threadId)).name, 'caller');
    await finish();
}

/**
 * The debug console evaluates expressions, assigns, and runs commands; the
 * watch view and hovers show whole expressions; the variables view
 * follows assignments and the hexadecimal setting; values show inline.
 */
async function consoleWatchHoverHexadecimalAndInlineValues() {
    const file = source('command-names.c');
    const line = lineOf(file, 'volatile int sink');
    vscode.debug.addBreakpoints([sourceBreakpoint(file, 'volatile int sink')]);
    await vscode.commands.executeCommand('workbench.view.debug');
    // VS Code shows values inline by asking the extension, which asks the
    // adapter where each variable is declared.
    const declared = request('locations');
    const { session, frame } = await launchToStop({ name: 'vscode-console', program: fixture('command-names') }, 'breakpoint');
    const editor = await vscode.window.showTextDocument(vscode.Uri.file(file), { preserveFocus: false });
    await declared;

    // Lines are expressions unless they name a command the frame has no
    // variable for.
    for (const [text, result] of [
        ['x + 1', '21'], ['list', '5'], ['n', '10'], ['p/x list', '(int) list = 0x5'],
        ['(u8)-1', '255'], ['$rip == $pc', 'true'], ['sizeof(origin)', '8'],
    ]) {
        const reply = await inConsole(text);
        assert.ok(reply.success, JSON.stringify(reply));
        assert.strictEqual(reply.body.result, result, text);
    }
    assert.match((await inConsole('bt')).body.result, /^#0 .*names/);
    assert.match((await inConsole('whatis where')).body.result, /point \*/);
    // Mistakes are answered, not fatal.
    for (const [text, reason] of [
        ['next', /`next` is not available in the debug console/],
        ['x + missing', /missing[\s\S]*\^{7}/],
        ['origin + 1', /type/],
        ['1 / 0', /divi/],
    ]) {
        const reply = await inConsole(text);
        assert.strictEqual(reply.success, false, text);
        assert.match(reply.message, reason, text);
    }

    // Watching follows the stop.
    await vscode.commands.executeCommand('workbench.debug.action.focusWatchView');
    const watched = await onSelection('editor.debug.action.selectionToWatch', 'where->y + x', 'watch');
    assert.strictEqual(watched.body.result, '22');

    // Assigning in the console updates what the views show.
    const invalidated = event('invalidated');
    const reread = answer('evaluate', (arguments_) => arguments_.context === 'watch' && arguments_.expression === 'where->y + x');
    assert.strictEqual((await inConsole('x = 7')).body.result, '7');
    await invalidated;
    assert.strictEqual((await reread).body.result, '9');

    // Hovering a member shows the whole expression it ends. The mouse may
    // hover something else meanwhile.
    const hover = answer('evaluate', (arguments_) => arguments_.context === 'hover' && arguments_.expression === 'where->y');
    const shown = await vscode.window.showTextDocument(editor.document);
    const text = shown.document.lineAt(line).text;
    const column = text.indexOf('where->y') + 'where->'.length;
    shown.selection = new vscode.Selection(line, column, line, column);
    await vscode.commands.executeCommand('editor.debug.action.showDebugHover');
    assert.strictEqual((await hover).body.result, '2');
    // So does hovering an indexed or qualified name, which VS Code alone
    // would cut at its brackets or colons.
    for (const [text, column, expression] of [
        ['total += items[i + 1].value;', 23, 'items[i + 1].value'],
        ['return ns::config.limit;', 19, 'ns::config.limit'],
        ['x = $rip;', 6, '$rip'],
        ['f(a)->b', 6, 'f(a)->b'],
        ['(*node).next', 10, '(*node).next'],
        ['len(items).x', 11, 'len(items).x'],
        ['// 12', 3, undefined],
    ]) {
        assert.strictEqual(extension().expressionAt(text, column)?.expression, expression, text);
    }

    // The hexadecimal setting shows every value in hexadecimal.
    const formatted = request('uscope/setValueFormat', (arguments_) => arguments_.hex === true);
    const hexadecimal = answer('evaluate', (arguments_) => arguments_.context === 'watch' && arguments_.expression === 'where->y + x');
    await vscode.commands.executeCommand('uscope.toggleHexadecimal');
    await formatted;
    assert.strictEqual((await hexadecimal).body.result, '0x9');
    const decimal = request('uscope/setValueFormat', (arguments_) => arguments_.hex === false);
    await vscode.commands.executeCommand('uscope.toggleHexadecimal');
    await decimal;

    // Inline values are each variable's, on the lines it is in scope.
    const values = await extension().provideInlineValues(editor.document, new vscode.Range(0, 0, line + 1, 0), {
        frameId: frame.frameId, stoppedLocation: new vscode.Range(line, 0, line, 0),
    });
    const inline = values.map((value) => `${value.range.start.line - line}: ${value.text}`);
    for (const expected of ['0: x = 7', '0: list = 5', '0: shadowed = -1', `${lineOf(file, 'int x = n * 2') - line}: x = 7`]) {
        assert.ok(inline.includes(expected), `${expected} in ${inline}`);
    }
    // A member named like a variable is no use of it.
    assert.ok(!inline.some((value) => value.endsWith(': y = 2')), `${inline}`);
    await vscode.commands.executeCommand('workbench.action.closeAllEditors');
    await vscode.commands.executeCommand('workbench.debug.viewlet.action.removeAllWatchExpressions');
    // The program checks its sum, which the assignment to x changed.
    await finish(1);
}

/**
 * The variables view links a function pointer to its function, and the
 * console completes members, registers, and globals.
 */
async function completionsAndLocations() {
    const file = source('variables.c');
    vscode.debug.addBreakpoints([sourceBreakpoint(file, 'return **pointer_pointer')]);
    await vscode.commands.executeCommand('workbench.debug.action.focusVariablesView');
    const listed = response('variables', (reply) => reply.body?.variables?.some((variable) => variable.name === 'function_pointer'));
    const { session, frame } = await launchToStop({ name: 'vscode-locations', program: fixture('variables-gcc-o0') }, 'breakpoint');
    // VS Code shows the locals, where the function pointer's value links to
    // the function.
    const pointer = (await listed).message.body.variables.find((variable) => variable.name === 'function_pointer');
    assert.ok(pointer.valueLocationReference > 0, JSON.stringify(pointer));
    const location = await session.customRequest('locations', { locationReference: pointer.valueLocationReference });
    assert.strictEqual(location.source.path, file);
    assert.strictEqual(location.line, lineOf(file, 'static int pointer_identity(int value)') + 1);
    const complete = async (text) => (await session.customRequest('completions', {
        text, column: text.length + 1, frameId: frame.frameId,
    })).targets.map((target) => target.label);
    assert.deepStrictEqual(await complete('pair.'), ['first', 'second']);
    assert.deepStrictEqual(await complete('structure_pointer->s'), ['second']);
    assert.ok((await complete('$rs')).includes('rsp'));
    assert.ok((await complete('pointer_parameter_v')).includes('pointer_parameter_value'));
    await finish();
}

/**
 * Data breakpoints stop where the value changes, or at every store in
 * their store mode, and the console removing one removes the client's.
 */
async function dataBreakpointsAndTheirModes() {
    const breakpoint = new vscode.FunctionBreakpoint('scalar_stores');
    vscode.debug.addBreakpoints([breakpoint]);
    const { session, frame } = await launchToStop({ name: 'vscode-data', program: fixture('watch-gcc-o0') }, 'function breakpoint');
    await remove(breakpoint);
    const capabilities = await session.customRequest('dataBreakpointInfo', { name: 'watch_i32', frameId: frame.frameId });
    assert.deepStrictEqual(capabilities.accessTypes, ['write', 'readWrite']);
    const set = await session.customRequest('setDataBreakpoints', {
        breakpoints: [{ dataId: capabilities.dataId, accessType: 'write', mode: 'store' }],
    });
    assert.strictEqual(set.breakpoints[0].verified, true);
    for (const description of [
        'watch_i32 changed from 0 to 1', 'watch_i32 changed from 1 to 2',
        'watch_i32 was written; it is still 2', 'watch_i32 changed from 2 to 42',
    ]) {
        const stopped = event('stopped', (body) => body.reason === 'data breakpoint');
        await vscode.commands.executeCommand('workbench.action.debug.continue');
        assert.strictEqual((await stopped).message.body.description, description);
        await focus();
    }
    const removed = event('breakpoint', (body) => body.reason === 'removed' && body.breakpoint.id === set.breakpoints[0].id);
    assert.match((await inConsole('unwatch all')).body.result, /1 watchpoint/);
    await removed;
    // The program raises signals of its own later.
    await session.customRequest('setExceptionBreakpoints', { filters: [] });
    await finish();
}

/** A crash stops at the signal, explains it, and ends the program when continued. */
async function crashAtASignal() {
    const exception = answer('exceptionInfo');
    const { body } = await launchToStop({ name: 'vscode-crash', program: fixture('crash-gcc-o0') }, 'exception');
    assert.strictEqual(body.text, 'SIGSEGV');
    // VS Code asks what the exception is, to show it at the stop.
    const info = (await exception).body;
    assert.strictEqual(info.exceptionId, 'SIGSEGV');
    const explained = event('output', (output) => /terminated by SIGSEGV/.test(output.output));
    await finish(139);
    await explained;
}

/** Pausing stops every thread, which VS Code lists by name. */
async function pauseAndThreads() {
    const breakpoint = new vscode.FunctionBreakpoint('worker_breakpoint');
    vscode.debug.addBreakpoints([breakpoint]);
    const { session, threadId } = await launchToStop({ name: 'vscode-threads', program: fixture('threads') }, 'function breakpoint');
    // Every thread stopped with the one that hit the breakpoint.
    const { threads } = await session.customRequest('threads');
    assert.strictEqual(threads.length, 3, JSON.stringify(threads));
    assert.ok(threads.some((thread) => thread.id === threadId));
    await finish();

    // VS Code lists a running program's threads when told one started.
    const spinning = event('thread', (body) => body.reason === 'started')
        .then(() => answer('threads'));
    assert.ok(await vscode.debug.startDebugging(undefined, {
        type: 'uscope', request: 'launch', name: 'vscode-spin', program: fixture('spin'), cwd: root,
    }));
    await spinning;
    const spin = vscode.debug.activeDebugSession;
    // Pause, as the call stack's menu on the session does.
    const paused = event('stopped', (body) => body.reason === 'pause');
    await vscode.commands.executeCommand('workbench.action.debug.pause', { sessionId: spin.id });
    await paused;
    await focus();
    await stop(spin);
}

/** Breakpoints in a library wait for it, and its sources come and go with it. */
async function librariesAndTheirSources() {
    const file = source('shared/library.c');
    const breakpoint = sourceBreakpoint(file, 'return *dso_pointer');
    const after = new vscode.FunctionBreakpoint('after_unload');
    vscode.debug.addBreakpoints([breakpoint, after]);
    const pending = answer('setBreakpoints', (arguments_) => arguments_.source.path === file);
    const { session } = await launchToStop({
        name: 'vscode-library', program: fixture('globals-shared'), stopOnEntry: true,
    }, 'entry');
    assert.strictEqual((await pending).body.breakpoints[0].reason, 'pending');
    // The loaded scripts view lists the sources, and follows them.
    const listed = answer('loadedSources');
    await vscode.commands.executeCommand('workbench.debug.loadedScriptsView.focus');
    assert.ok(!(await listed).body.sources.some((entry) => entry.path === file));
    const loaded = event('loadedSource', (body) => body.reason === 'new' && body.source.path === file);
    const resolved = event('breakpoint', (body) => body.breakpoint.verified && body.breakpoint.source?.path === file);
    const hit = event('stopped', (body) => body.reason === 'breakpoint');
    await vscode.commands.executeCommand('workbench.action.debug.continue');
    await loaded;
    await resolved;
    const { message: stopped } = await hit;
    await focus();
    assert.strictEqual((await topFrame(session, stopped.body.threadId)).source.path, file);
    const unloaded = event('loadedSource', (body) => body.reason === 'removed' && body.source.path === file);
    const unloadStop = event('stopped', (body) => body.reason === 'function breakpoint');
    await vscode.commands.executeCommand('workbench.action.debug.continue');
    await unloaded;
    await unloadStop;
    await focus();
    await stop(session);
    vscode.debug.removeBreakpoints(vscode.debug.breakpoints);
}

async function runInTheIntegratedTerminal() {
    const breakpoint = sourceBreakpoint(source('process-environment.c'), 'if (fgets(line');
    vscode.debug.addBreakpoints([breakpoint]);
    const stopped = event('stopped', (body) => body.reason === 'breakpoint');
    const process = event('process');
    assert.ok(await vscode.debug.startDebugging(undefined, {
        type: 'uscope', request: 'launch', name: 'vscode-terminal', program: fixture('process-environment'),
        args: ['one'], console: 'integratedTerminal', cwd: root,
    }));
    await stopped;
    await focus();
    assert.strictEqual((await process).message.body.startMethod, 'launch');
    const terminal = vscode.window.terminals.find((candidate) => candidate.name.includes('process-environment'));
    assert.ok(terminal, `a terminal among ${vscode.window.terminals.map((candidate) => candidate.name)}`);
    await remove(breakpoint);
    const exited = event('exited');
    const ended = terminated();
    await vscode.commands.executeCommand('workbench.action.debug.continue');
    // The program reads its line from the terminal.
    terminal.sendText('typed');
    assert.strictEqual((await exited).message.body.exitCode, 2);
    await ended;
    terminal.dispose();
}

/** Spawns the attach fixture and waits until it is ready to be attached to. */
async function attachTarget() {
    const target = childProcess.spawn(fixture('attach'), [], { stdio: ['pipe', 'pipe', 'inherit'] });
    const exit = new Promise((resolve) => target.once('exit', resolve));
    await new Promise((resolve, reject) => {
        target.stdout.once('data', (data) =>
            (String(data).startsWith('READY') ? resolve() : reject(new Error(`readiness ${data}`))));
        target.once('error', reject);
    });
    return { target, exit };
}

async function attachToAProcess() {
    const { target, exit } = await attachTarget();
    try {
        const process = event('process');
        assert.ok(await vscode.debug.startDebugging(undefined, {
            type: 'uscope', request: 'attach', name: 'vscode-attach', pid: String(target.pid),
        }));
        const { session } = await process;
        // A breakpoint added while the process runs is placed at once.
        const breakpoint = new vscode.FunctionBreakpoint('attach_breakpoint');
        const placed = answer('setFunctionBreakpoints', (arguments_) => arguments_.breakpoints.length === 1);
        vscode.debug.addBreakpoints([breakpoint]);
        assert.strictEqual((await placed).body.breakpoints[0].verified, true);
        // Restarting detaches, which leaves the process running, and attaches
        // to it again, keeping the breakpoints.
        const detached = answer('disconnect', (arguments_) => arguments_.restart === true);
        const again = event('process', (body) => body.systemProcessId === target.pid);
        await vscode.commands.executeCommand('workbench.action.debug.restart');
        assert.ok((await detached).success);
        await again;
        const stopped = event('stopped', (body) => body.reason === 'function breakpoint');
        target.stdin.write('x');
        const { message: sent } = await stopped;
        assert.strictEqual((await topFrame(session, sent.body.threadId)).name, 'attach_breakpoint');
        await remove(breakpoint);
        // Disconnecting leaves the process to finish on its own.
        const ended = terminated();
        await vscode.debug.stopDebugging(session);
        await ended;
        assert.strictEqual(await within('the process to exit', exit), 23);
    } finally {
        target.kill();
    }
}

/**
 * ${command:pickProcess} lists the user's processes, or with a program, only
 * those running it, which leaves the target as the one item to accept.
 */
async function pickAProcess() {
    const { target, exit } = await attachTarget();
    try {
        const { processes } = extension();
        const listed = processes();
        assert.deepStrictEqual(listed.filter((entry) => entry.pid === target.pid).map(
            ({ name, commandLine, executable }) => ({ name, commandLine, executable }),
        ), [{ name: 'attach', commandLine: fixture('attach'), executable: fixture('attach') }]);
        assert.ok(!listed.some((entry) => entry.pid === process.pid), 'the extension host is not listed');

        const attached = event('process');
        const started = vscode.debug.startDebugging(undefined, {
            type: 'uscope', request: 'attach', name: 'vscode-pick', program: fixture('attach'),
            pid: '${command:pickProcess}',
        });
        let settled = false;
        started.then(() => { settled = true; }, () => { settled = true; });
        // Accept the only process the picker lists once it opens.
        await until('the picker to be accepted', () => {
            if (!settled) {
                vscode.commands.executeCommand('workbench.action.acceptSelectedQuickOpenItem');
            }
            return settled;
        });
        assert.ok(await started);
        const { session, message: sent } = await attached;
        assert.strictEqual(sent.body.systemProcessId, target.pid);
        const ended = terminated();
        await vscode.debug.stopDebugging(session);
        await ended;
    } finally {
        target.kill();
        await exit;
    }
}

async function openACoreDump() {
    const stopped = event('stopped', (body) => body.reason === 'exception');
    const capabilities = event('capabilities');
    assert.ok(await vscode.debug.startDebugging(undefined, {
        type: 'uscope', request: 'attach', name: 'vscode-core', coreFile: fixture('crash-gcc-o0-segv.core'),
        program: fixture('crash-gcc-o0'),
    }));
    const { session, message: sent } = await stopped;
    assert.match(sent.body.text, /SIGSEGV/);
    assert.strictEqual((await capabilities).message.body.capabilities.supportsRestartRequest, false);
    const trace = await session.customRequest('stackTrace', { threadId: sent.body.threadId });
    assert.ok(trace.stackFrames.some((frame) => frame.name === 'main'), JSON.stringify(trace));
    await focus();
    // A core dump has nothing to run again, so VS Code restarts by opening
    // it again with a new adapter, in the same session.
    const disconnected = request('disconnect', (arguments_) => arguments_.restart === true);
    const reopened = event('stopped', (body) => body.reason === 'exception');
    await vscode.commands.executeCommand('workbench.action.debug.restart');
    await disconnected;
    const { session: again } = await reopened;
    // The new adapter numbers its frames from the start again.
    focused = undefined;
    await focus();
    const ended = terminated();
    await vscode.debug.stopDebugging(again);
    await ended;
}

/**
 * Configurations that cannot start say why without starting anything, and
 * a folder's programs are offered to launch.
 */
async function refuseBadConfigurationsAndOfferPrograms() {
    for (const [name, configuration, reason] of [
        ['vscode-no-program', { request: 'launch', program: fixture('no-such-program') }, /failed to load .*no-such-program/],
        ['vscode-bad-arguments', { request: 'launch', program: fixture('basic'), args: 'one' }, /invalid launch configuration at args/],
        ['vscode-no-process', { request: 'attach', pid: 999999999 }, /process 999999999/],
        ['vscode-no-core', { request: 'attach', coreFile: fixture('no-such.core') }, /no-such\.core/],
    ]) {
        const refused = response(configuration.request, (reply) => !reply.success);
        const disconnected = request('disconnect');
        const started = vscode.debug.startDebugging(undefined, { type: 'uscope', name, ...configuration });
        assert.match((await refused).message.message, reason, name);
        // VS Code shows the refusal in a dialog, which its test mode refuses
        // to open, and ends the session it never started.
        await assert.rejects(within('the refusal to be shown', started), (error) =>
            /refused to show dialog/.test(error.message) && reason.test(error.message));
        await disconnected;
        assert.strictEqual(vscode.debug.activeDebugSession, undefined, name);
    }

    const folder = workspace().uri.fsPath;
    const build = path.join(folder, 'build');
    fs.mkdirSync(path.join(build, 'deps'), { recursive: true });
    try {
        fs.copyFileSync(fixture('basic'), path.join(build, 'basic'));
        fs.chmodSync(path.join(build, 'basic'), 0o755);
        // Neither a library, a dependency's build, nor a script is a program.
        fs.copyFileSync(fixture('libglobals.so'), path.join(build, 'libglobals.so'));
        fs.chmodSync(path.join(build, 'libglobals.so'), 0o755);
        fs.copyFileSync(fixture('basic'), path.join(build, 'deps', 'basic-test'));
        fs.chmodSync(path.join(build, 'deps', 'basic-test'), 0o755);
        fs.writeFileSync(path.join(build, 'script'), '#!/bin/sh\n', { mode: 0o755 });
        const offered = extension().provideDebugConfigurations(workspace());
        assert.deepStrictEqual(offered.map((configuration) => configuration.program), ['${workspaceFolder}/build/basic']);
        const exited = event('exited');
        const ended = terminated(offered[0].name);
        assert.ok(await vscode.debug.startDebugging(workspace(), offered[0]));
        assert.strictEqual((await exited).message.body.exitCode, 0);
        await ended;
    } finally {
        fs.rmSync(build, { recursive: true, force: true });
    }
}

exports.run = async function run() {
    try {
        for (const scenario of [
            createALaunchJson, findTheAdapterFromTheSetting, logTheProtocolToAFile, launchStepInspectAndRestart,
            stepIntoByInstructionAndRunToCursor, conditionsHitCountsLogpointsAndRefusals,
            consoleWatchHoverHexadecimalAndInlineValues, completionsAndLocations, dataBreakpointsAndTheirModes,
            crashAtASignal,
            pauseAndThreads, librariesAndTheirSources, runInTheIntegratedTerminal, attachToAProcess,
            pickAProcess, openACoreDump, refuseBadConfigurationsAndOfferPrograms,
        ]) {
            console.log(`uat: ${scenario.name}`);
            await scenario();
            console.log(`uat: ${scenario.name} passed`);
        }
    } finally {
        save();
    }
};
