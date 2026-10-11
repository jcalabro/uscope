import { useVirtualizer } from "@tanstack/react-virtual";
import { useMemo, useRef, useState } from "react";
import { formatChange, formatPercent, shareChange } from "../../profile/paint";
import { type CallTree, decodeTree, nameOf } from "../../profile/tree";
import type { ProfileQuery } from "../../protocol";
import { useAnswer, useProfile } from "./context";
import { Details } from "./FlameView";

const ROW = 20;

/** The call tree of the query, with the selected function's details. */
export function TreeView() {
  const { query, look } = useProfile();
  const tree = useTree(query);
  return (
    <div className="pane grow">
      <div className="pane-head">
        {query.invert ? "Callers" : "Call tree"}
        <span className="end">total · own</span>
      </div>
      <TreeRows tree={tree} label="Call tree" />
      {tree.tree && look.fn ? <Details tree={tree.tree} name={look.fn} /> : null}
    </div>
  );
}

export interface Fetched {
  tree: CallTree | null;
  error: string | null;
  current: boolean;
}

export function useTree(query: ProfileQuery): Fetched {
  const answer = useAnswer("profileTree", query);
  const tree = useMemo(
    () => (answer.data ? decodeTree(answer.data, answer.data.payload) : null),
    [answer.data],
  );
  return { tree, error: answer.error?.message ?? null, current: answer.current };
}

/** The nodes a tree opens with: down the heaviest path while each child
 * holds at least half its caller's weight. */
function opening(tree: CallTree): Set<number> {
  const open = new Set<number>();
  for (let root = 0; root < tree.count; root = tree.end[root] ?? tree.count) {
    let node = root;
    while (node + 1 < (tree.end[node] ?? 0)) {
      open.add(node);
      const child = node + 1;
      if ((tree.total[child] ?? 0) * 2 < (tree.total[node] ?? 0)) {
        break;
      }
      node = child;
    }
  }
  return open;
}

/** A tree's rows, each node under its caller, opened and closed by its
 * twist; choosing a row selects its function. */
export function TreeRows({ tree: fetched, label }: { tree: Fetched; label: string }) {
  const { look, setLook } = useProfile();
  const tree = fetched.tree;
  const body = useRef<HTMLDivElement>(null);
  // Opened nodes belong to one tree; another answer opens its own.
  const [opened, setOpened] = useState<{ tree: CallTree | null; open: Set<number> }>({
    tree: null,
    open: new Set(),
  });
  const open = opened.tree === tree ? opened.open : tree ? opening(tree) : new Set<number>();
  const rows = useMemo(() => {
    const shown: number[] = [];
    if (!tree) {
      return shown;
    }
    for (let node = 0; node < tree.count; ) {
      shown.push(node);
      node = open.has(node) ? node + 1 : (tree.end[node] ?? node + 1);
    }
    return shown;
  }, [tree, open]);
  const virtual = useVirtualizer({
    count: rows.length,
    getScrollElement: () => body.current,
    estimateSize: () => ROW,
    overscan: 30,
  });
  if (fetched.error) {
    return <div className="empty">{fetched.error}</div>;
  }
  if (tree && tree.count === 0) {
    return <div className="empty">No samples are selected.</div>;
  }
  const toggle = (node: number) => {
    const next = new Set(open);
    if (!next.delete(node)) {
      next.add(node);
    }
    setOpened({ tree, open: next });
  };
  return (
    <div
      ref={body}
      className={`pane-body ${fetched.current ? "" : "stale"}`}
      role="tree"
      aria-label={label}
    >
      <div className="tree-rows" style={{ height: virtual.getTotalSize() }}>
        {tree
          ? virtual.getVirtualItems().map((item) => {
              const node = rows[item.index] ?? 0;
              const name = nameOf(tree, node);
              const parent = node + 1 < (tree.end[node] ?? 0);
              return (
                <div
                  key={node}
                  role="treeitem"
                  aria-level={(tree.depth[node] ?? 0) + 1}
                  aria-expanded={parent ? open.has(node) : undefined}
                  aria-selected={look.fn === name}
                  className={`row tree-row ${look.fn === name ? "selected" : ""}`}
                  style={{ transform: `translateY(${item.start}px)` }}
                  onClick={() => setLook((look) => ({ ...look, fn: name }))}
                  onKeyDown={(event) => {
                    if (event.key === "Enter") {
                      toggle(node);
                    }
                  }}
                  tabIndex={-1}
                >
                  <span className="percent">
                    {formatPercent(tree.total[node] ?? 0, tree.weight)}
                  </span>
                  <span className="percent dim">
                    {formatPercent(tree.own[node] ?? 0, tree.weight)}
                  </span>
                  {tree.baseTotal ? (
                    <Change
                      points={shareChange(
                        tree.total[node] ?? 0,
                        tree.weight,
                        tree.baseTotal[node] ?? 0,
                        tree.baseWeight ?? 0,
                      )}
                    />
                  ) : null}
                  <span style={{ width: (tree.depth[node] ?? 0) * 12, flex: "none" }} />
                  <button
                    type="button"
                    className="twist"
                    aria-label={open.has(node) ? "Close" : "Open"}
                    style={{ visibility: parent ? "visible" : "hidden" }}
                    onClick={(event) => {
                      event.stopPropagation();
                      toggle(node);
                    }}
                  >
                    {open.has(node) ? "▾" : "▸"}
                  </button>
                  <span className="name" title={name}>
                    {name}
                  </span>
                </div>
              );
            })
          : null}
      </div>
    </div>
  );
}

/** A share's change from a diff's base, colored as the flame graph is. */
export function Change({ points }: { points: number | null }) {
  const way = points === null || Math.abs(points) < 0.005 ? "" : points > 0 ? "up" : "down";
  return (
    <span className={`percent change ${way}`} title="Change in share from the base">
      {formatChange(points)}
    </span>
  );
}
