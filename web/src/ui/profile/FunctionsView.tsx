import { useVirtualizer } from "@tanstack/react-virtual";
import { useMemo, useRef, useState } from "react";
import { withLook } from "../../profile/look";
import { formatPercent, formatWeight, shareChange } from "../../profile/paint";
import type { ProfileQuery } from "../../protocol";
import { useAnswer, useProfile } from "./context";
import { Change, TreeRows, useTree } from "./TreeView";

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
  const base = answer.data?.base ?? null;
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
                by {each === "own" && base ? "change" : each}
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
                    {base ? (
                      <Change points={shareChange(row.own, weight, row.baseOwn, base.weight)} />
                    ) : null}
                    <span className="name">{key.name}</span>
                    {key.module ? <span className="end">{key.module}</span> : null}
                  </div>
                );
              })}
            </div>
          </div>
        )}
      </div>
      {look.fn ? <Selected name={look.fn} /> : null}
    </div>
  );
}

const PANELS = [
  [undefined, "Callers and callees"],
  ["lines", "Source"],
  ["code", "Assembly"],
] as const;

/** The selected function: its callers and callees, or its samples on its
 * source lines or on its instructions. */
function Selected({ name }: { name: string }) {
  const { query, look, setLook } = useProfile();
  return (
    <div className="profile-selected">
      <div className="pane-head">
        <div className="segmented">
          {PANELS.map(([ann, label]) => (
            <button
              key={label}
              type="button"
              aria-pressed={look.ann === ann}
              onClick={() => setLook((look) => withLook(look, "ann", ann), true)}
            >
              {label}
            </button>
          ))}
        </div>
        {look.ann && query.transforms.length > 0 ? (
          <span className="muted">Every selected sample counts here, without the transforms.</span>
        ) : null}
      </div>
      {look.ann === "lines" ? (
        <SourceLines name={name} />
      ) : look.ann === "code" ? (
        <Code name={name} />
      ) : (
        <Sandwich query={query} name={name} />
      )}
    </div>
  );
}

/** A function's samples on its source lines, by file, as each reads now. */
function SourceLines({ name }: { name: string }) {
  const { query } = useProfile();
  const answer = useAnswer(
    "profileLines",
    useMemo(() => ({ ...query, function: name }), [query, name]),
  );
  if (answer.error) {
    return <div className="empty">{answer.error.message}</div>;
  }
  const weight = answer.data?.weight ?? 0;
  return (
    <div
      className={`pane-body annotated ${answer.current ? "" : "stale"}`}
      data-testid="source-lines"
    >
      {(answer.data?.files ?? []).map((file) => {
        const costs = new Map(file.lines.map(([line, own, total]) => [line, [own, total]]));
        return (
          <div key={file.path ?? ""} className="annotated-file">
            <div className="annotated-head mono">{file.path ?? "no file"}</div>
            {file.text ? (
              file.text.map((text, at) => {
                const line = file.first + at;
                const cost = costs.get(line);
                return (
                  <div key={line} className={`row annotated-row ${cost ? "hot" : ""}`}>
                    <span className="percent">
                      {cost ? formatPercent(cost[0] ?? 0, weight) : ""}
                    </span>
                    <span className="percent dim">
                      {cost ? formatPercent(cost[1] ?? 0, weight) : ""}
                    </span>
                    <span className="line-number">{line}</span>
                    <code>{text}</code>
                  </div>
                );
              })
            ) : (
              <div className="empty">{file.unread}</div>
            )}
          </div>
        );
      })}
    </div>
  );
}

/** A function's samples on its instructions, or why its code is unknown. */
function Code({ name }: { name: string }) {
  const { query } = useProfile();
  const answer = useAnswer(
    "profileCode",
    useMemo(() => ({ ...query, function: name }), [query, name]),
  );
  if (answer.error) {
    return <div className="empty">{answer.error.message}</div>;
  }
  const code = answer.data;
  if (code?.unavailable) {
    return <div className="empty">{code.unavailable}</div>;
  }
  const weight = code?.weight ?? 0;
  return (
    <div className={`pane-body annotated ${answer.current ? "" : "stale"}`} data-testid="code">
      {(code?.blocks ?? []).map((block) => (
        <div key={block.symbol} className="annotated-file">
          <div className="annotated-head mono">
            {block.symbol} <span className="muted">{code?.module}</span>
          </div>
          {block.instructions.map((instruction) => (
            <div
              key={instruction.address}
              className={`row annotated-row ${instruction.total > 0 ? "hot" : ""}`}
            >
              <span className="percent">
                {instruction.own > 0 ? formatPercent(instruction.own, weight) : ""}
              </span>
              <span className="percent dim">
                {instruction.total > 0 ? formatPercent(instruction.total, weight) : ""}
              </span>
              <span className="line-number">{instruction.address.toString(16)}</span>
              <code>{instruction.text}</code>
            </div>
          ))}
          {block.stopped ? <div className="empty">{block.stopped}</div> : null}
        </div>
      ))}
      {(code?.unplaced.length ?? 0) > 0 ? (
        <div className="empty">
          {code?.unplaced.length} sampled addresses lie outside every decoded instruction.
        </div>
      ) : null}
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
