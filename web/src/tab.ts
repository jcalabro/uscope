// What belongs to this tab alone: where it looks, whether it is pinned to a
// stop, and the files it has open. The URL holds the focus; this store
// mirrors it for code outside the router, such as the keyboard.

import { createStore, useStore } from "zustand";
import type { At, Look } from "./focus";

export interface TabState {
  session: string | null;
  /** The stop, thread, and frame shown, when a stop is. */
  at: At | null;
  look: Look;
  /** A pinned tab stays on its stop when the program stops elsewhere. */
  pinned: boolean;
  /** The source line the keyboard acts on, such as F9's. */
  cursor: { path: string; line: number } | null;
  /** The file shown and the line the frame is at in it, when it is. */
  shown: { path: string; line: number | null } | null;
  /** How many frames the stack shown has. */
  frames: number;
  /** The breakpoint whose editor is open. */
  editing: number | null;
  /** A short message, such as that a link was copied. */
  flash: { text: string; at: number } | null;
  /** Files opened in this tab, most recent last. */
  files: string[];
  /** The palette, open at everything, at files, or at a line. */
  palette: "all" | "files" | "line" | null;
  /** Whether the key help is open. */
  help: boolean;
}

export const initialTab: TabState = {
  session: null,
  at: null,
  look: {},
  pinned: false,
  cursor: null,
  shown: null,
  frames: 0,
  editing: null,
  flash: null,
  files: [],
  palette: null,
  help: false,
};

export const tab = createStore<TabState>(() => initialTab);

export function useTab<T>(select: (state: TabState) => T): T {
  return useStore(tab, select);
}

/** The most files a tab keeps open. */
const FILES = 8;

export function openFile(path: string): void {
  tab.setState((state) => {
    if (state.files.includes(path)) {
      return state;
    }
    return { files: [...state.files, path].slice(-FILES) };
  });
}

export function closeFile(path: string): void {
  tab.setState((state) => ({ files: state.files.filter((file) => file !== path) }));
}

export function flash(text: string): void {
  tab.setState({ flash: { text, at: Date.now() } });
}
