import { useEffect, useMemo, useRef, useState } from "react";
import { formatRange, parseRange, withLook } from "../../profile/look";
import { formatDuration, hue } from "../../profile/paint";
import {
  blockAt,
  decodeZones,
  describeFlags,
  plotPath,
  type Track,
  tracks,
} from "../../profile/zones";
import type { ProfileMark, ProfilePlot, ProfileZoneSite } from "../../protocol";
import { isDark, token, useAnswer, useProfile } from "./context";

/** A depth's row, the gap between tracks, and the names column, in CSS
 * pixels. */
const ROW = 16;
const GAP = 6;
const NAMES = 180;
const PLOT = 40;

/** What the pointer is over. */
interface Tip {
  x: number;
  y: number;
  lines: string[];
}

/**
 * Each thread's zones and labels over the selected range, a row per
 * depth, with the program's messages and frames as ticks above. Zones
 * under three pixels wide fold into one grey block; a click on a block
 * selects its span, which every view then reads. Beside it, Find Zone
 * measures one site's zones.
 */
export function ZonesView() {
  return (
    <div className="profile-zones" data-testid="zones">
      <ZoneTracks />
      <FindZone />
    </div>
  );
}

function ZoneTracks() {
  const { profile, summary, query, look, setLook } = useProfile();
  const box = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const [width, setWidth] = useState(0);
  const [tip, setTip] = useState<Tip | null>(null);

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

  const plot = Math.max(width - NAMES, 0);
  const selected = look.range ? parseRange(look.range) : null;
  const params = useMemo(
    () =>
      plot > 0
        ? {
            profile,
            revision: query.revision ?? 0,
            ...(selected ? { range: selected } : {}),
            ...(query.threads ? { threads: query.threads } : {}),
            width: Math.round(plot),
          }
        : null,
    [profile, query.revision, query.threads, selected?.[0], selected?.[1], plot],
  );
  const answer = useAnswer("profileZones", params);
  const blocks = useMemo(
    () => (answer.data ? decodeZones(answer.data, answer.data.payload) : null),
    [answer.data],
  );
  const order = useMemo(
    () => [...summary.threads].sort((a, b) => b.weight - a.weight).map((thread) => thread.row),
    [summary.threads],
  );
  const laid = useMemo(() => (blocks ? tracks(blocks, order) : []), [blocks, order]);
  const marks = answer.data?.marks ?? [];
  const range = answer.data?.range ?? [0, Math.max(summary.duration, 1)];
  const span = Math.max(range[1] - range[0], 1);
  const height =
    laid.length === 0
      ? 0
      : trackTop(laid.at(-1) as Track, laid.length - 1) + ROW * (laid.at(-1)?.depths ?? 0);
  const origin = range[0];
  const name = (thread: number) =>
    summary.threads.find((each) => each.row === thread)?.name ?? `thread ${thread}`;

  useEffect(() => {
    const element = canvas.current;
    const context = element?.getContext("2d");
    if (!element || !context || !blocks || plot === 0) {
      return;
    }
    const x = (time: number) => ((time - origin) * plot) / span;
    const ratio = window.devicePixelRatio || 1;
    element.width = Math.round(plot * ratio);
    element.height = Math.round(Math.max(height, 1) * ratio);
    context.setTransform(ratio, 0, 0, ratio, 0, 0);
    context.clearRect(0, 0, plot, height);
    context.font = "11px var(--mono)";
    context.textBaseline = "middle";
    const dark = isDark();
    const ink = token("--ink");
    const folded = token("--line-strong");
    const flagged = token("--changed");
    const found = token("--accent");
    const positions = new Map(laid.map((track, index) => [track.thread, trackTop(track, index)]));
    for (let at = 0; at < blocks.count; at += 1) {
      const y = positions.get(blocks.thread[at] ?? 0);
      if (y === undefined) {
        continue;
      }
      const row = y + (blocks.depth[at] ?? 0) * ROW;
      const from = x(blocks.start[at] ?? 0);
      const wide = Math.max(x(blocks.end[at] ?? 0) - from, 1);
      const label = blocks.names[blocks.name[at] ?? 0] ?? "";
      const many = (blocks.zones[at] ?? 1) > 1;
      context.fillStyle = many
        ? folded
        : dark
          ? `hsl(${hue(label.split("=")[0] ?? label)} 35% 32%)`
          : `hsl(${hue(label.split("=")[0] ?? label)} 55% 76%)`;
      context.fillRect(from, row + 1, wide, ROW - 2);
      const site = label.split("=")[0];
      if (look.zs && site === look.zs && !many) {
        context.strokeStyle = found;
        context.lineWidth = 2;
        context.strokeRect(from + 1, row + 2, Math.max(wide - 2, 1), ROW - 4);
      } else if ((blocks.flags[at] ?? 0) !== 0) {
        context.strokeStyle = flagged;
        context.lineWidth = 1;
        context.setLineDash([3, 2]);
        context.strokeRect(from + 0.5, row + 1.5, Math.max(wide - 1, 1), ROW - 3);
        context.setLineDash([]);
      }
      if (wide > 40) {
        context.save();
        context.beginPath();
        context.rect(from, row, wide, ROW);
        context.clip();
        context.fillStyle = ink;
        const length = (blocks.end[at] ?? 0) - (blocks.start[at] ?? 0);
        const text = many ? `${blocks.zones[at]} zones` : `${label} ${formatDuration(length)}`;
        context.fillText(text, from + 3, row + ROW / 2);
        context.restore();
      }
    }
    // The program's marks, as ticks above each track.
    for (const mark of marks) {
      const y = positions.get(mark.thread);
      if (y === undefined) {
        continue;
      }
      context.fillStyle =
        mark.kind === "trigger" ? flagged : mark.kind === "frame" ? folded : found;
      context.fillRect(x(mark.time) - 0.5, y - ROW + 3, 1.5, ROW - 4);
    }
  }, [blocks, laid, marks, plot, height, look.zs, origin, span]);

  const point = (event: React.MouseEvent<HTMLCanvasElement>) => {
    const bounds = event.currentTarget.getBoundingClientRect();
    const px = event.clientX - bounds.left;
    const py = event.clientY - bounds.top;
    const time = range[0] + (px * span) / Math.max(plot, 1);
    const slack = (2 * span) / Math.max(plot, 1);
    for (const [index, track] of laid.entries()) {
      const first = trackTop(track, index);
      if (py >= first - ROW && py < first) {
        const near = nearest(marks, track.thread, time, slack * 2);
        return { px, py, time, block: null, mark: near };
      }
      if (py >= first && py < first + track.depths * ROW) {
        const depth = Math.floor((py - first) / ROW);
        return {
          px,
          py,
          time,
          block: blocks ? blockAt(blocks, track, depth, time, slack) : null,
          mark: null,
        };
      }
    }
    return { px, py, time, block: null, mark: null };
  };

  return (
    <div className="pane grow zones-tracks">
      <div className="pane-head">
        Zones
        <span className="count">
          {summary.zoneSites.reduce((sum, site) => sum + site.count, 0)}
        </span>
        {selected ? (
          <span className="muted">
            {formatDuration(selected[0])} – {formatDuration(selected[1])}
          </span>
        ) : null}
      </div>
      {answer.error ? (
        <div className="empty">{answer.error.message}</div>
      ) : (
        <div className={`pane-body zones-body ${answer.current ? "" : "stale"}`} ref={box}>
          <div className="zone-lane">
            <div className="zone-names" style={{ width: NAMES, height }}>
              {laid.map((track, index) => (
                <div
                  key={track.thread}
                  className="zone-name"
                  style={{ top: trackTop(track, index) - ROW }}
                >
                  {name(track.thread)}
                </div>
              ))}
            </div>
            <canvas
              ref={canvas}
              style={{ width: plot, height }}
              role="img"
              aria-label={`Zones over ${formatDuration(span)}`}
              onPointerMove={(event) => {
                const at = point(event);
                const lines =
                  at.block !== null && blocks
                    ? blockLines(blocks, at.block, name)
                    : at.mark
                      ? [
                          `${at.mark.kind} ${at.mark.name} at ${formatDuration(at.mark.time)}`,
                          ...(at.mark.text ? [at.mark.text] : []),
                        ]
                      : null;
                setTip(lines ? { x: at.px + NAMES + 12, y: at.py + 30, lines } : null);
              }}
              onPointerLeave={() => setTip(null)}
              onClick={(event) => {
                const at = point(event);
                if (at.block !== null && blocks) {
                  const start = blocks.start[at.block] ?? 0;
                  const end = blocks.end[at.block] ?? 0;
                  if (end > start) {
                    setLook((look) => withLook(look, "range", formatRange([start, end])), true);
                  }
                }
              }}
            />
          </div>
          {(answer.data?.plots ?? []).map((series) => (
            <PlotLane key={series.name} series={series} origin={origin} span={span} width={plot} />
          ))}
          {tip ? (
            <div className="flame-tip" style={{ left: tip.x, top: tip.y }} data-testid="zone-tip">
              {tip.lines.map((line) => (
                <div key={line}>{line}</div>
              ))}
            </div>
          ) : null}
          {blocks && blocks.count === 0 && (answer.data?.plots.length ?? 0) === 0 ? (
            <div className="empty">No zones in this range.</div>
          ) : null}
        </div>
      )}
    </div>
  );
}

