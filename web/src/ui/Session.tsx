import { useEffect, useRef } from "react";
import type { Inferior, State } from "../protocol";
import { useModel } from "../store";

/** The session's main screen for what phase 1 shows: threads, the stop, and output. */
export function Session() {
  const state = useModel((model) => model.state);
  const link = useModel((model) => model.link);
  if (!state?.target) {
    return null;
  }
  // Data stays on screen while the program runs or the link is down,
  // marked as the last stop's rather than blanked.
  const stale = state.inferior.state === "running" || link !== "open";
  return (
    <main className="workspace">
      <div className="column">
        <section className={`pane grow ${stale ? "stale" : ""}`} aria-label="Threads">
          <div className="pane-head">
            Threads <span className="count">{state.threads.length}</span>
          </div>
          <div className="pane-body">
            <Threads state={state} />
          </div>
        </section>
      </div>
      <div className="column">
        <section className="pane" aria-label="Program">
          <div className="pane-head">Program</div>
          <Summary state={state} />
        </section>
        <section className="pane grow" aria-label="Output">
          <div className="pane-head">
            Output <OutputCount />
          </div>
          <Output />
        </section>
      </div>
    </main>
  );
}

function Threads({ state }: { state: State }) {
  if (state.threads.length === 0) {
    const why: Record<Inferior["state"], string> = {
      notStarted: "No threads until the program runs.",
      running: "No threads yet.",
      stopped: "No threads.",
      exited: "The program exited.",
      detached: "uscope let the process go.",
    };
    return <div className="empty">{why[state.inferior.state]}</div>;
  }
  const stopping = state.inferior.state === "stopped" ? state.inferior.thread : null;
  return (
    <ul className="rows">
      {state.threads.map((thread) => (
        <li key={thread.id} className={`row ${thread.id === stopping ? "selected" : ""}`}>
          <span className="dim">{thread.id}</span>
          <span>{thread.name ?? "unnamed"}</span>
          <span className="end">
            {thread.id === stopping ? "◂ stopped here" : thread.stopped ? "stopped" : "running"}
          </span>
        </li>
      ))}
    </ul>
  );
}

function describeInferior(inferior: Inferior): string {
  switch (inferior.state) {
    case "notStarted":
      return "not started";
    case "running":
      return `running as process ${inferior.pid}`;
    case "stopped":
      return `process ${inferior.pid}, stop #${inferior.stop}: ${inferior.reason.description}`;
    case "exited":
      return inferior.description;
    case "detached":
      return `detached from process ${inferior.pid}, which runs on`;
  }
}

/** A path relative to where uscope runs, when it is inside it. */
export function shortPath(path: string, cwd: string | undefined): string {
  if (cwd && path.startsWith(`${cwd}/`)) {
    return `./${path.slice(cwd.length + 1)}`;
  }
  return path;
}

function Summary({ state }: { state: State }) {
  const hello = useModel((model) => model.hello);
  const target = state.target;
  if (!target) {
    return null;
  }
  return (
    <dl className="summary">
      <dt>Program</dt>
      <dd title={target.program}>{shortPath(target.program, hello?.cwd)}</dd>
      {target.arguments.length > 0 && (
        <>
          <dt>Arguments</dt>
          <dd>{target.arguments.join(" ")}</dd>
        </>
      )}
      <dt>{target.kind === "core" ? "Core dump" : "State"}</dt>
      <dd data-testid="inferior">{describeInferior(state.inferior)}</dd>
      <dt>Session</dt>
      <dd>{state.session}</dd>
    </dl>
  );
}

function OutputCount() {
  const bytes = useModel((model) => model.outputBytes);
  return bytes > 0 ? <span className="count">{bytes.toLocaleString()} bytes</span> : null;
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
