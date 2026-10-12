import { type ReactNode, useEffect, useMemo, useRef, useState } from "react";
import {
  formatRange,
  parseRange,
  type SyscallTab,
  syscallQueryOf,
  withLook,
} from "../../profile/look";
import { formatBytes, formatDuration, formatInstant } from "../../profile/paint";
import { bucketLabel, heatAlpha, seriesColor, shownBuckets, stacked } from "../../profile/syscalls";
import type { SyscallGroup, SyscallQuery, SyscallRow, SyscallToken } from "../../protocol";
import { useShownScheme } from "../../theme";
import { token, useAnswer, useProfile } from "./context";

/** The names column, as wide as the timeline's, so lanes line up. */
const NAMES = 180;
const LANE = 18;
const CHART = 120;

const TABS: { tab: SyscallTab | undefined; label: string }[] = [
  { tab: undefined, label: "Summary" },
  { tab: "log", label: "Log" },
  { tab: "files", label: "Files" },
  { tab: "network", label: "Network" },
  { tab: "processes", label: "Processes" },
];

const GROUPS: { group: SyscallGroup; label: string }[] = [
  { group: "syscall", label: "by call" },
  { group: "class", label: "by class" },
  { group: "process", label: "by process" },
  { group: "thread", label: "by thread" },
];

/** The profile's system calls: what they cost, when, from where, and what
 * they did. Every panel reads the same selection, in strace's `-e`
 * syntax, over the same time range as the rest of the profile. */
export function SyscallsView() {
  const { profile, look, query, setLook } = useProfile();
  const base = useMemo(
    () => syscallQueryOf(profile, look, query.revision ?? 0),
    [profile, look, query.revision],
  );
  const summary = useAnswer("syscallSummary", { ...base, group: look.sg ?? "syscall" });
  return (
    <div className="syscalls" data-testid="syscalls">
      <div className="syscalls-bar">
        <FilterBar error={summary.error?.message} />
        <div className="segmented">
          {TABS.map(({ tab, label }) => (
            <button
              key={label}
              type="button"
              aria-pressed={look.st === tab}
              onClick={() => setLook((look) => withLook(look, "st", tab), true)}
            >
              {label}
            </button>
          ))}
        </div>
        {summary.data ? (
          <span className="muted syscalls-counts" data-testid="syscall-counts">
            {summary.data.calls.toLocaleString("en-US")} of{" "}
            {summary.data.total.toLocaleString("en-US")} calls ·{" "}
            {summary.data.errors.toLocaleString("en-US")} failed
            {summary.data.engine ? ` · ${summary.data.engine}` : ""}
            {summary.data.lost > 0 ? (
              <span className="warn"> · lost {summary.data.lost.toLocaleString("en-US")}</span>
            ) : null}
          </span>
        ) : null}
      </div>
      {summary.data?.counted ? (
        <div className="profile-untimed muted">
          The kernel counted these calls without reporting each, so there is only the summary.
        </div>
      ) : (
        <Lanes query={base} />
      )}
      <div className="syscalls-body">
        {look.st === "log" ? (
          // A new selection starts at its first page.
          <Log key={JSON.stringify(base)} query={base} />
        ) : look.st === "files" ? (
          <Files query={base} />
        ) : look.st === "network" ? (
          <Network query={base} />
        ) : look.st === "processes" ? (
          <Processes query={base} />
        ) : (
          <Summary rows={summary.data?.rows ?? []} query={base} />
        )}
      </div>
    </div>
  );
}

/** The selection, as `-e` takes it; Enter applies it. */
function FilterBar({ error }: { error: string | undefined }) {
  const { look, setLook } = useProfile();
  const [text, setText] = useState(look.sf ?? "");
  useEffect(() => setText(look.sf ?? ""), [look.sf]);
  return (
    <form
      className="syscalls-filter"
      onSubmit={(event) => {
        event.preventDefault();
        setLook((look) => withLook(look, "sf", text.trim() || undefined), true);
      }}
    >
      <input
        className="input mono"
        aria-label="Selection"
        placeholder="trace=%file status=failed"
        value={text}
        onChange={(event) => setText(event.target.value)}
        aria-invalid={error ? true : undefined}
      />
      {error ? (
        <span className="error-text" role="alert">
          {error}
        </span>
      ) : null}
    </form>
  );
}

