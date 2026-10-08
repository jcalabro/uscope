// What a tab remembers belongs to one session, and some of it to one stop,
// one connection, or one revision of the program's values. Forgetting
// follows the model, not a page: a tab in the picker while the session
// changes forgets as surely as one watching it.

import type { StoreApi } from "zustand";
import { clearConsole } from "./console";
import { type Cache, cache } from "./data";
import { latestStop } from "./follow";
import type { Model } from "./model";
import { initialTab, tab } from "./tab";
import { type Recall, recall } from "./tree";

interface Marks {
  session: string | null;
  latest: number | undefined;
  writes: number | undefined;
  settings: number | undefined;
  connection: number | undefined;
}

function marks(model: Model): Marks {
  const state = model.state;
  return {
    session: state?.session ?? null,
    latest: state ? latestStop(state) : undefined,
    writes: state?.writes,
    settings: state?.settings,
    connection: model.hello?.connection,
  };
}

/** Forgets what `after` no longer holds of what `before` did. */
export function forget(before: Marks, after: Marks, answers: Cache, values: Recall): boolean {
  if (before.session !== after.session) {
    answers.clear();
    values.clear();
    return true;
  }
  // The files and modules a program has change as its libraries load, so
  // each stop asks again.
  if (before.latest !== after.latest) {
    answers.forget("sources ");
    answers.forget("modules ");
  }
  // What a stop holds changes when someone writes a value or memory, and
  // value handles belong to one connection.
  if (before.writes !== after.writes || before.connection !== after.connection) {
    for (const method of [
      "scopes ",
      "children ",
      "evaluate ",
      "readMemory ",
      "registers ",
      "tasks ",
    ]) {
      answers.forget(method);
    }
  }
  if (before.writes !== after.writes) {
    // Code is memory too.
    answers.forget("disassemble ");
  }
  // Signal policies change without a stop.
  if (before.settings !== after.settings) {
    answers.forget("signals ");
  }
  return false;
}

/** Forgets as `store`'s model changes, until the returned function is called. */
export function forgetting(store: StoreApi<Model>): () => void {
  let before = marks(store.getState());
  return store.subscribe((model) => {
    const after = marks(model);
    if (forget(before, after, cache, recall)) {
      // Another program's files, places, and values are not this one's.
      const { files, cursor, shown, editing, pinned, frames } = initialTab;
      tab.setState({ files, cursor, shown, editing, pinned, frames });
      clearConsole();
    }
    before = after;
  });
}
