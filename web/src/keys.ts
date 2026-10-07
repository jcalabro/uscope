// The keyboard map: VS Code's debugger keys, which a page can claim (F5
// does not reload, F11 does not go fullscreen), and letter keys that work
// whenever no text box has focus.

import type { ActionName } from "./actions";

export type Command = ActionName | "palette" | "help";

export interface Binding {
  key: string;
  shift?: boolean;
  ctrl?: boolean;
  alt?: boolean;
  /** Letter keys type text, so they act only outside text boxes. */
  typing: boolean;
  command: Command;
}

export const BINDINGS: readonly Binding[] = [
  { key: "F5", typing: false, command: "continue" },
  { key: "F6", typing: false, command: "pause" },
  { key: "F5", shift: true, typing: false, command: "kill" },
  { key: "F5", ctrl: true, shift: true, typing: false, command: "restart" },
  { key: "c", typing: true, command: "continue" },
  { key: "?", shift: true, typing: true, command: "help" },
];

export interface KeyLike {
  key: string;
  shiftKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  metaKey: boolean;
}

/** The command a key press means, given whether a text box has focus. */
export function commandFor(event: KeyLike, inTextBox: boolean): Command | null {
  for (const binding of BINDINGS) {
    if (
      binding.key === event.key &&
      (binding.shift ?? false) === event.shiftKey &&
      (binding.ctrl ?? false) === event.ctrlKey &&
      (binding.alt ?? false) === event.altKey &&
      !event.metaKey &&
      !(binding.typing && inTextBox)
    ) {
      return binding.command;
    }
  }
  return null;
}

/** Whether keys typed now go into text. */
export function isTextBox(element: Element | null): boolean {
  if (!element) {
    return false;
  }
  if (element instanceof HTMLTextAreaElement || element instanceof HTMLSelectElement) {
    return true;
  }
  if (element instanceof HTMLInputElement) {
    return !["checkbox", "radio", "button", "submit"].includes(element.type);
  }
  return element instanceof HTMLElement && element.isContentEditable;
}

/** How a binding is written in tooltips and help, such as `⇧F5`. */
export function describe(command: Command): string | null {
  const binding = BINDINGS.find((candidate) => candidate.command === command && !candidate.typing);
  if (!binding) {
    return null;
  }
  return `${binding.ctrl ? "Ctrl+" : ""}${binding.alt ? "Alt+" : ""}${binding.shift ? "⇧" : ""}${binding.key}`;
}
