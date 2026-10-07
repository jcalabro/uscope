// Where a tab goes when the session moves on (D3). A tab that showed the
// latest stop follows the program to the next one, at the thread that
// stopped it. A tab pinned with `p`, or opened on an earlier stop, stays
// where it is and says the stop has passed.

import type { At } from "./focus";
import type { State } from "./protocol";

export type Move = { to: "stop"; at: At } | { to: "session" } | null;

/** The newest stop the session has had, if any. */
export function latestStop(state: State): number | undefined {
  return state.stops.at(-1)?.stop;
}

/**
 * Where to go now. `followed` is the latest stop as of the state before
 * this one, which a tab that was following was showing.
 */
export function follow(
  state: State,
  at: At | null,
  followed: number | undefined,
  pinned: boolean,
): Move {
  const inferior = state.inferior;
  const following = at === null || (!pinned && at.stop === followed);
  if (inferior.state === "stopped") {
    if (at?.stop === inferior.stop || !following) {
      return null;
    }
    return { to: "stop", at: { stop: inferior.stop, thread: inferior.thread, frame: 0 } };
  }
  if (inferior.state === "running" || at === null || !following) {
    return null;
  }
  // The program ended: its last stop is gone with it.
  return { to: "session" };
}
