// Links name a focus exactly: what goes into the address bar comes back out.

import { describe, expect, it } from "vitest";
import {
  followedLook,
  isPagePath,
  type Look,
  linkPath,
  parseAt,
  parsePlace,
  parseSearch,
  recordedPath,
  stopPath,
  stringifySearch,
  validateLook,
} from "../src/focus";

const roundTrip = (look: Look) => validateLook(parseSearch(stringifySearch({ ...look })));

describe("a link's query", () => {
  it("round-trips watches holding any character, in order", () => {
    const look: Look = {
      src: "src/server.c:46-49",
      w: ["c->server->stats.inserts", "f(a, b)", "x & 0xff == 3", 's["k=v"]'],
      x: ["args/req", "locals/e/next"],
    };
    expect(roundTrip(look)).toEqual(look);
  });

  it("reads like the path it names", () => {
    expect(stringifySearch({ src: "src/server.c:49", w: ["a"] })).toBe("?src=src/server.c:49&w=a");
  });

  it("drops what is malformed instead of guessing", () => {
    expect(
      validateLook(parseSearch("src=nofile&asm=main&mem=0x10&view=hex&w=&x=args/a&src=b.c:2")),
    ).toEqual({ mem: "0x10", x: ["args/a"] });
    // Memory may say how many bytes the value it shows occupies.
    expect(validateLook(parseSearch("mem=0x10:16&view=memory"))).toEqual({
      mem: "0x10:16",
      view: "memory",
    });
    expect(validateLook(parseSearch("mem=0x10:x"))).toEqual({});
    expect(validateLook(parseSearch("mem=0x10:0"))).toEqual({});
    expect(validateLook(parseSearch("view=source&src=a.c:0"))).toEqual({});
  });
});

describe("places and paths", () => {
  it("parses a line or a range in a path that holds colons", () => {
    expect(parsePlace("C:/a:b.c:7")).toEqual({ path: "C:/a:b.c", line: 7, end: 7 });
    expect(parsePlace("a.c:7-9")).toEqual({ path: "a.c", line: 7, end: 9 });
    expect(parsePlace("a.c:9-7")).toBeNull();
  });

  it("shortens paths inside uscope's directory and restores them exactly", () => {
    const cwd = "/home/me/kv";
    const files = [
      "/home/me/kv/src/server.c",
      "/home/me/kv/lib/a.c",
      "/home/me/kvstore/a.c",
      "/usr/include/stdio.h",
      // Compilers can record a path relative to where they ran.
      "lib/a.c",
      "gen/table.c",
    ];
    for (const path of files) {
      expect(recordedPath(linkPath(path, cwd, files), cwd, files), path).toBe(path);
    }
    expect(linkPath("/home/me/kv/src/server.c", cwd, files)).toBe("src/server.c");
    expect(linkPath("/home/me/kvstore/a.c", cwd, files)).toBe("/home/me/kvstore/a.c");
    // A short form that names another recorded file stays long.
    expect(linkPath("/home/me/kv/lib/a.c", cwd, files)).toBe("/home/me/kv/lib/a.c");
    expect(linkPath("gen/table.c", cwd, files)).toBe("gen/table.c");
    // Until the files are known, links stay whole and relative ones name nothing.
    expect(linkPath("/home/me/kv/src/server.c", cwd, undefined)).toBe("/home/me/kv/src/server.c");
    expect(recordedPath("src/server.c", cwd, undefined)).toBeNull();
    expect(recordedPath("/usr/include/stdio.h", cwd, undefined)).toBe("/usr/include/stdio.h");
  });

  it("reads a stop's numbers, refusing anything else", () => {
    expect(parseAt({ stop: "12", thread: "41872", frame: "1" })).toEqual({
      stop: 12,
      thread: 41872,
      frame: 1,
    });
    expect(parseAt({ stop: "12", thread: "x", frame: "0" })).toBeNull();
  });

  it("names a task by its runtime and number, and reads it back", () => {
    const at = { stop: 7, thread: 0, task: { runtime: 2, number: 31 }, frame: 3 };
    expect(stopPath("k7q2", at)).toBe("/s/k7q2/stop/7/task/2.31/f/3");
    expect(parseAt({ stop: "7", task: "2.31", frame: "3" })).toEqual(at);
    for (const task of ["2", "2.x", "2.31.4", "", ".31"]) {
      expect(parseAt({ stop: "7", task, frame: "0" }), task).toBeNull();
    }
  });

  it("follows a new stop keeping the view, watches, and expansion, not the place", () => {
    expect(
      followedLook({ src: "a.c:3", asm: "0x10", view: "disassembly", w: ["a"], x: ["args/b"] }),
    ).toEqual({ view: "disassembly", w: ["a"], x: ["args/b"] });
    expect(followedLook({ mem: "0x10", view: "memory" })).toEqual({ mem: "0x10", view: "memory" });
  });
});

it("only paths on this page are followed from someone else's link", () => {
  for (const path of ["/", "/s/ab/stop/3/t/9/f/0?w=a%26b", "/pick"]) {
    expect(isPagePath(path), path).toBe(true);
  }
  for (const path of [undefined, "", "//host/", "/\\host/", "https://host/", "s/ab", "/a\nb"]) {
    expect(isPagePath(path), String(path)).toBe(false);
  }
});
