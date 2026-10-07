// The console; the program's output, what logpoints wrote, and the
// program's input; how signals are handled; and the loaded modules.

import { useEffect, useRef, useState } from "react";
import { controls } from "../model";
import { isString, read, write } from "../storage";
import { useConnection, useModel } from "../store";
import { Console } from "./Console";
import { Modules, Signals } from "./Signals";
import { useSplit } from "./Split";

type Tab = "console" | "output" | "signals" | "modules";

const TABS: readonly { key: Tab; label: string }[] = [
  { key: "console", label: "Console" },
  { key: "output", label: "Output" },
  { key: "signals", label: "Signals" },
  { key: "modules", label: "Modules" },
];

export function BottomPanel() {
  const split = useSplit("uscope-split-bottom", 220, {
    min: 90,
    max: 640,
    axis: "rows",
    reversed: true,
  });
  const [shown, setShown] = useState<Tab>(() =>
    read(
      "uscope-bottom-tab",
      "output",
      (value): value is Tab => isString(value) && TABS.some((tab) => tab.key === value),
    ),
  );
  const choose = (tab: Tab) => {
    setShown(tab);
    write("uscope-bottom-tab", tab);
  };
  return (
    <>
      <div {...split.handle} />
      <section
        className="pane bottom"
        style={{ height: split.size }}
        aria-label="Console and output"
      >
        <div className="pane-head tabs-head" role="tablist">
          {TABS.map((tab) => (
            <button
              key={tab.key}
              type="button"
              role="tab"
              aria-selected={shown === tab.key}
              className={`tab-label ${shown === tab.key ? "on" : ""}`}
              onClick={() => choose(tab.key)}
            >
              {tab.label}
              {tab.key === "output" && <OutputCount />}
            </button>
          ))}
          <span className="end">Alt+6</span>
        </div>
        {shown === "console" && <Console />}
        {shown === "output" && (
          <>
            <Output />
            <Input />
          </>
        )}
        {shown === "signals" && <Signals />}
        {shown === "modules" && <Modules />}
      </section>
    </>
  );
}

function OutputCount() {
  const bytes = useModel((model) => model.outputBytes);
  return bytes > 0 ? <span className="count"> {bytes.toLocaleString()} bytes</span> : null;
}

function Output() {
  const output = useModel((model) => model.output);
  const body = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);

  // Follow new output unless the person scrolled up to read.
  useEffect(() => {
    const element = body.current;
    if (element && pinned.current && output.length > 0) {
      element.scrollTop = element.scrollHeight;
    }
  }, [output]);

  return (
    <div
      className="pane-body"
      ref={body}
      data-pane="6"
      tabIndex={-1}
      onScroll={(event) => {
        const element = event.currentTarget;
        pinned.current = element.scrollHeight - element.scrollTop - element.clientHeight < 24;
      }}
    >
      {output.length === 0 ? (
        <div className="empty">The program's output appears here.</div>
      ) : (
        <pre className="output" data-testid="output">
          {output.map((piece) => (
            <span key={piece.seq} className={piece.stream}>
              {piece.text}
            </span>
          ))}
        </pre>
      )}
    </div>
  );
}

/** A line typed here is the program's next line of input. */
function Input() {
  const control = useModel(controls);
  const running = useModel(
    (model) =>
      model.state?.inferior.state === "running" || model.state?.inferior.state === "stopped",
  );
  const connection = useConnection();
  const [text, setText] = useState("");
  const [error, setError] = useState<string | null>(null);
  if (!control || !running) {
    return null;
  }
  const send = (eof: boolean) => {
    setError(null);
    connection
      .request("input", { text: eof ? text : `${text}\n`, eof })
      .then(() => setText(""))
      .catch((failure: Error) => setError(failure.message));
  };
  return (
    <form
      className="program-input"
      onSubmit={(event) => {
        event.preventDefault();
        send(false);
      }}
    >
      <span className="prompt" aria-hidden="true">
        ›
      </span>
      <input
        className="input quiet"
        aria-label="Program input"
        placeholder="input for the program; Enter sends a line, Ctrl+D ends its input"
        value={text}
        onChange={(event) => setText(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "d" && event.ctrlKey) {
            event.preventDefault();
            send(true);
          }
        }}
      />
      {error && (
        <span className="error" title={error}>
          {error}
        </span>
      )}
    </form>
  );
}