/** One plotted series as a line from its lowest value to its highest. */
function PlotLane({
  series,
  origin,
  span,
  width,
}: {
  series: ProfilePlot;
  origin: number;
  span: number;
  width: number;
}) {
  let [low, high] = [Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY];
  for (const bin of series.bins) {
    low = Math.min(low, bin[3] ?? low);
    high = Math.max(high, bin[4] ?? high);
  }
  const extent = series.bins.length > 0 ? `${formatValue(low)} – ${formatValue(high)}` : "";
  return (
    <div className="zone-lane zone-plot">
      <div className="zone-names" style={{ width: NAMES, height: PLOT }}>
        <div className="zone-name">{series.name}</div>
        <div className="zone-name dim" style={{ top: 16 }}>
          {extent}
          {series.unshown > 0 ? ` · ${series.unshown} not finite` : ""}
        </div>
      </div>
      <svg
        width={width}
        height={PLOT}
        role="img"
        aria-label={`Plot ${series.name}, ${series.values} values${extent ? ` from ${extent}` : ""}`}
      >
        <path d={plotPath(series.bins, origin, span, width, PLOT, low, high)} />
      </svg>
    </div>
  );
}

/** A plotted value, short. */
function formatValue(value: number): string {
  return Number.isInteger(value) ? String(value) : value.toPrecision(4);
}

