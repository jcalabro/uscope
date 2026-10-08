import type { Frame } from "../protocol";
import { useGo } from "./navigation";
import { fileName } from "./paths";
import { useFocus } from "./Workspace";

/** The call stack of the thread shown; choosing a frame moves every pane. */
export function Stack() {
  const { state, at, trace, stale } = useFocus();
  const go = useGo();
  const thread = state.threads.find((candidate) => candidate.id === at?.thread);

  let body: React.ReactNode;
  if (!at) {
    body = <div className="empty">The stack appears when the program stops.</div>;
  } else if (!trace) {
    body = (
      <div className="empty">
        {stale === "passed"
          ? `Stop #${at.stop} has passed; its stack is gone with it.`
          : "Reading the stack…"}
      </div>
    );
  } else {
    body = (
      <ul className="rows" data-testid="stack">
        {trace.frames.map((frame) => (
          <li key={frame.index}>
            <button
              type="button"
              className={`row button-row ${frame.index === at.frame ? "selected" : ""}`}
              aria-current={frame.index === at.frame ? "true" : undefined}
              title={frame.address ? `${frame.name} at ${frame.address}` : frame.name}
              onClick={() => go({ ...at, frame: frame.index })}
            >
              <span className="dim index">{frame.index}</span>
              <span className={frame.source ? "fn" : "fn subtle"}>
                {frame.name}
                {frame.kind === "inline" && <span className="dim"> inlined</span>}
                {frame.kind === "tailCall" && <span className="dim"> tail call</span>}
                {frame.kind === "async" && <span className="dim"> async</span>}
              </span>
              <span className="end">{where(frame)}</span>
            </button>
          </li>
        ))}
        {trace.incomplete && (
          <li className="row dim" title={trace.incomplete}>
            the stack ends here: {trace.incomplete}
          </li>
        )}
      </ul>
    );
  }
  return (
    <section className="pane grow" aria-label="Call stack" data-pane="2" tabIndex={-1}>
      <div className="pane-head">
        Call stack
        {thread && <span className="count">{thread.name ?? thread.id}</span>}
        <span className="end">Alt+2</span>
      </div>
      <div className={`pane-body ${stale && stale !== "passed" ? "stale" : ""}`}>{body}</div>
    </section>
  );
}

function where(frame: Frame): string {
  if (frame.source) {
    return `${fileName(frame.source.path)}:${frame.source.line}`;
  }
  return frame.module ?? frame.address ?? "";
}
