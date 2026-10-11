import { useEffect, useMemo, useRef, useState } from "react";
import { transformed, withLook } from "../../profile/look";
import { fill, formatPercent, formatWeight } from "../../profile/paint";
import {
  type CallTree,
  decodeTree,
  findPath,
  hit,
  layout,
  nameOf,
  pathOf,
  type Rect,
} from "../../profile/tree";
import type { TransformOp } from "../../protocol";
import { isDark, token, useAnswer, useProfile } from "./context";

/** One row of the flame graph, in CSS pixels. */
const ROW = 18;

/** The flame graph, roots on top, with a search and the selected node's
 * details. Clicking a frame zooms to it. */
export function FlameView() {
  const { query, look, setLook } = useProfile();
  const answer = useAnswer("profileTree", query);
  const tree = useMemo(
    () => (answer.data ? decodeTree(answer.data, answer.data.payload) : null),
    [answer.data],
  );
  const [search, setSearch] = useState("");
  const zoom = tree && look.z ? findPath(tree, look.z) : null;
  const matched = useMemo(() => (tree && search ? matching(tree, search) : null), [tree, search]);

  return (
    <div className="pane grow">
      <div className="pane-head">
        Flame graph
        {zoom !== null ? (
          <button
            type="button"
            className="link-button"
            onClick={() => setLook((look) => withLook(look, "z", undefined), true)}
          >
            Reset zoom
          </button>
        ) : null}
        <input
          className="input profile-search"
          placeholder="Search"
          aria-label="Search frames"
          value={search}
          onChange={(event) => setSearch(event.target.value)}
        />
        {matched && tree ? (
          <span className="count" data-testid="matched">
            matched {formatPercent(matched.weight, tree.weight)} of samples
          </span>
        ) : null}
      </div>
      <div className={`pane-body ${answer.current ? "" : "stale"}`}>
        {answer.error ? (
          <div className="empty">{answer.error.message}</div>
        ) : tree ? (
          tree.count === 0 ? (
            <div className="empty">No samples are selected.</div>
          ) : (
            <FlameGraph
              tree={tree}
              zoom={zoom}
              matched={matched?.nodes ?? null}
              onZoom={(node) =>
                setLook(
                  (look) => ({
                    ...withLook(look, "z", pathOf(tree, node)),
                    fn: nameOf(tree, node),
                  }),
                  true,
                )
              }
            />
          )
        ) : null}
      </div>
      {tree && look.fn ? <Details tree={tree} name={look.fn} /> : null}
    </div>
  );
}

/** The nodes whose names hold `search`, and the weight under them,
 * counting nested matches once. */
function matching(tree: CallTree, search: string) {
  const needle = search.toLowerCase();
  const nodes = new Set<number>();
  let weight = 0;
  for (let node = 0; node < tree.count; ) {
    if (nameOf(tree, node).toLowerCase().includes(needle)) {
      weight += tree.total[node] ?? 0;
      for (let inner = node; inner < (tree.end[node] ?? node + 1); inner += 1) {
        if (nameOf(tree, inner).toLowerCase().includes(needle)) {
          nodes.add(inner);
        }
      }
      node = tree.end[node] ?? node + 1;
    } else {
      node += 1;
    }
  }
  return { nodes, weight };
}

