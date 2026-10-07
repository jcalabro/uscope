// What people type: debugger keys, program arguments, and environments.

import { describe, expect, it } from "vitest";
import { BINDINGS, commandFor, describe as describeKey, type KeyLike } from "../src/keys";
import { joinWords, parseEnvironment, splitWords } from "../src/words";

const key = (name: string, modifiers: Partial<KeyLike> = {}): KeyLike => ({
  key: name,
  shiftKey: false,
  ctrlKey: false,
  altKey: false,
  metaKey: false,
  ...modifiers,
});

describe("debugger keys", () => {
  it("maps VS Code's run keys whether or not a text box has focus", () => {
    for (const inTextBox of [false, true]) {
      expect(commandFor(key("F5"), inTextBox)).toBe("continue");
      expect(commandFor(key("F6"), inTextBox)).toBe("pause");
      expect(commandFor(key("F5", { shiftKey: true }), inTextBox)).toBe("kill");
      expect(commandFor(key("F5", { shiftKey: true, ctrlKey: true }), inTextBox)).toBe("restart");
    }
  });

  it("leaves letters to text boxes and other modifiers to the browser", () => {
    expect(commandFor(key("c"), false)).toBe("continue");
    expect(commandFor(key("c"), true)).toBeNull();
    expect(commandFor(key("c", { ctrlKey: true }), false)).toBeNull();
    expect(commandFor(key("F5", { ctrlKey: true }), false)).toBeNull();
    expect(commandFor(key("F5", { metaKey: true }), false)).toBeNull();
    expect(commandFor(key("F5", { altKey: true }), false)).toBeNull();
  });

  it("never binds one press to two commands", () => {
    const presses = BINDINGS.map((b) => `${b.key}/${b.shift}/${b.ctrl}/${b.alt}`);
    expect(new Set(presses).size).toBe(presses.length);
  });

  it("writes keys as tooltips show them", () => {
    expect(describeKey("continue")).toBe("F5");
    expect(describeKey("kill")).toBe("⇧F5");
    expect(describeKey("restart")).toBe("Ctrl+⇧F5");
    expect(describeKey("palette")).toBeNull();
  });
});

describe("program arguments", () => {
  it.each([
    ["", []],
    ["  a   b ", ["a", "b"]],
    [`'one two' "three four"`, ["one two", "three four"]],
    [`a\\ b`, ["a b"]],
    [`"say \\"hi\\" \\n"`, [`say "hi" \\n`]],
    [`'it''s'`, ["its"]],
    [`""`, [""]],
    [`--name=x"y z"`, ["--name=xy z"]],
    [`'$HOME' *.c`, ["$HOME", "*.c"]],
  ])("splits %j", (line, words) => {
    expect(splitWords(line)).toEqual({ ok: true, words });
  });

  it.each([
    [`"open`, `a " quote is not closed`],
    [`'open`, `a ' quote is not closed`],
    [`end\\`, "a backslash ends the line"],
  ])("refuses %j", (line, error) => {
    expect(splitWords(line)).toEqual({ ok: false, error });
  });

  it("joins words into a line that splits back into them", () => {
    const cases = [
      ["plain", "--flag=1", "./path/x"],
      ["two words", "", "it's", `"q"`, "a\\b", "$x"],
    ];
    for (const words of cases) {
      expect(splitWords(joinWords(words))).toEqual({ ok: true, words });
    }
    expect(joinWords(["plain", "two words"])).toBe("plain 'two words'");
  });
});

describe("environments", () => {
  it("reads NAME=VALUE lines, keeping later equals signs", () => {
    expect(parseEnvironment("A=1\n\n  B=x=y  \nEMPTY=\n")).toEqual({
      ok: true,
      pairs: [
        ["A", "1"],
        ["B", "x=y"],
        ["EMPTY", ""],
      ],
    });
  });

  it("names the line it cannot read", () => {
    expect(parseEnvironment("A=1\nnonsense")).toEqual({
      ok: false,
      error: "line 2 is not NAME=VALUE",
    });
    expect(parseEnvironment("=value")).toMatchObject({ ok: false });
  });
});
