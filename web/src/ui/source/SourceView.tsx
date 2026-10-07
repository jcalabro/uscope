// One source file, read-only, in CodeMirror: the breakpoint gutter, the
// line the program stopped at, the line a caller's frame is at, and the
// lines the link selects. Its keys stay the debugger's: a letter key typed
// here steps or breaks rather than moving a cursor through text.

import {
  Compartment,
  EditorState,
  type Extension,
  RangeSet,
  StateEffect,
  StateField,
} from "@codemirror/state";
import {
  Decoration,
  type DecorationSet,
  EditorView,
  GutterMarker,
  gutter,
  lineNumbers,
  ViewPlugin,
  type ViewUpdate,
  WidgetType,
} from "@codemirror/view";
import { useEffect, useRef } from "react";
import { highlighting, language } from "./languages";

export type MarkKind = "plain" | "conditional" | "log" | "pending";

/** A value shown at the end of a line, such as `e = 0x5555` . */
export interface InlineValue {
  text: string;
  changed: boolean;
}

export interface Marks {
  /** Breakpoints by line. */
  breakpoints: ReadonlyMap<number, MarkKind>;
  /** Lines a breakpoint can stop at. */
  breakable: ReadonlySet<number>;
  /** Where the shown thread stopped, when this file has it. */
  pc: number | null;
  /** Where the shown outer frame is, when this file has it. */
  frame: number | null;
  /** The lines the link selects. */
  selection: { line: number; end: number } | null;
  /** Values beside the lines that compute them. */
  inline: ReadonlyMap<number, readonly InlineValue[]>;
}

export interface SourceViewProps {
  path: string;
  text: string;
  marks: Marks;
  /** Scroll so this line shows; changes of it scroll again. */
  reveal: number | null;
  onGutter(line: number): void;
  onLineNumber(line: number, extend: boolean): void;
  onCursor(line: number): void;
}

const setMarks = StateEffect.define<Marks>();

const EMPTY: Marks = {
  breakpoints: new Map(),
  breakable: new Set(),
  pc: null,
  frame: null,
  selection: null,
  inline: new Map(),
};

const marksField = StateField.define<Marks>({
  create: () => EMPTY,
  update(marks, transaction) {
    for (const effect of transaction.effects) {
      if (effect.is(setMarks)) {
        return effect.value;
      }
    }
    return marks;
  },
});

class BreakpointMarker extends GutterMarker {
  constructor(readonly kind: MarkKind) {
    super();
  }
  override eq(other: GutterMarker): boolean {
    return other instanceof BreakpointMarker && other.kind === this.kind;
  }
  override toDOM(): Node {
    const dot = document.createElement("span");
    dot.className = `dot ${this.kind}`;
    return dot;
  }
}

class BreakableMarker extends GutterMarker {
  override toDOM(): Node {
    const dot = document.createElement("span");
    dot.className = "dot hint";
    return dot;
  }
}

const breakable = new BreakableMarker();
const markers = new Map<MarkKind, BreakpointMarker>(
  (["plain", "conditional", "log", "pending"] as const).map((kind) => [
    kind,
    new BreakpointMarker(kind),
  ]),
);

class Values extends WidgetType {
  constructor(readonly values: readonly InlineValue[]) {
    super();
  }
  override eq(other: WidgetType): boolean {
    return (
      other instanceof Values &&
      other.values.length === this.values.length &&
      other.values.every(
        (value, index) =>
          value.text === this.values[index]?.text && value.changed === this.values[index]?.changed,
      )
    );
  }
  override toDOM(): HTMLElement {
    const span = document.createElement("span");
    span.className = "cm-inline-values";
    for (const value of this.values) {
      const part = document.createElement("span");
      part.className = value.changed ? "changed" : "";
      part.textContent = value.text;
      span.append(part);
    }
    return span;
  }
}

function lineDecorations(state: EditorState): DecorationSet {
  const marks = state.field(marksField);
  const doc = state.doc;
  const ranges = [];
  const lines = (from: number, to: number, className: string) => {
    for (let line = Math.max(1, from); line <= Math.min(to, doc.lines); line += 1) {
      ranges.push(Decoration.line({ class: className }).range(doc.line(line).from));
    }
  };
  if (marks.selection) {
    lines(marks.selection.line, marks.selection.end, "cm-selected-line");
  }
  if (marks.frame !== null) {
    lines(marks.frame, marks.frame, "cm-frame-line");
  }
  if (marks.pc !== null) {
    lines(marks.pc, marks.pc, "cm-pc-line");
  }
  for (const [line, values] of marks.inline) {
    if (line >= 1 && line <= doc.lines && values.length > 0) {
      ranges.push(
        Decoration.widget({ widget: new Values(values), side: 1 }).range(doc.line(line).to),
      );
    }
  }
  return RangeSet.of(ranges, true);
}