/** A canvas's width in CSS pixels, as its box changes. */
function useWidth(box: React.RefObject<HTMLElement | null>): number {
  const [width, setWidth] = useState(0);
  useEffect(() => {
    const element = box.current;
    if (!element) {
      return;
    }
    const observer = new ResizeObserver(() => setWidth(element.clientWidth));
    observer.observe(element);
    setWidth(element.clientWidth);
    return () => observer.disconnect();
  }, [box]);
  return width;
}

/** Prepares a canvas for drawing at the screen's resolution. */
function prepare(
  element: HTMLCanvasElement | null,
  width: number,
  height: number,
): CanvasRenderingContext2D | null {
  const context = element?.getContext("2d");
  if (!element || !context || width <= 0) {
    return null;
  }
  const ratio = window.devicePixelRatio || 1;
  element.width = Math.round(width * ratio);
  element.height = Math.round(height * ratio);
  context.setTransform(ratio, 0, 0, ratio, 0, 0);
  context.clearRect(0, 0, width, height);
  return context;
}

/** Dragging across a chart of the whole profile selects a range; a click
 * clears it. Returns the handlers and the range being dragged. */
function useRangeDrag(span: number) {
  const { setLook } = useProfile();
  const [dragging, setDragging] = useState<{ from: number; to: number } | null>(null);
  const time = (event: React.PointerEvent) => {
    const bounds = event.currentTarget.getBoundingClientRect();
    const fraction = (event.clientX - bounds.left) / Math.max(bounds.width, 1);
    return Math.min(Math.max(fraction, 0), 1) * span;
  };
  const handlers = {
    onPointerDown(event: React.PointerEvent<HTMLCanvasElement>) {
      event.currentTarget.setPointerCapture(event.pointerId);
      const at = time(event);
      setDragging({ from: at, to: at });
    },
    onPointerMove(event: React.PointerEvent<HTMLCanvasElement>) {
      if (dragging) {
        setDragging({ ...dragging, to: time(event) });
      }
    },
    onPointerUp(event: React.PointerEvent<HTMLCanvasElement>) {
      if (!dragging) {
        return;
      }
      const to = time(event);
      const [from, until] = [Math.min(dragging.from, to), Math.max(dragging.from, to)];
      const width = event.currentTarget.getBoundingClientRect().width;
      setDragging(null);
      const chosen = (until - from) * (width / span) >= 3 ? formatRange([from, until]) : undefined;
      setLook((look) => withLook(look, "range", chosen), true);
    },
  };
  const shown = dragging
    ? ([Math.min(dragging.from, dragging.to), Math.max(dragging.from, dragging.to)] as const)
    : null;
  return { handlers, shown };
}

/** Shades what lies outside the selected range. */
function shade(
  context: CanvasRenderingContext2D,
  range: readonly [number, number] | null,
  span: number,
  width: number,
  height: number,
) {
  if (!range) {
    return;
  }
  const from = (width * range[0]) / span;
  const to = (width * range[1]) / span;
  context.fillStyle = token("--accent-soft");
  context.globalAlpha = 0.6;
  context.fillRect(0, 0, from, height);
  context.fillRect(to, 0, width - to, height);
  context.globalAlpha = 1;
}

/** A lane per thread of its time in calls over the whole profile, with
 * failed calls as red ticks, under the samples' timeline on one clock. */
