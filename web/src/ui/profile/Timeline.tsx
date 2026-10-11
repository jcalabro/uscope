import { useEffect, useMemo, useRef, useState } from "react";
import { formatRange, parseRange, withLook } from "../../profile/look";
import { formatDuration } from "../../profile/paint";
import { token, useAnswer, useProfile } from "./context";

/** One thread's lane, in CSS pixels. */
const LANE = 18;
/** The names column. */
const NAMES = 180;

/**
 * A lane per thread of how its kept samples fall over the whole profile.
 * Dragging selects a time range, which every view then reads; a click
 * clears it. A thread's name selects or deselects it.
 */
export function Timeline() {
  const { summary, query, look, setLook } = useProfile();
  const box = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const [width, setWidth] = useState(0);
  const [dragging, setDragging] = useState<{ from: number; to: number } | null>(null);

  useEffect(() => {
    const element = box.current;
    if (!element) {
      return;
    }
    const observer = new ResizeObserver(() => setWidth(element.clientWidth));
    observer.observe(element);
    setWidth(element.clientWidth);
    return () => observer.disconnect();
  }, []);

  // Every thread, over the whole profile: the selection is drawn over it.
  const plot = Math.max(width - NAMES, 0);
  const buckets = Math.max(1, Math.min(2048, Math.floor(plot / 2)));
  const params = useMemo(() => {
    const { range: _range, threads: _threads, ...rest } = query;
    return plot > 0 ? { ...rest, buckets } : null;
  }, [query, buckets, plot]);
  const answer = useAnswer("profileTimeline", params);
  const threads = useMemo(
    () => [...summary.threads].sort((a, b) => b.weight - a.weight),
    [summary.threads],
  );
  const selected = look.th ? new Set(look.th.map(Number)) : null;
  const range = look.range ? parseRange(look.range) : null;
  const span = Math.max(summary.duration, 1);
  const height = threads.length * LANE;

  useEffect(() => {
    const element = canvas.current;
    const context = element?.getContext("2d");
    if (!element || !context || plot === 0) {
      return;
    }
    const ratio = window.devicePixelRatio || 1;
    element.width = Math.round(plot * ratio);
    element.height = Math.round(height * ratio);
    context.setTransform(ratio, 0, 0, ratio, 0, 0);
    context.clearRect(0, 0, plot, height);
    const lanes = new Map((answer.data?.lanes ?? []).map((lane) => [lane.thread, lane.weights]));
    const most = Math.max(1, ...[...lanes.values()].flat());
    const bar = token("--accent");
    const [start, end] = answer.data?.range ?? [0, span];
    const width = (plot * (end - start)) / span / Math.max(1, buckets);
    for (const [index, thread] of threads.entries()) {
      const weights = lanes.get(thread.row) ?? [];
      context.globalAlpha = selected && !selected.has(thread.row) ? 0.3 : 0.85;
      context.fillStyle = bar;
      for (const [bucket, weight] of weights.entries()) {
        if (weight > 0) {
          const h = Math.max(1, ((LANE - 3) * weight) / most);
          const x = (plot * start) / span + bucket * width;
          context.fillRect(x, index * LANE + LANE - 1 - h, Math.max(width, 1), h);
        }
      }
    }
    context.globalAlpha = 1;
    const shown = dragging
      ? [Math.min(dragging.from, dragging.to), Math.max(dragging.from, dragging.to)]
      : range;
    if (shown) {
      context.fillStyle = token("--accent-soft");
      context.globalAlpha = 0.6;
      const from = (plot * (shown[0] ?? 0)) / span;
      const to = (plot * (shown[1] ?? 0)) / span;
      context.fillRect(0, 0, from, height);
      context.fillRect(to, 0, plot - to, height);
      context.globalAlpha = 1;
    }
  }, [answer.data, threads, plot, height, span, buckets, selected, range, dragging]);

  const time = (event: React.PointerEvent) => {
    const bounds = event.currentTarget.getBoundingClientRect();
    const fraction = (event.clientX - bounds.left) / bounds.width;
    return Math.min(Math.max(fraction, 0), 1) * span;
  };
  const toggle = (row: number) =>
    setLook((look) => {
      const rows = new Set(look.th ?? []);
      if (!rows.delete(String(row))) {
        rows.add(String(row));
      }
      return withLook(look, "th", rows.size > 0 ? [...rows].sort() : undefined);
    });
  return (
    <div className="profile-timeline" ref={box} data-testid="timeline">
      <div className="lane-names" style={{ width: NAMES }}>
        {threads.map((thread) => (
          <button
            key={thread.row}
            type="button"
            className={`lane-name ${selected?.has(thread.row) ? "selected" : ""}`}
            aria-pressed={selected?.has(thread.row) ?? false}
            title={`pid ${thread.pid}, tid ${thread.tid}: ${thread.samples} samples`}
            onClick={() => toggle(thread.row)}
          >
            {thread.name ?? "thread"} <span className="dim">{thread.tid}</span>
          </button>
        ))}
      </div>
      <canvas
        ref={canvas}
        className="lanes"
        style={{ width: plot, height }}
        role="img"
        aria-label={`Samples over ${formatDuration(span)}`}
        onPointerDown={(event) => {
          event.currentTarget.setPointerCapture(event.pointerId);
          const at = time(event);
          setDragging({ from: at, to: at });
        }}
        onPointerMove={(event) => {
          if (dragging) {
            setDragging({ ...dragging, to: time(event) });
          }
        }}
        onPointerUp={(event) => {
          if (!dragging) {
            return;
          }
          const to = time(event);
          const [from, until] = [Math.min(dragging.from, to), Math.max(dragging.from, to)];
          setDragging(null);
          // A click, not a drag, clears the selection.
          const chosen =
            (until - from) * (plot / span) >= 3 ? formatRange([from, until]) : undefined;
          setLook((look) => withLook(look, "range", chosen), true);
        }}
      />
    </div>
  );
}
