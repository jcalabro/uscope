// User acceptance test: VS Code itself drives `uscope dap` through the
// extension, as a user would from the editor, and every message each
// session exchanges is recorded in the adapter log's format.
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
const TIMEOUT = 15000;

// Every message of every session, and the waiters for adapter messages.
const transcripts = new Map();
let waiters = [];

vscode.debug.registerDebugAdapterTrackerFactory('uscope', {
    createDebugAdapterTracker(session) {
        const lines = [];
        transcripts.set(session.configuration.name, lines);
        return {
            onWillReceiveMessage: (message) => lines.push(`<- ${JSON.stringify(message)}`),
            onDidSendMessage: (message) => {
                lines.push(`-> ${JSON.stringify(message)}`);
                waiters = waiters.filter((waiter) => !waiter(session, message));
            },
            onError: (error) => lines.push(`!! ${error}`),
        };
    },
});

/** Waits for an adapter message the predicate accepts. */
function adapterMessage(description, predicate) {
    return new Promise((resolve, reject) => {
        const timer = setTimeout(
            () => reject(new Error(`timed out waiting for ${description}`)),
            TIMEOUT,
        );
        waiters.push((session, message) => {
            if (!predicate(session, message)) {
                return false;
            }
            clearTimeout(timer);
            resolve({ session, message });
            return true;
        });
    });
}

const event = (name, predicate = () => true) =>
    adapterMessage(`a ${name} event`, (_, message) =>
        message.type === 'event' && message.event === name && predicate(message.body));

const response = (command, predicate = () => true) =>
    adapterMessage(`a ${command} response`, (_, message) =>
        message.type === 'response' && message.command === command && predicate(message));

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
        fs.writeFileSync(path.join(record, `${name}.log`), `${lines.join('\n')}\n`);
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
 * commands act on that stop. Frame ids are never reused.
 */
function focus() {
    return new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error('timed out waiting for a focused frame')), TIMEOUT);
        const check = (item) => {
            if (!(item instanceof vscode.DebugStackFrame) || item.frameId === focused) {
                return false;
            }
            focused = item.frameId;
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

function sourceBreakpoint(file, marker) {
    const location = new vscode.Location(vscode.Uri.file(file), new vscode.Position(lineOf(file, marker), 0));
    return new vscode.SourceBreakpoint(location);
}

async function topFrame(session, threadId) {
    const trace = await session.customRequest('stackTrace', { threadId, startFrame: 0, levels: 1 });
    return trace.stackFrames[0];
}

const workspace = () => vscode.workspace.workspaceFolders[0];

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
        assert.deepStrictEqual(configurations.map(({ type, request }) => ({ type, request })), [
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

async function launchStepInspectAndRestart() {
    const breakpoint = sourceBreakpoint(source('basic.c'), 'return uscope_value;');
    vscode.debug.addBreakpoints([breakpoint]);
    const stopped = event('stopped', (body) => body.reason === 'breakpoint');
    assert.ok(await vscode.debug.startDebugging(undefined, {
        type: 'uscope', request: 'launch', name: 'vscode-launch', program: fixture('basic'), cwd: root,
    }));
    const { session, message } = await stopped;
    const threadId = message.body.threadId;
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
    const disassembled = response('disassemble', (message) => message.success && message.body.instructions.length > 0);
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

async function attachToAProcess() {
    const target = childProcess.spawn(fixture('attach'), [], { stdio: ['pipe', 'pipe', 'inherit'] });
    const exit = new Promise((resolve) => target.once('exit', resolve));
    try {
        await new Promise((resolve, reject) => {
            target.stdout.once('data', (data) =>
                (String(data).startsWith('READY') ? resolve() : reject(new Error(`readiness ${data}`))));
            target.once('error', reject);
        });
        const breakpoint = new vscode.FunctionBreakpoint('attach_breakpoint');
        vscode.debug.addBreakpoints([breakpoint]);
        const process = event('process');
        assert.ok(await vscode.debug.startDebugging(undefined, {
            type: 'uscope', request: 'attach', name: 'vscode-attach', pid: String(target.pid),
        }));
        const { session } = await process;
        const stopped = event('stopped', (body) => body.reason === 'function breakpoint');
        target.stdin.write('x');
        const { message } = await stopped;
        assert.strictEqual((await topFrame(session, message.body.threadId)).name, 'attach_breakpoint');
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
    const target = childProcess.spawn(fixture('attach'), [], { stdio: ['pipe', 'pipe', 'inherit'] });
    const exit = new Promise((resolve) => target.once('exit', resolve));
    try {
        await new Promise((resolve, reject) => {
            target.stdout.once('data', resolve);
            target.once('error', reject);
        });
        const { processes } = require(path.join(root, 'editors/vscode/extension.js'));
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
        const { session, message } = await attached;
        assert.strictEqual(message.body.systemProcessId, target.pid);
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
    assert.ok(await vscode.debug.startDebugging(undefined, {
        type: 'uscope', request: 'attach', name: 'vscode-core', coreFile: fixture('crash-gcc-o0-segv.core'),
        program: fixture('crash-gcc-o0'),
    }));
    const { session, message } = await stopped;
    assert.match(message.body.text, /SIGSEGV/);
    const trace = await session.customRequest('stackTrace', { threadId: message.body.threadId });
    assert.ok(trace.stackFrames.some((frame) => frame.name === 'main'), JSON.stringify(trace));
    const ended = terminated();
    await vscode.debug.stopDebugging(session);
    await ended;
}

exports.run = async function run() {
    try {
        for (const scenario of [
            createALaunchJson, findTheAdapterFromTheSetting, launchStepInspectAndRestart,
            runInTheIntegratedTerminal, attachToAProcess, pickAProcess, openACoreDump,
        ]) {
            console.log(`uat: ${scenario.name}`);
            await scenario();
            console.log(`uat: ${scenario.name} passed`);
        }
    } finally {
        save();
    }
};