function Lanes({ query }: { query: SyscallQuery }) {
  const { summary, look } = useProfile();
  const box = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const width = useWidth(box);
  const plot = Math.max(width - NAMES, 0);
  const buckets = Math.max(1, Math.min(2048, Math.floor(plot / 2)));
  const span = Math.max(summary.duration, 1);
  const scheme = useShownScheme();
  const { range: _range, ...whole } = query;
  const answer = useAnswer("syscallLanes", plot > 0 ? { ...whole, buckets } : null);
  const lanes = answer.data?.lanes ?? [];
  const height = Math.max(lanes.length, 1) * LANE;
  const range = look.range ? parseRange(look.range) : null;
  const { handlers, shown } = useRangeDrag(span);

  useEffect(() => {
    const context = prepare(canvas.current, plot, height);
    const data = answer.data;
    if (!context || !data) {
      return;
    }
    canvas.current?.setAttribute("data-scheme", scheme);
    const slice = (plot * data.width) / span;
    const x = (bucket: number) => (plot * (data.start + bucket * data.width)) / span;
    const busy = token("--accent");
    const failed = token("--error");
    for (const [index, lane] of lanes.entries()) {
      const top = index * LANE;
      context.fillStyle = busy;
      for (const [bucket, nanoseconds] of lane.busy.entries()) {
        if (nanoseconds > 0) {
          // As tall as the share of its slice the thread spent in calls.
          const h = Math.max(1, (LANE - 3) * Math.min(1, nanoseconds / data.width));
          context.globalAlpha = 0.75;
          context.fillRect(x(bucket), top + LANE - 1 - h, Math.max(slice, 1), h);
        }
      }
      context.globalAlpha = 1;
      context.fillStyle = failed;
      for (const [bucket, count] of lane.failed.entries()) {
        if (count > 0) {
          context.fillRect(x(bucket), top + 1, Math.max(slice, 2), 4);
        }
      }
    }
    shade(context, shown ?? range, span, plot, height);
  }, [answer.data, lanes, plot, height, span, range, shown, scheme]);

  return (
    <div className="profile-timeline syscall-lanes" ref={box} data-testid="syscall-lanes">
      <div className="lane-names" style={{ width: NAMES }}>
        {lanes.map((lane) => (
          <span
            key={`${lane.pid}-${lane.tid}`}
            className="lane-name"
            title={`pid ${lane.pid}, tid ${lane.tid}`}
          >
            {lane.name ?? "thread"} <span className="dim">{lane.tid}</span>
          </span>
        ))}
      </div>
      <canvas
        ref={canvas}
        className="lanes"
        style={{ width: plot, height }}
        role="img"
        aria-label="Each thread's time in system calls"
        {...handlers}
      />
    </div>
  );
}

/** The rate chart, the table, and the selected call's detail. */
function Summary({ rows, query }: { rows: SyscallRow[]; query: SyscallQuery }) {
  const { look, setLook } = useProfile();
  const group = look.sg ?? "syscall";
  return (
    <div className="syscalls-summary">
      <Rate query={query} />
      <div className="syscalls-split">
        <div className="pane syscalls-table-pane">
          <div className="pane-head">
            <select
              className="input profile-by"
              aria-label="Group"
              value={group}
              onChange={(event) => {
                const group = event.target.value as SyscallGroup;
                setLook((look) =>
                  withLook(
                    withLook(look, "sc", undefined),
                    "sg",
                    group === "syscall" ? undefined : group,
                  ),
                );
              }}
            >
              {GROUPS.map(({ group, label }) => (
                <option key={group} value={group}>
                  {label}
                </option>
              ))}
            </select>
          </div>
          <Table rows={rows} selectable={group === "syscall"} />
        </div>
        {look.sc && group === "syscall" ? <Detail call={look.sc} query={query} /> : null}
      </div>
    </div>
  );
}

type Sort = "total" | "calls" | "errors" | "p99" | "name";

/** Every row with `-c`'s columns and percentiles, sortable; a call's row
 * selects it. */
