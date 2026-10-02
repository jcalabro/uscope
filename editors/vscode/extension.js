// @ts-check
// Starts `uscope dap` for uscope debug sessions, opens a launch.json when
// there is no configuration to debug, and picks processes to attach to.

'use strict';

const fs = require('fs');
const os = require('os');
const path = require('path');
const vscode = require('vscode');

/** @param {vscode.ExtensionContext} context */
function activate(context) {
    context.subscriptions.push(
        vscode.debug.registerDebugAdapterDescriptorFactory('uscope', { createDebugAdapterDescriptor }),
        vscode.debug.registerDebugConfigurationProvider('uscope', { resolveDebugConfiguration }),
        vscode.commands.registerCommand('uscope.pickProcess', pickProcess),
    );
}

/** @param {vscode.DebugSession} session */
function createDebugAdapterDescriptor(session) {
    const folder = session.workspaceFolder;
    const setting = vscode.workspace.getConfiguration('uscope', folder).get('path', 'uscope');
    const command = findExecutable(setting, folder?.uri.fsPath);
    if (command === undefined) {
        throw new Error(`Cannot find uscope at "${setting}". Install uscope on PATH or set uscope.path.`);
    }
    return new vscode.DebugAdapterExecutable(command, ['dap']);
}

/**
 * Resolves the uscope.path setting to an executable file: a name searched
 * for on PATH, or a path that may start with ${workspaceFolder} or ${userHome}.
 * Relative paths start at the workspace folder.
 *
 * @param {string} setting
 * @param {string | undefined} workspaceFolder
 * @returns {string | undefined}
 */
function findExecutable(setting, workspaceFolder) {
    if (setting.includes('${workspaceFolder}') && workspaceFolder === undefined) {
        return undefined;
    }
    const expanded = setting
        .replaceAll('${workspaceFolder}', workspaceFolder ?? '')
        .replaceAll('${userHome}', os.homedir());
    const candidates = expanded.includes('/')
        ? [path.resolve(workspaceFolder ?? process.cwd(), expanded)]
        : (process.env.PATH ?? '').split(path.delimiter).filter(Boolean).map((directory) => path.join(directory, expanded));
    return candidates.find(isExecutableFile);
}

/** @param {string} file */
function isExecutableFile(file) {
    try {
        fs.accessSync(file, fs.constants.X_OK);
        return fs.statSync(file).isFile();
    } catch {
        return false;
    }
}

/**
 * Pressing F5 without a launch.json passes an empty configuration; returning
 * null asks VS Code to create launch.json from the initial configurations.
 *
 * @param {vscode.WorkspaceFolder | undefined} folder
 * @param {vscode.DebugConfiguration} configuration
 */
function resolveDebugConfiguration(folder, configuration) {
    if (configuration.request !== undefined) {
        return configuration;
    }
    if (folder === undefined) {
        vscode.window.showErrorMessage('Open a folder to create a launch.json for uscope.');
        return undefined;
    }
    return null;
}

/**
 * Lists the user's processes, newest first, for ${command:pickProcess}: only
 * those running the configuration's program, when it names one. Returns the
 * chosen process id, or undefined when the pick is dismissed, which cancels
 * the debug session.
 *
 * @param {vscode.DebugConfiguration | undefined} configuration
 */
async function pickProcess(configuration) {
    // VS Code has substituted the configuration's other variables already.
    const program = typeof configuration?.program === 'string' ? realPath(configuration.program) : undefined;
    const items = processes()
        .filter((entry) => program === undefined || entry.executable === program)
        .map((entry) => ({
            label: entry.name,
            description: String(entry.pid),
            detail: entry.commandLine,
            pid: entry.pid,
        }));
    const picked = await vscode.window.showQuickPick(items, {
        title: program === undefined ? 'Attach to a process' : `Attach to a process running ${program}`,
        placeHolder: 'Filter by name, process ID, or command line',
        matchOnDescription: true,
        matchOnDetail: true,
    });
    return picked === undefined ? undefined : String(picked.pid);
}

/** @param {string} file */
function realPath(file) {
    try {
        return fs.realpathSync(file);
    } catch {
        return file;
    }
}

/**
 * The processes the current user owns, except this extension host, which
 * stopping would hang, newest first.
 *
 * @returns {{ pid: number, name: string, commandLine: string, executable?: string, started: number }[]}
 */
function processes() {
    const uid = process.getuid?.();
    const found = [];
    for (const entry of fs.readdirSync('/proc')) {
        const pid = Number(entry);
        if (!Number.isInteger(pid) || pid === process.pid) {
            continue;
        }
        try {
            const directory = `/proc/${pid}`;
            if (fs.statSync(directory).uid !== uid) {
                continue;
            }
            // The fields follow the parenthesized name, which may itself contain
            // spaces and parentheses. fields[0] is the state, field 3 of stat,
            // so field 22, the start time, is fields[19].
            const stat = fs.readFileSync(`${directory}/stat`, 'utf8');
            const fields = stat.slice(stat.lastIndexOf(')') + 2).split(' ');
            // A zombie has exited and cannot be traced.
            if (fields[0] === 'Z') {
                continue;
            }
            const name = fs.readFileSync(`${directory}/comm`, 'utf8').trimEnd();
            const commandLine = fs.readFileSync(`${directory}/cmdline`, 'utf8').split('\0').filter(Boolean).join(' ');
            let executable;
            try {
                executable = fs.readlinkSync(`${directory}/exe`);
            } catch {
                // The process has no executable to read, or it just exited.
            }
            found.push({ pid, name, commandLine: commandLine || `[${name}]`, executable, started: Number(fields[19]) });
        } catch {
            // The process exited while it was being read.
        }
    }
    return found.sort((first, second) => second.started - first.started || second.pid - first.pid);
}

module.exports = { activate, findExecutable, processes };