/** Where a track's first row begins: its marks take the row above. */
function trackTop(track: Track, index: number): number {
  return (track.row + 1) * ROW + index * GAP;
}

/** A tooltip's lines for a block. */
function blockLines(
  blocks: ReturnType<typeof decodeZones>,
  at: number,
  name: (thread: number) => string,
): string[] {
  const length = (blocks.end[at] ?? 0) - (blocks.start[at] ?? 0);
  const count = blocks.zones[at] ?? 1;
  const label = blocks.names[blocks.name[at] ?? 0] ?? "";
  const head =
    count > 1
      ? `${count} ${label === "zones" ? "zones" : label} over ${formatDuration(length)}: click to see them`
      : `${label}: ${formatDuration(length)}`;
  return [
    head,
    `${name(blocks.thread[at] ?? 0)}, from ${formatDuration(blocks.start[at] ?? 0)}`,
    ...describeFlags(blocks.flags[at] ?? 0),
  ];
}

/** The mark of a thread nearest `time`, within `slack`. */
function nearest(marks: ProfileMark[], thread: number, time: number, slack: number) {
  let best: ProfileMark | null = null;
  for (const mark of marks) {
    if (mark.thread === thread && Math.abs(mark.time - time) <= slack) {
      if (!best || Math.abs(mark.time - time) < Math.abs(best.time - time)) {
        best = mark;
      }
    }
  }
  return best;
}

/** Every zone site and label key, and one's spread of lengths. */
function FindZone() {
  const { summary, look, setLook } = useProfile();
  const chosen = summary.zoneSites.find((site) => site.name === look.zs);
  return (
    <div className="pane find-zone" data-testid="find-zone">
      <div className="pane-head">
        Find zone<span className="count">{summary.zoneSites.length}</span>
      </div>
      <div className="pane-body">
        <table className="zone-sites">
          <thead>
            <tr>
              <th>zone</th>
              <th>count</th>
              <th>total</th>
              <th>longest</th>
            </tr>
          </thead>
          <tbody>
            {summary.zoneSites.map((site) => (
              <SiteRow
                key={`${site.label}:${site.name}`}
                site={site}
                chosen={site.name === look.zs}
                choose={() =>
                  setLook((look) =>
                    withLook(look, "zs", look.zs === site.name ? undefined : site.name),
                  )
                }
              />
            ))}
          </tbody>
        </table>
        {chosen ? <ZoneStats site={chosen} /> : null}
      </div>
    </div>
  );
}

function SiteRow({
  site,
  chosen,
  choose,
}: {
  site: ProfileZoneSite;
  chosen: boolean;
  choose: () => void;
}) {
  return (
    <tr className={chosen ? "selected" : undefined}>
      <td>
        <button type="button" className="link-button" aria-pressed={chosen} onClick={choose}>
          {site.name}
        </button>
        {site.label ? <span className="dim"> label</span> : null}
        {site.flagged > 0 ? (
          <span className="warn" title="Zones whose begin or end was not seen, or did not nest">
            {" "}
            {site.flagged} flagged
          </span>
        ) : null}
      </td>
      <td>{site.count.toLocaleString("en-US")}</td>
      <td>{formatDuration(site.total)}</td>
      <td>{formatDuration(site.longest)}</td>
    </tr>
  );
}

