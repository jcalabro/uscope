// A profile's view: its address round-trips, and its call tree's columns
// decode into geometry that nests, as real servers' answers do.

import { describe, expect, it } from "vitest";
import { parseSearch, stringifySearch } from "../src/focus";
import {
  type ProfileLook,
  queryOf,
  transformed,
  untransformed,
  validateProfileLook,
} from "../src/profile/look";
import { type CallTree, decodeTree, findPath, hit, layout, pathOf } from "../src/profile/tree";
import type { ProfileKey, ProfileTree, ServerMessage } from "../src/protocol";
import { binaries, transcript } from "./transcripts";

const roundTrip = (look: ProfileLook) =>
  validateProfileLook(parseSearch(stringifySearch(look as Record<string, unknown>)));

describe("a profile's address", () => {
  it("holds the whole view, names with any characters included", () => {
    const look: ProfileLook = {
      view: "functions",
      range: "1000-250000",
      th: ["0", "2"],
      src: "waits",
      tk: ["3", "none"],
      bt: "1",
      lanes: "tasks",
      tf: ["focus:<Pin<Box<F>> as Future>::poll", "drop:std::sys::backtrace::__rust_begin"],
      invert: "1",
      native: "1",
      by: "line",
      fn: "app::db::decode",
      z: ["main", "a & b", "c=d?"],
    };
    expect(roundTrip(look)).toEqual(look);
    expect(queryOf("p", look)).toEqual({
      profile: "p",
      range: [1000, 250000],
      threads: [0, 2],
      source: "waits",
      tasks: [3, null],
      byTask: true,
      transforms: [
        { op: "focus", name: "<Pin<Box<F>> as Future>::poll" },
        { op: "drop", name: "std::sys::backtrace::__rust_begin" },
      ],
      invert: true,
      native: true,
      granularity: "line",
    });
  });

  it("drops what it cannot mean", () => {
    expect(
      validateProfileLook({
        view: "flame",
        range: "9-3",
        th: ["x"],
        src: "cpu",
        tk: ["x", "-1"],
        bt: "yes",
        lanes: "threads",
        tf: ["spin:main", ":main", "merge:"],
        invert: "yes",
        by: "function",
      }),
    ).toEqual({});
    expect(queryOf("p", {})).toEqual({
      profile: "p",
      source: "samples",
      byTask: false,
      transforms: [],
      invert: false,
      native: false,
      granularity: "function",
    });
  });

  it("keeps transforms in order, and forgets the zoom they move", () => {
    const focused = transformed({ z: ["main"], fn: "leaf" }, { op: "focus", name: "outer" });
    expect(focused).toEqual({ fn: "leaf", tf: ["focus:outer"] });
    const both = transformed(focused, { op: "merge", name: "middle" });
    expect(untransformed(both, 1)).toEqual(focused);
    expect(untransformed(both, 0)).toEqual({ fn: "leaf" });
  });
});

/** The columns of a tree of `nodes`, each [name, own, total, depth]. */
function encode(nodes: [string, number, number, number][]): CallTree {
  const names = [...new Set(nodes.map(([name]) => name))];
  const keys: ProfileKey[] = names.map((name) => ({
    name,
    kind: "function",
    module: null,
    file: null,
    line: null,
  }));
  const count = nodes.length;
  const bytes = new Uint8Array(count * 24);
  const view = new DataView(bytes.buffer);
  for (const [node, [name, own, total, depth]] of nodes.entries()) {
    view.setFloat64(node * 8, own, true);
    view.setFloat64((count + node) * 8, total, true);
    view.setUint32(count * 16 + node * 4, names.indexOf(name), true);
    view.setUint32(count * 20 + node * 4, depth, true);
  }
  const weight = nodes.filter(([, , , depth]) => depth === 0).reduce((sum, [, , t]) => sum + t, 0);
  const answer: ProfileTree = { nodes: count, weight, samples: weight, keys };
  return decodeTree(answer, bytes);
}

const tree = encode([
  ["main", 0, 10, 0],
  ["work", 2, 6, 1],
  ["leaf", 4, 4, 2],
  ["idle", 4, 4, 1],
  ["thread", 0, 2, 0],
  ["leaf", 2, 2, 1],
]);

describe("a call tree's geometry", () => {
  it("finds each node's caller and subtree", () => {
    expect([...tree.parent]).toEqual([-1, 0, 1, 0, -1, 4]);
    expect([...tree.end]).toEqual([4, 3, 3, 4, 6, 6]);
    expect(pathOf(tree, 2)).toEqual(["main", "work", "leaf"]);
    expect(findPath(tree, ["thread", "leaf"])).toBe(5);
    expect(findPath(tree, ["main", "leaf"])).toBeNull();
  });

  it("nests every node inside its caller, siblings side by side", () => {
    const rects = layout(tree, null, 0);
    // In twelfths of the width, the weight of everything.
    const twelfths = (fraction: number) => Math.round(fraction * 12 * 1e9) / 1e9;
    expect(rects.map(({ node, x, width }) => [node, twelfths(x), twelfths(width)])).toEqual([
      [0, 0, 10],
      [1, 0, 6],
      [2, 0, 4],
      [3, 6, 4],
      [4, 10, 2],
      [5, 10, 2],
    ]);
    expect(hit(rects, 1, 0.7)?.node).toBe(3);
    expect(hit(rects, 2, 0.7)).toBeNull();
  });

  it("zooms a node to the whole width, under its ancestors", () => {
    const rects = layout(tree, 1, 0);
    expect(rects.map(({ node, x, width }) => [node, x, width])).toEqual([
      [0, 0, 1],
      [1, 0, 1],
      [2, 0, 4 / 6],
    ]);
  });

  it("leaves out what is too narrow to see, and what it calls", () => {
    expect(layout(tree, null, 0.2).map(({ node }) => node)).toEqual([0, 1, 2, 3]);
  });

  it("refuses columns that do not fit the tree", () => {
    const answer: ProfileTree = { nodes: 2, weight: 1, samples: 1, keys: [] };
    expect(() => decodeTree(answer, new Uint8Array(24))).toThrow(/48 bytes/);
  });
});

describe("a real server's call trees", () => {
  const lines = transcript("profile");
  const frames = binaries("profile");
  const trees = lines.flatMap((line) => {
    if (line.to !== "page" || !("message" in line)) {
      return [];
    }
    const message: ServerMessage = line.message;
    if (message.type !== "result" || !frames.has(message.id)) {
      return [];
    }
    return [decodeTree(message.result as ProfileTree, frames.get(message.id))];
  });

  it("decodes, and each node's total is its own and its callees'", () => {
    expect(trees.length).toBeGreaterThanOrEqual(3);
    for (const tree of trees) {
      let roots = 0;
      for (let node = 0; node < tree.count; node += 1) {
        let callees = 0;
        for (let child = node + 1; child < (tree.end[node] ?? 0); child = tree.end[child] ?? 0) {
          callees += tree.total[child] ?? 0;
        }
        expect(tree.total[node]).toBe((tree.own[node] ?? 0) + callees);
        roots += tree.parent[node] === -1 ? (tree.total[node] ?? 0) : 0;
      }
      expect(roots).toBe(tree.weight);
      for (const rect of layout(tree, null, 0)) {
        const parent = tree.parent[rect.node] ?? -1;
        expect(rect.x + rect.width).toBeLessThanOrEqual(1 + 1e-9);
        if (parent >= 0) {
          const outer = layout(tree, null, 0).find((each) => each.node === parent);
          expect(rect.x).toBeGreaterThanOrEqual(outer?.x ?? 0);
        }
      }
    }
  });
});