function FlameGraph({
  tree,
  zoom,
  matched,
  onZoom,
}: {
  tree: CallTree;
  zoom: number | null;
  matched: Set<number> | null;
  onZoom: (node: number) => void;
}) {
  const { unit } = useProfile();
  const box = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const [width, setWidth] = useState(0);
  const [hovered, setHovered] = useState<{ rect: Rect; left: number; top: number } | null>(null);

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

  const rects = useMemo(
    () => (width > 0 ? layout(tree, zoom, 0.5 / width) : []),
    [tree, zoom, width],
  );
  const depth = rects.reduce((deepest, rect) => Math.max(deepest, rect.depth + 1), 0);
  const height = depth * ROW;

  useEffect(() => {
    const element = canvas.current;
    const context = element?.getContext("2d");
    if (!element || !context || width === 0) {
      return;
    }
    const ratio = window.devicePixelRatio || 1;
    element.width = Math.round(width * ratio);
    element.height = Math.round(height * ratio);
    context.setTransform(ratio, 0, 0, ratio, 0, 0);
    context.clearRect(0, 0, width, height);
    const dark = isDark();
    const ink = token("--ink");
    const surface = token("--surface");
    context.font = `11px ${token("--mono")}`;
    context.textBaseline = "middle";
    for (const rect of rects) {
      const x = rect.x * width;
      const w = rect.width * width;
      const y = rect.depth * ROW;
      const key = tree.keys[tree.key[rect.node] ?? 0];
      context.globalAlpha = matched && !matched.has(rect.node) ? 0.35 : 1;
      context.fillStyle = key ? fill(key, dark) : surface;
      context.fillRect(x, y, Math.max(w - 1, 0.5), ROW - 1);
      if (w > 28) {
        context.fillStyle = ink;
        context.fillText(clip(context, nameOf(tree, rect.node), w - 8), x + 4, y + ROW / 2);
      }
    }
    context.globalAlpha = 1;
  }, [rects, tree, width, height, matched]);

  const at = (event: React.MouseEvent) => {
    const bounds = event.currentTarget.getBoundingClientRect();
    const x = event.clientX - bounds.left;
    const y = event.clientY - bounds.top;
    const rect = hit(rects, Math.floor(y / ROW), x / bounds.width);
    return rect ? { rect, left: x, top: y } : null;
  };
  const shown = hovered ? hovered.rect.node : null;
  return (
    <div ref={box} className="flame" style={{ height }}>
      <canvas
        ref={canvas}
        style={{ width: "100%", height }}
        role="img"
        aria-label="Flame graph"
        onMouseMove={(event) => setHovered(at(event))}
        onMouseLeave={() => setHovered(null)}
        onClick={(event) => {
          const found = at(event);
          if (found) {
            onZoom(found.rect.node);
          }
        }}
      />
      {hovered && shown !== null ? (
        <div
          className="flame-tip"
          data-testid="flame-tip"
          style={{
            left: Math.min(hovered.left + 12, Math.max(width - 320, 0)),
            top: hovered.top + 16,
          }}
        >
          <div className="mono">{nameOf(tree, shown)}</div>
          <div className="muted">
            {formatWeight(tree.total[shown] ?? 0, unit)} ·{" "}
            {formatPercent(tree.total[shown] ?? 0, tree.weight)} total,{" "}
            {formatPercent(tree.own[shown] ?? 0, tree.weight)} own
          </div>
        </div>
      ) : null}
    </div>
  );
}

/** The longest start of `text`, with an ellipsis, that fits `width`. */
function clip(context: CanvasRenderingContext2D, text: string, width: number): string {
  if (context.measureText(text).width <= width) {
    return text;
  }
  let low = 0;
  let high = text.length;
  while (low < high) {
    const middle = Math.ceil((low + high) / 2);
    if (context.measureText(`${text.slice(0, middle)}…`).width <= width) {
      low = middle;
    } else {
      high = middle - 1;
    }
  }
  return low > 0 ? `${text.slice(0, low)}…` : "";
}

const ACTIONS: { op: TransformOp; label: string; title: string }[] = [
  { op: "focus", label: "Focus", title: "Only samples through it, rooted at it" },
  { op: "merge", label: "Merge", title: "Hide it, giving its cost to its callers" },
  { op: "drop", label: "Drop", title: "Remove every sample through it" },
  { op: "collapse", label: "Collapse recursion", title: "Merge its calls of itself" },
];

/** The selected function: what it costs under the query, and what can be
 * done with it. */
export function Details({ tree, name }: { tree: CallTree; name: string }) {
  const { unit, setLook } = useProfile();
  // A function's cost in a tree counts it once per sample.
  let total = 0;
  let own = 0;
  for (let node = 0; node < tree.count; ) {
    if (nameOf(tree, node) === name) {
      total += tree.total[node] ?? 0;
      for (let inner = node; inner < (tree.end[node] ?? node + 1); inner += 1) {
        own += nameOf(tree, inner) === name ? (tree.own[inner] ?? 0) : 0;
      }
      node = tree.end[node] ?? node + 1;
    } else {
      node += 1;
    }
  }
  const key = tree.keys.find((each) => each.name === name);
  return (
    <div className="profile-details" data-testid="details">
      <span className="mono name" title={name}>
        {name}
      </span>
      {key?.file ? (
        <span className="muted mono">
          {key.file.slice(key.file.lastIndexOf("/") + 1)}:{key.line}
        </span>
      ) : null}
      <span className="muted">
        {formatWeight(total, unit)} · {formatPercent(total, tree.weight)} total,{" "}
        {formatPercent(own, tree.weight)} own
      </span>
      <span className="spacer" />
      {ACTIONS.map(({ op, label, title }) => (
        <button
          key={op}
          type="button"
          className="tb ghost"
          title={title}
          onClick={() => setLook((look) => transformed(look, { op, name }), true)}
        >
          {label}
        </button>
      ))}
      <button
        type="button"
        className="tb ghost"
        title="Its callers and callees"
        onClick={() =>
          setLook((look) => ({ ...withLook(look, "view", "functions"), fn: name }), true)
        }
      >
        Callers and callees
      </button>
    </div>
  );
}
