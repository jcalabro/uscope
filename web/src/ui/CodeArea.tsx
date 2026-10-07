// The code at the focus: the file and lines the link names, or else the
// frame's own line, or, before the program runs, its main function (D2).

import { useEffect, useMemo, useState } from "react";
import { useRequest } from "../data";
import { formatPlace, parsePlace, showingSource } from "../focus";
import { controls } from "../model";
import type { Breakpoint } from "../protocol";
import { useConnection, useModel } from "../store";
import { closeFile, openFile, tab, useTab } from "../tab";
import { useLinkPaths, useLook } from "./navigation";
import { fileName } from "./paths";
import { type MarkKind, type Marks, SourceView } from "./source/SourceView";
import { useFocus } from "./Workspace";

/** The file a tab shows and where in it, before its text arrives. */
export interface Shown {
  path: string;
  /** The line to bring into view. */
  line: number | null;
  /** The lines the link selects. */
  selection: { line: number; end: number } | null;
}

export function useShown(): Shown | null {
  const { at, look, trace, state } = useFocus();
  const paths = useLinkPaths();
  const sources = useRequest("sources", state.session ? undefined : null).data;
  const named = look.src ? parsePlace(look.src) : null;
  if (named) {
    const path = paths.recorded(named.path);
    return path
      ? { path, line: named.line, selection: { line: named.line, end: named.end } }
      : null;
  }
  const frame = at ? trace?.frames.find((candidate) => candidate.index === at.frame) : undefined;
  if (frame?.source) {
    return { path: frame.source.path, line: frame.source.line, selection: null };
  }
  if (!at && sources?.entry) {
    return { path: sources.entry.path, line: sources.entry.line, selection: null };
  }
  return null;
}

export function CodeArea() {
  const { at, trace, state, stale } = useFocus();
  const shown = useShown();
  const files = useTab((current) => current.files);
  const look = useLook();
  const paths = useLinkPaths();

  const path = shown?.path;
  const line = shown?.line ?? null;
  useEffect(() => {
    if (path) {
      openFile(path);
    }
    tab.setState({ shown: path ? { path, line } : null });
  }, [path, line]);

  const frame = at ? trace?.frames.find((candidate) => candidate.index === at.frame) : undefined;
  const show = (path: string) =>
    look(
      (current) => showingSource(current, formatPlace({ path: paths.link(path), line: 1, end: 1 })),
      {
        replace: false,
      },
    );

  return (
    <section className="pane code grow" aria-label="Code">
      <nav className="file-tabs" aria-label="Open files">
        {files.map((path) => (
          <div key={path} className={`file-tab ${path === shown?.path ? "on" : ""}`} title={path}>
            <button
              type="button"
              className="file-name"
              aria-current={path === shown?.path ? "page" : undefined}
              onClick={() => show(path)}
            >
              {fileName(path)}
            </button>
            <button
              type="button"
              className="close"
              aria-label={`Close ${fileName(path)}`}
              onClick={() => closeFile(path)}
            >
              ×
            </button>
          </div>
        ))}
        <span className="file-tabs-end">
          {stale === "running" && at && (
            <span className="showing">Showing stop #{at.stop} · program running</span>
          )}
        </span>
      </nav>
      {shown ? (
        <SourceFile shown={shown} />
      ) : (
        <div className="empty center-message">
          {at && frame && !frame.source
            ? `${frame.name} has no source: it is at ${frame.address}${frame.module ? ` in ${frame.module}` : ""}.`
            : at && !trace
              ? stale === "passed"
                ? `Stop #${at.stop} has passed.`
                : "Reading the stack…"
              : state.inferior.state === "notStarted"
                ? "Press F5 to run the program."
                : "No source to show."}
        </div>
      )}
    </section>
  );
}

/** Lines with breakpoints in a file, and what kind each is. */
export function breakpointLines(
  breakpoints: readonly Breakpoint[],
  path: string,
): Map<number, MarkKind> {
  const lines = new Map<number, MarkKind>();
  for (const breakpoint of breakpoints) {
    const kind: MarkKind = breakpoint.logMessage
      ? "log"
      : breakpoint.condition || breakpoint.hitCondition
        ? "conditional"
        : "plain";
    for (const place of breakpoint.places) {
      if (place.path === path && place.line) {
        lines.set(place.line, kind);
      }
    }
    // One set at a line no code has yet shows where it was asked for.
    const asked = /^(.*):(\d+)$/.exec(breakpoint.location);
    if (breakpoint.places.length === 0 && asked && asked[1] === path) {
      lines.set(Number(asked[2]), "pending");
    }
  }
  return lines;
}

function SourceFile({ shown }: { shown: Shown }) {
  const { at, trace, state } = useFocus();
  const connection = useConnection();
  const control = useModel(controls);
  const paths = useLinkPaths();
  const look = useLook();
  const source = useRequest("source", { path: shown.path });
  const [error, setError] = useState<string | null>(null);

  const marks = useMemo((): Marks => {
    const innermost = trace?.frames[0];
    const frame = at ? trace?.frames.find((candidate) => candidate.index === at.frame) : undefined;
    return {
      breakpoints: breakpointLines(state.breakpoints, shown.path),
      breakable: new Set(source.data?.breakable ?? []),
      pc: innermost?.source?.path === shown.path ? innermost.source.line : null,
      frame:
        frame && frame.index > 0 && frame.source?.path === shown.path ? frame.source.line : null,
      selection: shown.selection,
      inline: new Map(),
    };
  }, [trace, at, state.breakpoints, shown, source.data]);

  if (source.error) {
    return (
      <div className="empty center-message">
        Cannot show {shown.path}: {source.error.message}
      </div>
    );
  }
  if (!source.data || source.data.path !== shown.path) {
    return <div className="empty center-message">Reading {fileName(shown.path)}…</div>;
  }

  const toggle = (line: number) => {
    if (!control) {
      return;
    }
    setError(null);
    const existing = state.breakpoints.find((breakpoint) =>
      breakpoint.places.some((place) => place.path === shown.path && place.line === line),
    );
    const request = existing
      ? connection.request("removeBreakpoint", { id: existing.id })
      : connection.request("addBreakpoint", { location: `${shown.path}:${line}` });
    request.catch((failure: Error) => setError(failure.message));
  };

  return (
    <>
      {error && (
        <div className="inline-error" role="alert">
          {error}
          <button
            type="button"
            className="icon"
            aria-label="Dismiss"
            onClick={() => setError(null)}
          >
            ×
          </button>
        </div>
      )}
      <SourceView
        path={shown.path}
        text={source.data.text}
        marks={marks}
        reveal={shown.line}
        onGutter={toggle}
        onLineNumber={(line, extend) => {
          const from = extend && shown.selection ? Math.min(shown.selection.line, line) : line;
          const to = extend && shown.selection ? Math.max(shown.selection.end, line) : line;
          look((current) =>
            showingSource(
              current,
              formatPlace({ path: paths.link(shown.path), line: from, end: to }),
            ),
          );
        }}
        onCursor={(line) => tab.setState({ cursor: { path: shown.path, line } })}
      />
    </>
  );
}
