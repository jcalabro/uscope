// Syntax for the languages uscope debugs, chosen by file extension.

import { cpp } from "@codemirror/lang-cpp";
import { go } from "@codemirror/lang-go";
import { rust } from "@codemirror/lang-rust";
import { HighlightStyle, StreamLanguage, syntaxHighlighting } from "@codemirror/language";
import type { Extension } from "@codemirror/state";
import { tags } from "@lezer/highlight";

const ZIG_KEYWORDS = new Set(
  (
    "addrspace align allowzero and anyframe anytype asm async await break callconv catch comptime " +
    "const continue defer else enum errdefer error export extern fn for if inline linksection " +
    "noalias noinline nosuspend opaque or orelse packed pub resume return struct suspend switch " +
    "test threadlocal try union unreachable usingnamespace var volatile while"
  ).split(" "),
);
const ZIG_TYPES =
  /^(?:[iu]\d+|f16|f32|f64|f80|f128|usize|isize|bool|void|noreturn|type|anyerror|comptime_int|comptime_float|c_\w+)$/;

/** A small Zig tokenizer: keywords, types, strings, numbers, comments. */
const zig = StreamLanguage.define<{ inString: false }>({
  name: "zig",
  startState: () => ({ inString: false }),
  token(stream) {
    if (stream.eatSpace()) {
      return null;
    }
    if (stream.match("//")) {
      stream.skipToEnd();
      return "comment";
    }
    if (stream.match("\\\\")) {
      stream.skipToEnd();
      return "string";
    }
    const quote = stream.peek();
    if (quote === '"' || quote === "'") {
      stream.next();
      let escaped = false;
      for (let char = stream.next(); char !== undefined; char = stream.next()) {
        if (char === quote && !escaped) {
          break;
        }
        escaped = !escaped && char === "\\";
      }
      return "string";
    }
    if (
      stream.match(/^0x[0-9a-fA-F_]+|^0b[01_]+|^0o[0-7_]+|^\d[\d_]*(\.\d[\d_]*)?([eE][-+]?\d+)?/)
    ) {
      return "number";
    }
    if (stream.match(/^@\w+/)) {
      return "builtin";
    }
    const word = stream.match(/^[A-Za-z_]\w*/);
    if (word) {
      const text = stream.current();
      if (ZIG_KEYWORDS.has(text)) {
        return "keyword";
      }
      if (ZIG_TYPES.test(text)) {
        return "typeName";
      }
      if (text === "true" || text === "false" || text === "null" || text === "undefined") {
        return "atom";
      }
      return stream.peek() === "(" ? "variableName.function" : "variableName";
    }
    stream.next();
    return null;
  },
  tokenTable: {
    "variableName.function": tags.function(tags.variableName),
    builtin: tags.standard(tags.name),
  },
});

export function language(path: string): Extension {
  const extension = path.slice(path.lastIndexOf(".") + 1).toLowerCase();
  switch (extension) {
    case "c":
    case "h":
    case "cc":
    case "cpp":
    case "cxx":
    case "hh":
    case "hpp":
    case "hxx":
    case "inl":
      return cpp();
    case "rs":
      return rust();
    case "go":
      return go();
    case "zig":
      return zig;
    default:
      return [];
  }
}

/** Token colors, as the page's tokens define them in both themes. */
export const highlighting = syntaxHighlighting(
  HighlightStyle.define([
    {
      tag: [tags.keyword, tags.controlKeyword, tags.modifier, tags.operatorKeyword],
      class: "tk-k",
    },
    { tag: [tags.typeName, tags.className, tags.standard(tags.typeName)], class: "tk-t" },
    {
      tag: [tags.function(tags.variableName), tags.function(tags.propertyName), tags.macroName],
      class: "tk-f",
    },
    { tag: [tags.string, tags.character, tags.special(tags.string)], class: "tk-s" },
    { tag: [tags.number, tags.bool, tags.atom, tags.null], class: "tk-n" },
    { tag: [tags.comment, tags.lineComment, tags.blockComment], class: "tk-c" },
    { tag: [tags.processingInstruction, tags.standard(tags.name)], class: "tk-p" },
  ]),
);