function Table({ rows, selectable }: { rows: SyscallRow[]; selectable: boolean }) {
  const { look, setLook } = useProfile();
  const [sort, setSort] = useState<Sort>("total");
  const [open, setOpen] = useState<string | null>(null);
  const sorted = useMemo(() => {
    const key = (row: SyscallRow): number | string =>
      sort === "name"
        ? row.name
        : sort === "calls"
          ? row.calls
          : sort === "errors"
            ? row.errors
            : sort === "p99"
              ? (row.p99Ns ?? 0)
              : row.totalNs;
    return [...rows].sort((a, b) => {
      const [x, y] = [key(a), key(b)];
      return typeof x === "string" ? x.localeCompare(String(y)) : Number(y) - Number(x);
    });
  }, [rows, sort]);
  const header = (column: Sort, label: string, numeric = true) => (
    <th className={numeric ? "num" : undefined}>
      <button
        type="button"
        className="sort"
        aria-pressed={sort === column}
        onClick={() => setSort(column)}
      >
        {label}
      </button>
    </th>
  );
  const time = (value: number | null) => (value === null ? "–" : formatDuration(value));
  return (
    <table className="syscalls-table" data-testid="syscall-table">
      <thead>
        <tr>
          {header("name", "name", false)}
          {header("calls", "calls")}
          {header("errors", "errors")}
          {header("total", "total")}
          <th className="num">p50</th>
          <th className="num">p90</th>
          {header("p99", "p99")}
          <th className="num">max</th>
          <th className="num">bytes</th>
        </tr>
      </thead>
      <tbody>
        {sorted.map((row) => (
          <SummaryRow
            key={row.key}
            row={row}
            selected={selectable && look.sc === row.key}
            open={open === row.key}
            toggle={() => setOpen(open === row.key ? null : row.key)}
            select={
              selectable
                ? () =>
                    setLook((look) =>
                      withLook(look, "sc", look.sc === row.key ? undefined : row.key),
                    )
                : undefined
            }
            time={time}
          />
        ))}
      </tbody>
    </table>
  );
}

function SummaryRow({
  row,
  selected,
  open,
  toggle,
  select,
  time,
}: {
  row: SyscallRow;
  selected: boolean;
  open: boolean;
  toggle(): void;
  select: (() => void) | undefined;
  time(value: number | null): string;
}) {
  return (
    <>
      <tr className={selected ? "selected" : undefined} onClick={select} data-row={row.key}>
        <td className="mono">
          {row.errnos.length > 0 ? (
            <button
              type="button"
              className="disclose"
              aria-expanded={open}
              aria-label="Errors by errno"
              onClick={(event) => {
                event.stopPropagation();
                toggle();
              }}
            >
              {open ? "▾" : "▸"}
            </button>
          ) : null}
          {row.name}
        </td>
        <td className="num">{row.calls.toLocaleString("en-US")}</td>
        <td className={`num ${row.errors > 0 ? "error-text" : ""}`}>
          {row.errors.toLocaleString("en-US")}
        </td>
        <td className="num">{formatDuration(row.totalNs)}</td>
        <td className="num">{time(row.p50Ns)}</td>
        <td className="num">{time(row.p90Ns)}</td>
        <td className="num">{time(row.p99Ns)}</td>
        <td className="num">{row.timed > 0 ? formatDuration(row.maxNs) : "–"}</td>
        <td className="num">{row.bytes > 0 ? formatBytes(row.bytes) : ""}</td>
      </tr>
      {open
        ? row.errnos.map((errno) => (
            <tr key={errno.name} className="errno-row">
              <td className="mono error-text">{errno.name}</td>
              <td className="num">{errno.count.toLocaleString("en-US")}</td>
              <td colSpan={7} />
            </tr>
          ))
        : null}
    </>
  );
}

/** Calls per second over the whole profile, stacked by class or by call,
 * with failures as a red line; dragging selects a range. */
