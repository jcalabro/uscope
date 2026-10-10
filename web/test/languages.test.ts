// The languages without a CodeMirror package of their own are tokenized by
// small stream languages: each with its own comments, strings, and case.

import { Language } from "@codemirror/language";
import { classHighlighter, highlightTree } from "@lezer/highlight";
import { describe, expect, it } from "vitest";
import { language } from "../src/ui/source/languages";

/** The highlighted tokens of `text`, as [text, class] pairs. */
function tokens(path: string, text: string): [string, string][] {
  const syntax = language(path);
  if (!(syntax instanceof Language)) {
    throw new Error(`no language for ${path}`);
  }
  const found: [string, string][] = [];
  highlightTree(syntax.parser.parse(text), classHighlighter, (from, to, classes) => {
    found.push([text.slice(from, to), classes]);
  });
  return found;
}

/** The text of the tokens of `kind`. */
function of(found: [string, string][], kind: string): string[] {
  return found.filter(([, classes]) => classes.includes(`tok-${kind}`)).map(([text]) => text);
}

describe("stream languages", () => {
  it("nest Odin's block comments and span lines with its raw strings", () => {
    const found = tokens("a.odin", "/* a /* b */ c */ x := `r\ns` // done\nproc");
    expect(of(found, "comment")).toEqual(["/* a /* b */ c */", "// done"]);
    expect(of(found, "string")).toEqual(["`r", "s`"]);
    expect(of(found, "keyword")).toEqual(["proc"]);
  });

  it("read Fortran's keywords in any case and its doubled quotes", () => {
    const found = tokens("a.f90", "INTEGER :: n = 3 ! three\nprint *, 'it''s'");
    expect(of(found, "keyword")).toEqual(["INTEGER", "print"]);
    expect(of(found, "comment")).toEqual(["! three"]);
    expect(of(found, "string")).toEqual(["'it''s'"]);
    expect(of(found, "number")).toEqual(["3"]);
  });

  it("nest D's plus comments", () => {
    const found = tokens("a.d", '/+ a /+ b +/ c +/ auto s = r"x\\";');
    expect(of(found, "comment")).toEqual(["/+ a /+ b +/ c +/"]);
    expect(of(found, "keyword")).toEqual(["auto"]);
    expect(of(found, "string")).toEqual(['r"x\\"']);
  });

  it("read Nim's comments and its triple-quoted strings across lines", () => {
    const found = tokens("a.nim", '#[ a #[ b ]# ]# let s = """one\n"two"""" # done');
    expect(of(found, "comment")).toEqual(["#[ a #[ b ]# ]#", "# done"]);
    expect(of(found, "string")).toEqual(['"""one', '"two""""']);
  });

  it("tell Ada's characters from its attributes", () => {
    const found = tokens("a.adb", 'Last := Items\'Last; -- end\nC := \'q\'; S := "a""b"; BEGIN');
    expect(of(found, "comment")).toEqual(["-- end"]);
    expect(of(found, "string")).toEqual(["'q'", '"a""b"']);
    expect(of(found, "keyword")).toEqual(["BEGIN"]);
  });
});
