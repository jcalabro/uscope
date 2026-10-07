// The console: expressions and uscope's commands, in the frame the tab
// shows. Tab completes from the frame's names, members, and registers, and
// the arrow keys walk the lines run before.

import { useEffect, useRef, useState } from "react";
import { useStore } from "zustand";
import { commonPrefix, consoleStore, finished, started } from "../console";
import type { Completion } from "../protocol";
import { useConnection } from "../store";
import { LocalExpansion, ValueRow } from "./ValueTree";
import { useFocus } from "./Workspace";

export function Console() {
  const entries = useStore(consoleStore, (state) => state.entries);
  const body = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const element = body.current;
    if (element && entries.length > 0) {
      element.scrollTop = element.scrollHeight;
    }
  }, [entries]);

  return (
    <>
      <div className="pane-body console" ref={body} data-testid="console">
        {entries.length === 0 ? (
          <div className="empty">
            Expressions and uscope's commands run in the frame shown. Tab completes; ↑ and ↓ recall.
          </div>
        ) : (
          <LocalExpansion>
            <ul className="console-entries">
              {entries.map((entry) => (
                <li key={entry.id} className="console-entry">
                  <div className="console-line">
                    <span className="prompt">›</span> {entry.line}
                  </div>
                  {entry.error !== undefined && <pre className="error">{entry.error}</pre>}
                  {entry.result?.output != null && <pre>{entry.result.output}</pre>}
                  {entry.result?.row && (
                    <ul className="value-list">
                      <ValueRow row={entry.result.row} parent={`c${entry.id}`} depth={0} />
                    </ul>
                  )}
                </li>
              ))}
            </ul>
          </LocalExpansion>
        )}
      </div>
      <ConsoleInput />
    </>
  );
}

function ConsoleInput() {
  const { at, stale } = useFocus();
  const connection = useConnection();
  const history = useStore(consoleStore, (state) => state.history);
  const [text, setText] = useState("");
  const [recalled, setRecalled] = useState<number | null>(null);
  const [choices, setChoices] = useState<{ start: number; items: Completion[] } | null>(null);
  const field = useRef<HTMLInputElement>(null);
  // Lines run at the frame shown, while its stop is the program's.
  const frame = at && !stale ? at : null;

  const run = () => {
    const line = text.trim();
    if (!line) {
      return;
    }
    setText("");
    setRecalled(null);
    setChoices(null);
    const id = started(line, frame?.stop ?? null);
    connection
      .request("console", frame ? { line, ...frame } : { line })
      .then((result) => finished(id, { result }))
      .catch((failure: Error) => finished(id, { error: failure.message }));
  };

  const insert = (start: number, label: string, cursor: number) => {
    const before = [...text.slice(0, cursor)];
    const replaced = before.slice(0, start).join("") + label;
    setText(replaced + text.slice(cursor));
    requestAnimationFrame(() => field.current?.setSelectionRange(replaced.length, replaced.length));
  };

  const complete = () => {
    const cursor = field.current?.selectionStart ?? text.length;
    connection
      .request(
        "complete",
        frame ? { text: text.slice(0, cursor), ...frame } : { text: text.slice(0, cursor) },
      )
      .then(({ start, items }) => {
        if (items.length === 0) {
          setChoices(null);
          return;
        }
        if (items.length === 1) {
          insert(start, (items[0] as Completion).label, cursor);
          setChoices(null);
          return;
        }
        const typed = [...text.slice(0, cursor)].slice(start).join("");
        const shared = commonPrefix(items.map((item) => item.label));
        if (shared.length > typed.length) {
          insert(start, shared, cursor);
        }
        setChoices({ start, items: items.slice(0, 60) });
      })
      .catch(() => setChoices(null));
  };

  const recall = (step: -1 | 1) => {
    if (history.length === 0) {
      return;
    }
    const index =
      recalled === null
        ? step < 0
          ? history.length - 1
          : null
        : recalled + step >= history.length
          ? null
          : Math.max(0, recalled + step);
    setRecalled(index);
    setText(index === null ? "" : (history[index] as string));
  };

  return (
    <form
      className="console-input"
      onSubmit={(event) => {
        event.preventDefault();
        run();
      }}
    >
      {choices && (
        <ul className="completions" aria-label="Completions">
          {choices.items.map((item) => (
            <li key={item.label}>
              <button
                type="button"
                onMouseDown={(event) => {
                  event.preventDefault();
                  insert(choices.start, item.label, field.current?.selectionStart ?? text.length);
                  setChoices(null);
                }}
              >
                {item.label}
                <span className="dim"> {item.kind}</span>
              </button>
            </li>
          ))}
        </ul>
      )}
      <span className="prompt" aria-hidden="true">
        ›
      </span>
      <input
        ref={field}
        className="input quiet"
        aria-label="Console"
        data-pane="6"
        placeholder={
          frame
            ? `an expression or command, in frame ${frame.frame}`
            : "a command; expressions need a stop"
        }
        value={text}
        autoComplete="off"
        spellCheck={false}
        onChange={(event) => {
          setText(event.target.value);
          setChoices(null);
        }}
        onKeyDown={(event) => {
          if (event.key === "Tab" && !event.shiftKey) {
            event.preventDefault();
            complete();
          } else if (event.key === "ArrowUp") {
            event.preventDefault();
            recall(-1);
          } else if (event.key === "ArrowDown") {
            event.preventDefault();
            recall(1);
          } else if (event.key === "Escape" && choices) {
            event.stopPropagation();
            setChoices(null);
          }
        }}
      />
    </form>
  );
}