const decorations = EditorView.decorations.compute([marksField, "doc"], lineDecorations);

/** Reports the cursor's line as it moves. */
function cursorReporter(report: (line: number) => void): Extension {
  return ViewPlugin.fromClass(
    class {
      update(update: ViewUpdate) {
        if (update.selectionSet) {
          report(update.state.doc.lineAt(update.state.selection.main.head).number);
        }
      }
    },
  );
}

export function SourceView({
  path,
  text,
  marks,
  reveal,
  onGutter,
  onLineNumber,
  onCursor,
}: SourceViewProps) {
  const parent = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  const languageSlot = useRef(new Compartment());
  // Handlers change every render; the editor calls through these.
  const handlers = useRef({ onGutter, onLineNumber, onCursor });
  handlers.current = { onGutter, onLineNumber, onCursor };

  useEffect(() => {
    if (!parent.current) {
      return;
    }
    const editor = new EditorView({
      parent: parent.current,
      state: EditorState.create({
        doc: "",
        extensions: [
          EditorState.readOnly.of(true),
          marksField,
          decorations,
          gutter({
            class: "cm-breakpoints",
            markers: (current) => {
              const marks = current.state.field(marksField);
              const doc = current.state.doc;
              const lines = [...new Set([...marks.breakpoints.keys(), ...marks.breakable])]
                .filter((line) => line >= 1 && line <= doc.lines)
                .sort((left, right) => left - right);
              return RangeSet.of(
                lines.map((line) => {
                  const kind = marks.breakpoints.get(line);
                  const marker = kind ? (markers.get(kind) as GutterMarker) : breakable;
                  return marker.range(doc.line(line).from);
                }),
              );
            },
            lineMarkerChange: (update) =>
              update.transactions.some((transaction) =>
                transaction.effects.some((effect) => effect.is(setMarks)),
              ),
            domEventHandlers: {
              mousedown: (current, block) => {
                handlers.current.onGutter(current.state.doc.lineAt(block.from).number);
                return true;
              },
            },
          }),
          lineNumbers({
            domEventHandlers: {
              mousedown: (current, block, event) => {
                handlers.current.onLineNumber(
                  current.state.doc.lineAt(block.from).number,
                  (event as MouseEvent).shiftKey,
                );
                return true;
              },
            },
          }),
          highlighting,
          languageSlot.current.of([]),
          cursorReporter((line) => handlers.current.onCursor(line)),
          EditorView.contentAttributes.of({ "aria-label": "Source", "data-keys": "debugger" }),
        ],
      }),
    });
    view.current = editor;
    return () => {
      editor.destroy();
      view.current = null;
    };
  }, []);

  // A new file replaces the text and its language.
  useEffect(() => {
    const editor = view.current;
    if (!editor) {
      return;
    }
    editor.dispatch({
      changes: { from: 0, to: editor.state.doc.length, insert: text },
      effects: languageSlot.current.reconfigure(language(path)),
    });
  }, [path, text]);

  useEffect(() => {
    view.current?.dispatch({ effects: setMarks.of(marks) });
  }, [marks]);

  useEffect(() => {
    const editor = view.current;
    // The text arriving is a reason to look for the line again.
    if (!editor || !text || reveal === null || reveal < 1 || reveal > editor.state.doc.lines) {
      return;
    }
    const line = editor.state.doc.line(reveal);
    // Only a line out of sight moves the view, so stepping nearby keeps it still.
    const block = editor.lineBlockAt(line.from);
    const { top, bottom } = editor.scrollDOM.getBoundingClientRect();
    const height = bottom - top;
    const scrolled = editor.scrollDOM.scrollTop;
    if (height > 0 && block.top >= scrolled + 20 && block.bottom <= scrolled + height - 20) {
      return;
    }
    editor.dispatch({ effects: EditorView.scrollIntoView(line.from, { y: "center" }) });
  }, [reveal, text]);

  return <div className="source-view" ref={parent} data-testid="source" data-path={path} />;
}
