import { useEffect, useMemo, useRef, useState } from "react";
import { formatRange, parseRange, withLook } from "../../profile/look";
import { formatDuration } from "../../profile/paint";
import type { ProfileLane, ProfileSummary, ProfileThread } from "../../protocol";
import { useShownScheme } from "../../theme";
import { type ProfileFocus, token, useAnswer, useProfile } from "./context";

/** One lane, in CSS pixels. */
const LANE = 18;
/** The names column. */
const NAMES = 180;
/** The most task lanes drawn, the heaviest. */
const MOST_TASK_LANES = 50;

/** A lane: what the look selects it by, and the answer's lane id. */
interface Row {
  id: string;
  lane: number;
  label: string;
  dim?: string;
  title: string;
}

/** The lanes a look shows: threads, or tasks, which waits always are, and
 * for samples, what no snapshot named. */
function rows(summary: ProfileSummary, look: ProfileFocus["look"]): Row[] {
  const waits = look.src === "waits";
  if (!waits && look.lanes !== "tasks") {
    return [...summary.threads]
      .sort((a, b) => b.weight - a.weight)
      .map((thread) => ({
        id: String(thread.row),
        lane: thread.row,
        label: thread.name ?? "thread",
        dim: String(thread.tid),
        title: `pid ${thread.pid}, tid ${thread.tid}: ${thread.samples} samples${offCpu(thread)}`,
      }));
  }
  const weight = (task: ProfileSummary["tasks"][number]) => (waits ? task.waited : task.weight);
  const tasks: Row[] = summary.tasks
    .filter((task) => weight(task) > 0)
    .sort((a, b) => weight(b) - weight(a))
    .slice(0, MOST_TASK_LANES)
    .map((task) => ({
      id: String(task.row),
      lane: task.row,
      label: task.name,
      title: `${task.name}: ${formatDuration(task.weight)} sampled, ${formatDuration(task.waited)} waiting`,
    }));
  if (!waits) {
    tasks.push({
      id: "none",
      lane: -1,
      label: "no task known",
      title: "Samples no task snapshot named the task of",
    });
  }
  return tasks;
}

/**
 * A lane per thread of how its kept samples fall over the whole profile,
 * over bands of the time it spent off its CPU: grey where it blocked,
 * amber where it was preempted, as strong as the share of the slice it
 * was off; or a lane per task, of its samples or waits. Dragging selects a
 * time range, which every view then reads; a click clears it. A lane's
 * name selects or deselects its thread or task.
 */