function Rate({ query }: { query: SyscallQuery }) {
  const { summary, look, setLook } = useProfile();
  const box = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const width = useWidth(box);
  const plot = Math.max(width - NAMES, 0);
  const buckets = Math.max(1, Math.min(1024, Math.floor(plot / 4)));
  const span = Math.max(summary.duration, 1);
  const scheme = useShownScheme();
  const { range: _range, ...whole } = query;
  const by = look.sr ?? "class";
  const answer = useAnswer(
    "syscallRate",
    plot > 0 ? { ...whole, buckets, by: by === "syscall" ? "syscall" : "class" } : null,
  );
  const range = look.range ? parseRange(look.range) : null;
  const { handlers, shown } = useRangeDrag(span);
  const series = answer.data?.series ?? [];

  useEffect(() => {
    const context = prepare(canvas.current, plot, CHART);
    const data = answer.data;
    if (!context || !data) {
      return;
    }
    canvas.current?.setAttribute("data-scheme", scheme);
    const tops = stacked(data.series.map((series) => series.counts));
    const most = Math.max(1, ...(tops.at(-1) ?? [1]));
    const slice = (plot * data.width) / span;
    const x = (bucket: number) => (plot * (data.start + bucket * data.width)) / span;
    const y = (count: number) => CHART - 2 - ((CHART - 6) * count) / most;
    const dark = scheme === "dark";
    for (const [index, series] of data.series.entries()) {
      context.fillStyle = seriesColor(series.name, dark);
      for (const [bucket, count] of series.counts.entries()) {
        if (count > 0) {
          const top = tops[index]?.[bucket] ?? 0;
          context.fillRect(x(bucket), y(top), Math.max(slice, 1), y(top - count) - y(top));
        }
      }
    }
    context.strokeStyle = token("--error");
    context.lineWidth = 1.5;
    context.beginPath();
    for (const [bucket, count] of data.errors.entries()) {
      const at = x(bucket) + slice / 2;
      if (bucket === 0) {
        context.moveTo(at, y(count));
      } else {
        context.lineTo(at, y(count));
      }
    }
    if (data.errors.some((count) => count > 0)) {
      context.stroke();
    }
    shade(context, shown ?? range, span, plot, CHART);
  }, [answer.data, plot, span, range, shown, scheme]);

  const dark = scheme === "dark";
  return (
    <div className="syscalls-rate" ref={box} data-testid="syscall-rate">
      <div className="syscalls-legend" style={{ width: NAMES }}>
        <div className="segmented">
          {(["class", "syscall"] as const).map((each) => (
            <button
              key={each}
              type="button"
              aria-pressed={by === each}
              onClick={() =>
                setLook((look) => withLook(look, "sr", each === "class" ? undefined : "syscall"))
              }
            >
              {each === "class" ? "Classes" : "Calls"}
            </button>
          ))}
        </div>
        {series.map((series) => (
          <span key={series.name} className="legend-item">
            <span className="swatch" style={{ background: seriesColor(series.name, dark) }} />
            {by === "class" && series.name !== "other" ? `%${series.name}` : series.name}
          </span>
        ))}
        {answer.data?.errors.some((count) => count > 0) ? (
          <span className="legend-item error-text">― failed</span>
        ) : null}
      </div>
      <canvas
        ref={canvas}
        style={{ width: plot, height: CHART }}
        role="img"
        aria-label="Calls over time"
        {...handlers}
      />
    </div>
  );
}

/** One call's heat map of latency over the range, its histogram against
 * the whole trace's, its errors, and where its slow and failed calls came
 * from. */
