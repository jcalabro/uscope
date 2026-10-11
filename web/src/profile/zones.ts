// Zones as the server sends them, in columns, and how the zones view lays
// each thread's track out: a row per depth, tracks one under another.

import type { ProfileZones } from "../protocol";

export interface ZoneBlocks {
  count: number;
  /** Nanoseconds from the profile's start. */
  start: Float64Array;
  end: Float64Array;
  thread: Uint32Array;
  depth: Uint32Array;
  /** Indices into `names`. */
  name: Uint32Array;
  /** How many zones each block draws: more than one is a fold. */
  zones: Uint32Array;
  flags: Uint32Array;
  names: string[];
}

/** Decodes an answer and the columns sent before it: starts and ends as
 * little-endian `f64`s, then threads, depths, names, counts, and flags as
 * `u32`s. */
export function decodeZones(answer: ProfileZones, payload: Uint8Array | undefined): ZoneBlocks {
  const count = answer.blocks;
  const bytes = payload ?? new Uint8Array();
  if (bytes.byteLength !== count * 36) {
    throw new Error(`${count} zone blocks need ${count * 36} bytes, not ${bytes.byteLength}`);
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const floats = (column: number) =>
    Float64Array.from({ length: count }, (_, at) =>
      view.getFloat64((column * count + at) * 8, true),
    );
  const words = (column: number) =>
    Uint32Array.from({ length: count }, (_, at) =>
      view.getUint32(count * 16 + (column * count + at) * 4, true),
    );
  return {
    count,
    start: floats(0),
    end: floats(1),
    thread: words(0),
    depth: words(1),
    name: words(2),
    zones: words(3),
    flags: words(4),
    names: answer.names,
  };
}

/** One thread's track: its first row and how many rows it takes. */
export interface Track {
  thread: number;
  row: number;
  depths: number;
}

/** Each thread's track, in `order`, then any other thread with zones,
 * each as deep as its deepest block. */
export function tracks(blocks: ZoneBlocks, order: number[]): Track[] {
  const deepest = new Map<number, number>();
  for (let at = 0; at < blocks.count; at += 1) {
    const thread = blocks.thread[at] ?? 0;
    deepest.set(thread, Math.max(deepest.get(thread) ?? 0, (blocks.depth[at] ?? 0) + 1));
  }
  const threads = [
    ...order.filter((thread) => deepest.has(thread)),
    ...[...deepest.keys()].filter((thread) => !order.includes(thread)).sort((a, b) => a - b),
  ];
  let row = 0;
  return threads.map((thread) => {
    const depths = deepest.get(thread) ?? 1;
    const track = { thread, row, depths };
    row += depths;
    return track;
  });
}

/** The block drawn at `time` in a row, if any. */
export function blockAt(
  blocks: ZoneBlocks,
  track: Track,
  depth: number,
  time: number,
  slack: number,
): number | null {
  let best: number | null = null;
  for (let at = 0; at < blocks.count; at += 1) {
    if (blocks.thread[at] !== track.thread || blocks.depth[at] !== depth) {
      continue;
    }
    const start = blocks.start[at] ?? 0;
    const end = blocks.end[at] ?? 0;
    if (start - slack <= time && time <= end + slack) {
      if (best === null || Math.abs(time - start) < Math.abs(time - (blocks.start[best] ?? 0))) {
        best = at;
      }
    }
  }
  return best;
}

/** What a zone's flags say was not seen. */
export function describeFlags(flags: number): string[] {
  const said: string[] = [];
  if (flags & 1) {
    said.push("its begin was lost");
  }
  if (flags & 2) {
    said.push("its end was not seen");
  }
  if (flags & 4) {
    said.push("it does not nest");
  }
  return said;
}
