// What each run-control action does in the current state, or why it is
// unavailable. Toolbar buttons and keys both ask here, so they always agree.
// Steps act on the tab's focus: the stop, thread, and frame it shows.

import type { Connection } from "./connection";
import type { At } from "./focus";
import type { Model } from "./model";
import { controls } from "./model";
import type { StepKind } from "./protocol";

export type ActionName =
  | "continue"
  | "pause"
  | "kill"
  | "restart"
  | "over"
  | "into"
  | "out"
  | "instruction"
  | "overInstruction";

export const STEPS: Record<string, StepKind> = {
  over: "over",
  into: "into",
  out: "out",
  instruction: "instruction",
  overInstruction: "overInstruction",
};

export interface Available {
  enabled: true;
  run(connection: Connection): Promise<unknown>;
}

export interface Unavailable {
  enabled: false;
  /** Why, as a tooltip says it. */
  reason: string;
}

export type Action = Available | Unavailable;

const unavailable = (reason: string): Unavailable => ({ enabled: false, reason });

/** `at` is where the tab looks, when it looks at a stop. */
export function action(name: ActionName, model: Model, at: At | null = null): Action {
  const state = model.state;
  if (model.link !== "open") {
    return unavailable("not connected to uscope");
  }
  if (!controls(model)) {
    return unavailable("this link can only view the session");
  }
  if (!state?.target) {
    return unavailable("nothing is being debugged");
  }
  if (state.target.kind === "core") {
    return unavailable("a core dump cannot run");
  }
  const inferior = state.inferior;
  const launched = state.target.kind === "launch";
  const kind = STEPS[name];
  if (kind) {
    if (inferior.state !== "stopped") {
      return unavailable("the program is not stopped");
    }
    // A step leaves the thread and frame shown, which must be this stop's.
    if (at && at.stop !== inferior.stop) {
      return unavailable(`this tab shows stop #${at.stop}; go to stop #${inferior.stop} to step`);
    }
    const thread = at?.thread ?? inferior.thread;
    const task = at?.task ?? null;
    const frame = kind === "out" ? (at?.frame ?? 0) : 0;
    const stop = inferior.stop;
    return {
      enabled: true,
      run: (connection) => connection.request("step", { stop, thread, task, frame, kind }),
    };
  }
  switch (name) {
    case "continue":
      if (inferior.state === "stopped") {
        const stop = inferior.stop;
        return { enabled: true, run: (connection) => connection.request("continue", { stop }) };
      }
      if (launched && (inferior.state === "notStarted" || inferior.state === "exited")) {
        return {
          enabled: true,
          run: (connection) => connection.request("continue", { stop: null }),
        };
      }
      return unavailable(inferior.state === "running" ? "already running" : "the program is gone");
    case "pause":
      return inferior.state === "running"
        ? { enabled: true, run: (connection) => connection.request("pause") }
        : unavailable("the program is not running");
    case "kill":
      return inferior.state === "running" || inferior.state === "stopped"
        ? { enabled: true, run: (connection) => connection.request("kill") }
        : unavailable("the program is not running");
    case "restart":
      return launched
        ? { enabled: true, run: (connection) => connection.request("restart") }
        : unavailable("only a launched program can restart");
    default:
      return unavailable("unknown action");
  }
}

/** The label of the continue button: starting and continuing differ. */
export function continueLabel(model: Model): string {
  const inferior = model.state?.inferior;
  return inferior && inferior.state !== "stopped" && inferior.state !== "running"
    ? "Run"
    : "Continue";
}