function Detail({ call, query }: { call: string; query: SyscallQuery }) {
  const selected = useMemo(
    () => ({ ...query, filter: `${query.filter} trace=${call}`.trim() }),
    [query, call],
  );
  const detail = useAnswer("syscallDetail", { ...query, call });
  const data = detail.data;
  const [first, last] = data ? shownBuckets(data.whole, data.selected) : [0, -1];
  const most = data ? Math.max(1, ...data.whole, ...data.selected) : 1;
  return (
    <div className="pane syscalls-detail" data-testid="syscall-detail">
      <div className="pane-head">
        <span className="mono">{call}</span>
      </div>
      <Heatmap query={selected} />
      <section>
        <h3>Latency</h3>
        {last < first ? <p className="muted">No call was timed.</p> : null}
        <div className="latency-bars">
          {data && last >= first
            ? data.whole.slice(first, last + 1).map((whole, offset) => {
                const bucket = first + offset;
                const chosen = data.selected[bucket] ?? 0;
                return (
                  <div key={bucket} className="histogram-row" title={`${chosen} of ${whole} calls`}>
                    <span className="mono muted">{bucketLabel(bucket)}</span>
                    <span className="histogram-bar">
                      <span className="whole" style={{ width: `${(100 * whole) / most}%` }} />
                      <span className="chosen" style={{ width: `${(100 * chosen) / most}%` }} />
                    </span>
                    <span className="num">{chosen.toLocaleString("en-US")}</span>
                  </div>
                );
              })
            : null}
        </div>
      </section>
      {data && data.errnos.length > 0 ? (
        <section>
          <h3>Errors</h3>
          {data.errnos.map((errno) => (
            <div key={errno.name} className="histogram-row">
              <span className="mono error-text">{errno.name}</span>
              <span className="histogram-bar">
                <span
                  className="error"
                  style={{
                    width: `${(100 * errno.count) / Math.max(1, data.errnos[0]?.count ?? 1)}%`,
                  }}
                />
              </span>
              <span className="num">{errno.count.toLocaleString("en-US")}</span>
            </div>
          ))}
        </section>
      ) : null}
      {data ? (
        <section data-testid="syscall-callers">
          <h3>
            Callers of slow{data.slowNs !== null ? ` (≥ ${formatDuration(data.slowNs)})` : ""} and
            failed calls
          </h3>
          {data.callers.length === 0 ? <p className="muted">No stacks were recorded.</p> : null}
          {data.callers.map((caller) => (
            <div key={caller.site} className="caller">
              <span className="mono">{caller.site}</span>
              <span className="muted">
                {caller.slow > 0 ? `${caller.slow} slow` : ""}
                {caller.slow > 0 && caller.failed > 0 ? ", " : ""}
                {caller.failed > 0 ? (
                  <span className="error-text">{caller.failed} failed</span>
                ) : null}
              </span>
            </div>
          ))}
          {data.unplaced > 0 ? <p className="muted">{data.unplaced} had no stack.</p> : null}
        </section>
      ) : null}
    </div>
  );
}

/** Calls by when they returned, across, and how long they took, up. */
function Heatmap({ query }: { query: SyscallQuery }) {
  const box = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const width = useWidth(box);
  const scheme = useShownScheme();
  const buckets = Math.max(1, Math.min(512, Math.floor(width / 4)));
  const answer = useAnswer("syscallHeatmap", width > 0 ? { ...query, buckets } : null);
  const data = answer.data;
  // Only the latencies that occur, so the cells stay tall.
  const [first, last] = useMemo(() => {
    if (!data) {
      return [0, -1];
    }
    const rows = Array.from({ length: data.rows }, (_, row) => {
      let count = 0;
      for (let slice = 0; slice * data.rows < data.counts.length; slice++) {
        count += data.counts[slice * data.rows + row] ?? 0;
      }
      return count;
    });
    return shownBuckets(rows);
  }, [data]);
  const height = 140;

  useEffect(() => {
    const context = prepare(canvas.current, width, height);
    if (!context || !data || last < first) {
      return;
    }
    canvas.current?.setAttribute("data-scheme", scheme);
    const shown = last - first + 1;
    const cell = height / shown;
    const slices = data.counts.length / data.rows;
    const column = width / slices;
    context.fillStyle = token("--accent");
    for (let slice = 0; slice < slices; slice++) {
      for (let row = first; row <= last; row++) {
        const alpha = heatAlpha(data.counts[slice * data.rows + row] ?? 0, data.most);
        if (alpha > 0) {
          context.globalAlpha = alpha;
          context.fillRect(
            slice * column,
            height - (row - first + 1) * cell,
            Math.max(column, 1),
            cell,
          );
        }
      }
    }
    context.globalAlpha = 1;
  }, [data, width, first, last, scheme]);

  return (
    <section className="latency-map" data-testid="syscall-heatmap">
      <h3>
        Latency over time
        {last >= first ? (
          <span className="muted">
            {" "}
            · {bucketLabel(first)} to {bucketLabel(last)}
          </span>
        ) : null}
      </h3>
      {/* Measured without the section's padding, and never wider than
          it, so the canvas cannot widen the column it is measured in. */}
      <div ref={box} className="plot">
        <canvas ref={canvas} style={{ width, height }} role="img" aria-label="Latency heat map" />
      </div>
    </section>
  );
}