export function Timeline() {
  const { summary, query, look, setLook } = useProfile();
  const box = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const [width, setWidth] = useState(0);
  const scheme = useShownScheme();
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
  const byTask = look.src === "waits" || look.lanes === "tasks";
  const params = useMemo(() => {
    const { range: _range, threads: _threads, tasks: _tasks, ...rest } = query;
    return plot > 0 ? { ...rest, buckets, taskLanes: byTask } : null;
  }, [query, buckets, plot, byTask]);
  const answer = useAnswer("profileTimeline", params);
  const lanes = useMemo(() => rows(summary, look), [summary, look]);
  const chosen = byTask ? look.tk : look.th;
  const selected = chosen ? new Set(chosen) : null;
  const range = look.range ? parseRange(look.range) : null;
  const span = Math.max(summary.duration, 1);
  const height = lanes.length * LANE;

  useEffect(() => {
    const element = canvas.current;
    const context = element?.getContext("2d");
    if (!element || !context || plot === 0) {
      return;
    }
    const ratio = window.devicePixelRatio || 1;
    element.width = Math.round(plot * ratio);
    element.height = Math.round(height * ratio);
    // Its colors are the scheme's, so a change of scheme paints it again.
    element.dataset.scheme = scheme;
    context.setTransform(ratio, 0, 0, ratio, 0, 0);
    context.clearRect(0, 0, plot, height);
    const answered = new Map(
      (answer.data?.lanes ?? []).map((lane) => [(byTask ? lane.task : lane.thread) ?? -1, lane]),
    );
    const most = Math.max(1, ...[...answered.values()].flatMap((lane) => lane.weights));
    const bar = token("--accent");
    const [start, end] = answer.data?.range ?? [0, span];
    const width = (plot * (end - start)) / span / Math.max(1, buckets);
    const slice = Math.max(1, (end - start) / Math.max(1, buckets));
    const bands: [keyof Pick<ProfileLane, "blocked" | "preempted">, string][] = [
      ["blocked", token("--line-strong")],
      ["preempted", token("--warn-line")],
    ];
    for (const [index, row] of lanes.entries()) {
      const lane = answered.get(row.lane);
      const dim = selected && !selected.has(row.id) ? 0.3 : 1;
      for (const [kind, color] of bands) {
        context.fillStyle = color;
        for (const [bucket, off] of (lane?.[kind] ?? []).entries()) {
          if (off > 0) {
            context.globalAlpha = dim * Math.min(1, off / slice);
            const x = (plot * start) / span + bucket * width;
            context.fillRect(x, index * LANE + 1, Math.max(width, 1), LANE - 2);
          }
        }
      }
      const weights = lane?.weights ?? [];
      context.globalAlpha = dim * 0.85;
      context.fillStyle = bar;
      for (const [bucket, weight] of weights.entries()) {
        if (weight > 0) {
          const h = Math.max(1, ((LANE - 3) * weight) / most);
          const x = (plot * start) / span + bucket * width;
          context.fillRect(x, index * LANE + LANE - 1 - h, Math.max(width, 1), h);
        }
      }
    }
    // Spans not sampled cover every lane.
    context.fillStyle = token("--line");
    context.globalAlpha = 0.7;
    for (const [from, to] of summary.gaps) {
      const x = (plot * from) / span;
      context.fillRect(x, 0, Math.max((plot * (to - from)) / span, 1), height);
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
      // A range too short to see between its shades still shows where it is.
      const wide = Math.max(to - from, 3);
      context.fillStyle = token("--accent");
      context.fillRect((from + to) / 2 - wide / 2, 0, wide, 2);
      context.fillRect((from + to) / 2 - wide / 2, height - 2, wide, 2);
    }
  }, [
    answer.data,
    lanes,
    byTask,
    plot,
    height,
    span,
    buckets,
    selected,
    range,
    dragging,
    summary.gaps,
    scheme,
  ]);

  const time = (event: React.PointerEvent) => {
    const bounds = event.currentTarget.getBoundingClientRect();
    const fraction = (event.clientX - bounds.left) / bounds.width;
    return Math.min(Math.max(fraction, 0), 1) * span;
  };
  const toggle = (id: string) =>
    setLook((look) => {
      const key = byTask ? "tk" : "th";
      const ids = new Set(look[key] ?? []);
      if (!ids.delete(id)) {
        ids.add(id);
      }
      return withLook(look, key, ids.size > 0 ? [...ids].sort() : undefined);
    });
  return (
    <div className="profile-timeline" ref={box} data-testid="timeline">
      <div className="lane-names" style={{ width: NAMES }}>
        {lanes.map((row) => (
          <button
            key={row.id}
            type="button"
            className={`lane-name ${selected?.has(row.id) ? "selected" : ""}`}
            aria-pressed={selected?.has(row.id) ?? false}
            title={row.title}
            onClick={() => toggle(row.id)}
          >
            {row.label} {row.dim ? <span className="dim">{row.dim}</span> : null}
          </button>
        ))}
      </div>
      <canvas
        ref={canvas}
        className="lanes"
        style={{ width: plot, height }}
        role="img"
        aria-label={`Samples and time off CPU over ${formatDuration(span)}`}
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

/** What a lane's name says of its thread's time off its CPU, if any. */
function offCpu(thread: ProfileThread): string {
  if (thread.blocked === 0 && thread.preempted === 0) {
    return "";
  }
  const blocked = formatDuration(thread.blocked);
  const preempted = formatDuration(thread.preempted);
  return `; off its CPU ${blocked} blocked, ${preempted} preempted`;
}
