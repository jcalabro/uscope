import { useVirtualizer } from "@tanstack/react-virtual";
import { useMemo, useRef, useState } from "react";
import { formatPercent, formatWeight } from "../../profile/paint";
import type { ProfileQuery } from "../../protocol";
import { useAnswer, useProfile } from "./context";
import { TreeRows, useTree } from "./TreeView";

const ROW = 20;

type Order = "own" | "total";

/** Every function's cost, and for the selected one a sandwich: its
 * callers above and its callees below, merged from every call. */
export function FunctionsView() {
  const { query, look, unit, setLook } = useProfile();
  const answer = useAnswer("profileFunctions", query);
  const [order, setOrder] = useState<Order>("own");
  const body = useRef<HTMLDivElement>(null);
  const rows = useMemo(() => {
    const all = answer.data?.rows ?? [];
    return order === "own" ? all : [...all].sort((a, b) => b.total - a.total || b.own - a.own);
  }, [answer.data, order]);
  const virtual = useVirtualizer({
    count: rows.length,
    getScrollElement: () => body.current,
    estimateSize: () => ROW,
    overscan: 30,
  });
  const weight = answer.data?.weight ?? 0;
  const keys = answer.data?.keys ?? [];
  return (
    <div className="profile-functions">
      <div className="pane grow">
        <div className="pane-head">
          Functions
          <span className="count">{rows.length}</span>
          <div className="segmented">
            {(["own", "total"] as const).map((each) => (
              <button
                key={each}
                type="button"
                aria-pressed={order === each}
                onClick={() => setOrder(each)}
              >
                by {each}
              </button>
            ))}
          </div>
        </div>
        {answer.error ? (
          <div className="empty">{answer.error.message}</div>
        ) : (
          <div
            ref={body}
            className={`pane-body ${answer.current ? "" : "stale"}`}
            role="listbox"
            aria-label="Functions"
          >
            <div className="tree-rows" style={{ height: virtual.getTotalSize() }}>
              {virtual.getVirtualItems().map((item) => {
                const row = rows[item.index];
                const key = row ? keys[row.key] : undefined;
                if (!row || !key) {
                  return null;
                }
                const selected = look.fn === key.name;
                return (
                  <div
                    key={key.name}
                    role="option"
                    tabIndex={-1}
                    aria-selected={selected}
                    className={`row tree-row ${selected ? "selected" : ""}`}
                    style={{ transform: `translateY(${item.start}px)` }}
                    title={formatWeight(row.total, unit)}
                    onClick={() => setLook((look) => ({ ...look, fn: key.name }))}
                    onKeyDown={(event) => {
                      if (event.key === "Enter") {
                        setLook((look) => ({ ...look, fn: key.name }));
                      }
                    }}
                  >
                    <span className="percent">{formatPercent(row.own, weight)}</span>
                    <span className="percent dim">{formatPercent(row.total, weight)}</span>
                    <span className="name">{key.name}</span>
                    {key.module ? <span className="end">{key.module}</span> : null}
                  </div>
                );
              })}
            </div>
          </div>
        )}
      </div>
      {look.fn ? <Sandwich query={query} name={look.fn} /> : null}
    </div>
  );
}

/** A function's callers, inverted from it, and its callees, from it. */
function Sandwich({ query, name }: { query: ProfileQuery; name: string }) {
  const focused = useMemo(
    () => ({ ...query, transforms: [...query.transforms, { op: "focus" as const, name }] }),
    [query, name],
  );
  const callers = useTree(useMemo(() => ({ ...focused, invert: !query.invert }), [focused, query]));
  const callees = useTree(focused);
  return (
    <div className="profile-sandwich" data-testid="sandwich">
      <div className="pane grow">
        <div className="pane-head">Callers of {name}</div>
        <TreeRows tree={callers} label="Callers" />
      </div>
      <div className="pane grow">
        <div className="pane-head">Callees of {name}</div>
        <TreeRows tree={callees} label="Callees" />
      </div>
    </div>
  );
}