/** Tokens colored as the CLI colors them. */
function Tokens({ tokens }: { tokens: SyscallToken[] }) {
  return (
    <>
      {tokens.map((token, index) => (
        // biome-ignore lint/suspicious/noArrayIndexKey: tokens never move
        <span key={index} className={`tk-${token.kind}`}>
          {token.text}
        </span>
      ))}
    </>
  );
}

const PAGE = 500;

/** The trace as strace prints it, a page at a time; a call's line opens
 * its arguments, bytes, and stack. */
function Log({ query }: { query: SyscallQuery }) {
  const { look, setLook, summary } = useProfile();
  const [pages, setPages] = useState(1);
  const log = useAnswer("syscallLog", { ...query, offset: 0, limit: PAGE * pages });
  const lines = log.data?.lines ?? [];
  return (
    <div className="pane syscalls-log" data-testid="syscall-log">
      <div className="log-lines mono">
        {lines.map((line) => {
          const open = look.ev === String(line.event);
          return (
            <div key={line.event} className={`log-line ${line.failed ? "failed" : ""}`}>
              <button
                type="button"
                className="log-head"
                disabled={!line.call}
                aria-expanded={line.call ? open : undefined}
                onClick={() =>
                  setLook((look) => withLook(look, "ev", open ? undefined : String(line.event)))
                }
              >
                <span className="muted">{formatInstant(line.time, 1000, summary.duration)}</span>{" "}
                <span className="tk-pid">{line.tid}</span> <Tokens tokens={line.tokens} />
                {line.durationNs !== null ? (
                  <span className="tk-time"> &lt;{formatDuration(line.durationNs)}&gt;</span>
                ) : null}
              </button>
              {open ? <Call event={line.event} /> : null}
            </div>
          );
        })}
      </div>
      {log.data && log.data.total > lines.length ? (
        <button type="button" className="button" onClick={() => setPages(pages + 1)}>
          Show more ({(log.data.total - lines.length).toLocaleString("en-US")} left)
        </button>
      ) : null}
      {log.data && log.data.total === 0 ? <p className="muted">No call matches.</p> : null}
    </div>
  );
}

function Call({ event }: { event: number }) {
  const { profile, query } = useProfile();
  const call = useAnswer("syscallCall", { profile, revision: query.revision ?? 0, event });
  const data = call.data;
  if (!data) {
    return <div className="log-call muted">Reading…</div>;
  }
  return (
    <div className="log-call" data-testid="syscall-call">
      <table>
        <tbody>
          {data.arguments.map((argument) => (
            <tr key={argument.name}>
              <th>{argument.name}</th>
              <td>
                <Tokens tokens={argument.tokens} />
              </td>
            </tr>
          ))}
          <tr>
            <th>result</th>
            <td>
              <Tokens tokens={data.result} />
            </td>
          </tr>
        </tbody>
      </table>
      {data.dumps.length > 0 ? <pre className="dump">{data.dumps.join("\n")}</pre> : null}
      {data.stack.length > 0 ? (
        <ol className="stack">
          {data.stack.map((name, index) => (
            // biome-ignore lint/suspicious/noArrayIndexKey: a stack's frames never move
            <li key={index}>{name}</li>
          ))}
          {data.stackEnds ? <li className="muted">stack ends: {data.stackEnds}</li> : null}
        </ol>
      ) : null}
    </div>
  );
}