/** One site's zones in the range: their percentiles, a histogram of
 * lengths, the longest, and for a label, its values, each a filter. */
function ZoneStats({ site }: { site: ProfileZoneSite }) {
  const { profile, query, look, setLook, summary } = useProfile();
  const selected = look.range ? parseRange(look.range) : null;
  const params = useMemo(
    () => ({
      profile,
      revision: query.revision ?? 0,
      name: site.name,
      ...(selected ? { range: selected } : {}),
      ...(query.threads ? { threads: query.threads } : {}),
    }),
    [profile, query.revision, site.name, selected?.[0], selected?.[1], query.threads],
  );
  const answer = useAnswer("profileZoneStats", params);
  const stats = answer.data;
  if (answer.error) {
    return <div className="empty">{answer.error.message}</div>;
  }
  if (!stats) {
    return <div className="muted">Measuring {site.name}…</div>;
  }
  const most = Math.max(1, ...stats.histogram.map(([, count]) => count));
  const thread = (row: number) =>
    summary.threads.find((each) => each.row === row)?.name ?? `thread ${row}`;
  return (
    <div className="zone-stats" data-testid="zone-stats">
      <dl>
        {(
          [
            ["count", stats.count.toLocaleString("en-US")],
            ["total", formatDuration(stats.total)],
            ["mean", formatDuration(stats.count > 0 ? stats.total / stats.count : 0)],
            ["median", formatDuration(stats.median)],
            ["p90", formatDuration(stats.p90)],
            ["p99", formatDuration(stats.p99)],
            ["max", formatDuration(stats.max)],
            ["flagged", String(stats.flagged)],
          ] as const
        ).map(([term, value]) => (
          <div key={term}>
            <dt>{term}</dt>
            <dd>{value}</dd>
          </div>
        ))}
      </dl>
      <div className="zone-histogram" role="img" aria-label={`Lengths of ${site.name}`}>
        {stats.histogram.map(([low, count]) => (
          <span
            key={low}
            className="bar"
            style={{ height: `${(100 * count) / most}%` }}
            title={`${count} from ${formatDuration(low)}`}
          />
        ))}
      </div>
      <div className="zone-axis muted">
        <span>{formatDuration(stats.min)}</span>
        <span>{formatDuration(stats.max)}</span>
      </div>
      <div className="pane-head">Longest</div>
      <ul className="zone-longest">
        {stats.longest.map((zone) => (
          <li key={`${zone.thread}:${zone.start}`}>
            <button
              type="button"
              className="link-button"
              title="Select this zone's span"
              onClick={() =>
                setLook(
                  (look) =>
                    withLook(
                      look,
                      "range",
                      formatRange([zone.start, Math.max(zone.end, zone.start + 1)]),
                    ),
                  true,
                )
              }
            >
              {formatDuration(zone.end - zone.start)}
            </button>{" "}
            {zone.value !== null && zone.value !== undefined ? `${site.name}=${zone.value} ` : ""}
            <span className="dim">
              {thread(zone.thread)} at {formatDuration(zone.start)}
            </span>
          </li>
        ))}
      </ul>
      {site.label ? (
        <>
          <div className="pane-head">
            Values
            <button
              type="button"
              className="tb ghost"
              aria-pressed={look.bl === site.name}
              title="Root each stack at its value of this label"
              onClick={() =>
                setLook(
                  (look) => withLook(look, "bl", look.bl === site.name ? undefined : site.name),
                  true,
                )
              }
            >
              By {site.name}
            </button>
          </div>
          <ul className="zone-values">
            {stats.values.map((value) => {
              const label = `${site.name}=${value.value}`;
              const on = (look.lb ?? []).includes(label);
              return (
                <li key={value.value}>
                  <button
                    type="button"
                    className="link-button"
                    aria-pressed={on}
                    title="Keep only samples that carry this label"
                    onClick={() =>
                      setLook((look) => {
                        const kept = (look.lb ?? []).filter((each) => each !== label);
                        const next = on ? kept : [...kept, label];
                        return withLook(look, "lb", next.length > 0 ? next : undefined);
                      }, true)
                    }
                  >
                    {label}
                  </button>{" "}
                  <span className="dim">
                    {formatDuration(value.total)} in {value.count}
                  </span>
                </li>
              );
            })}
          </ul>
        </>
      ) : null}
    </div>
  );
}