/** A plain table of rows. */
function Rows({ head, rows, empty }: { head: string[]; rows: ReactNode[][]; empty: string }) {
  if (rows.length === 0) {
    return <p className="muted pad">{empty}</p>;
  }
  return (
    <table className="syscalls-table">
      <thead>
        <tr>
          {head.map((name, index) => (
            <th key={name} className={index > 0 ? "num" : undefined}>
              {name}
            </th>
          ))}
        </tr>
      </thead>
      <tbody>
        {rows.map((cells, row) => (
          // biome-ignore lint/suspicious/noArrayIndexKey: rows are answered in order
          <tr key={row}>
            {cells.map((cell, index) => (
              // biome-ignore lint/suspicious/noArrayIndexKey: columns are fixed
              <td key={index} className={index > 0 ? "num" : "mono"}>
                {cell}
              </td>
            ))}
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function Files({ query }: { query: SyscallQuery }) {
  const files = useAnswer("syscallFiles", query);
  return (
    <div className="pane" data-testid="syscall-files">
      <Rows
        head={["path", "opens", "failed", "read", "written", "fsync", "time"]}
        empty="No file was opened or used."
        rows={(files.data?.files ?? []).map((file) => [
          file.path,
          file.opens,
          file.failed > 0 ? (
            <span key="failed" className="error-text">
              {file.failed}
            </span>
          ) : (
            0
          ),
          formatBytes(file.read),
          formatBytes(file.written),
          formatDuration(file.syncNs),
          formatDuration(file.totalNs),
        ])}
      />
    </div>
  );
}

function Network({ query }: { query: SyscallQuery }) {
  const network = useAnswer("syscallNetwork", query);
  return (
    <div className="pane" data-testid="syscall-network">
      <Rows
        head={["endpoint", "connects", "failed", "connect time", "sent", "received"]}
        empty="No socket was used."
        rows={(network.data?.endpoints ?? []).map((endpoint) => [
          endpoint.endpoint,
          endpoint.connects,
          endpoint.failed > 0 ? (
            <span key="failed" className="error-text">
              {endpoint.failed}
            </span>
          ) : (
            0
          ),
          formatDuration(endpoint.connectNs),
          formatBytes(endpoint.sent),
          formatBytes(endpoint.received),
        ])}
      />
    </div>
  );
}

function Processes({ query }: { query: SyscallQuery }) {
  const processes = useAnswer("syscallProcesses", query);
  const all = processes.data?.processes ?? [];
  const depth = new Map<number, number>();
  for (const process of all) {
    depth.set(process.pid, process.parent === null ? 0 : (depth.get(process.parent) ?? -1) + 1);
  }
  return (
    <div className="pane" data-testid="syscall-processes">
      <Rows
        head={["process", "threads", "calls", "errors", "lifetime", "end"]}
        empty="No process was seen."
        rows={all.map((process) => [
          <span key="name" style={{ paddingLeft: `${(depth.get(process.pid) ?? 0) * 16}px` }}>
            {process.pid}{" "}
            <span className="muted">
              {process.images.map((image) => image.slice(image.lastIndexOf("/") + 1)).join(" → ")}
            </span>
          </span>,
          process.threads,
          process.calls.toLocaleString("en-US"),
          process.errors > 0 ? (
            <span key="errors" className="error-text">
              {process.errors}
            </span>
          ) : (
            0
          ),
          process.ended !== null ? formatDuration(process.ended - process.started) : "running",
          process.end ?? "",
        ])}
      />
    </div>
  );
}
