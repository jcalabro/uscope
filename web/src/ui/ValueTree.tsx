// Values as a tree. Rows are named by path, so what is open survives the
// next stop: the link lists open paths, and the same paths open again in
// the new stop's rows. A value that changed since the stop before is marked,
// with what it was.

import { createContext, type ReactNode, useContext, useState } from "react";
import { useRequest } from "../data";
import { controls } from "../model";
import type { Children, Row } from "../protocol";
import { useConnection, useModel } from "../store";
import { childKey, recall, toggled } from "../tree";
import { useLook } from "./navigation";
import { useFocus } from "./Workspace";

/** How many children one page shows. */
const PAGE = 100;

/** Which rows are open, and whether changes are marked. */
export interface Expansion {
  isOpen(key: string): boolean;
  toggle(key: string): void;
  /** Whether to compare values with the stop before. */
  remember: boolean;
}

const ExpansionContext = createContext<Expansion | null>(null);

/** The rows the link opens: Variables and Watch. */
export function LinkedExpansion({ children }: { children: ReactNode }) {
  const { look } = useFocus();
  const change = useLook();
  const expansion: Expansion = {
    isOpen: (key) => look.x?.includes(key) ?? false,
    toggle: (key) => change((current) => withOpen(current, toggled(current.x, key))),
    remember: true,
  };
  return <ExpansionContext value={expansion}>{children}</ExpansionContext>;
}

/**
 * Rows still showing an earlier answer while this stop's arrives: they are
 * not this stop's values, so nothing compares with them.
 */
export function Earlier({ children, when }: { children: ReactNode; when: boolean }) {
  const expansion = useExpansion();
  if (!when) {
    return children;
  }
  return <ExpansionContext value={{ ...expansion, remember: false }}>{children}</ExpansionContext>;
}

function withOpen<T extends { x?: string[] }>(look: T, x: string[] | undefined): T {
  const { x: _x, ...rest } = look;
  return (x ? { ...rest, x } : rest) as T;
}

/** Rows opened here alone, such as the console's. */
export function LocalExpansion({ children }: { children: ReactNode }) {
  const [open, setOpen] = useState<string[] | undefined>();
  const expansion: Expansion = {
    isOpen: (key) => open?.includes(key) ?? false,
    toggle: (key) => setOpen((current) => toggled(current, key)),
    remember: false,
  };
  return <ExpansionContext value={expansion}>{children}</ExpansionContext>;
}

function useExpansion(): Expansion {
  const expansion = useContext(ExpansionContext);
  if (!expansion) {
    throw new Error("value rows need an expansion");
  }
  return expansion;
}

/** A list of rows under the path `parent`. */
export function RowList({
  rows,
  parent,
  depth,
}: {
  rows: readonly Row[];
  parent: string;
  depth: number;
}) {
  return (
    <>
      {rows.map((row, index) => (
        <ValueRow
          // Names can repeat, as shadowed variables do; the index keeps them apart.
          // biome-ignore lint/suspicious/noArrayIndexKey: rows have no other identity
          key={`${index}:${row.name}`}
          row={row}
          parent={parent}
          depth={depth}
        />
      ))}
    </>
  );
}

export interface ValueRowProps {
  row: Row;
  parent: string;
  depth: number;
  /** Controls at the row's end, such as removing a watch. */
  actions?: ReactNode;
  /** The row says why there is no value. */
  failed?: boolean;
}

/** One value, which opens to its children. */
export function ValueRow({ row, parent, depth, actions, failed = false }: ValueRowProps) {
  const { at, stale } = useFocus();
  const expansion = useExpansion();
  const key = childKey(parent, row.name);
  const open = row.children !== null && expansion.isOpen(key);
  let was: string | undefined;
  if (expansion.remember && at && !stale && !row.truncated && !failed) {
    // Rendering notes what this stop shows, for the stop after it.
    was = recall.before(at.stop, key);
    recall.note(at.stop, key, row.text);
  }
  const changed = was !== undefined && was !== row.text;

  return (
    <li className="value">
      <div
        className={`value-row ${row.truncated ? "truncated" : ""}`}
        style={{ paddingLeft: 6 + depth * 14 }}
      >
        {row.children ? (
          <button
            type="button"
            className="twist"
            aria-label={`${open ? "Close" : "Open"} ${row.name}`}
            aria-expanded={open}
            onClick={() => expansion.toggle(key)}
          >
            {open ? "▾" : "▸"}
          </button>
        ) : (
          <span className="twist" />
        )}
        <span className="value-name" title={row.path ?? row.name}>
          {row.name}
        </span>
        <ValueText row={row} failed={failed} changed={changed} was={was} />
        {row.type && <span className="value-type">{row.type}</span>}
        {actions}
      </div>
      {open && row.children && (
        <ul className="value-list">
          <ChildRows of={row.children} parent={key} depth={depth + 1} />
        </ul>
      )}
    </li>
  );
}

/** A value's text, which a double click edits when the value can change. */
function ValueText({
  row,
  failed,
  changed,
  was,
}: {
  row: Row;
  failed: boolean;
  changed: boolean;
  was: string | undefined;
}) {
  const { at, stale } = useFocus();
  const control = useModel(controls);
  const connection = useConnection();
  const [editing, setEditing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const editable = row.editable && row.path !== null && control && at !== null && !stale;

  if (editing && at && row.path) {
    const path = row.path;
    return (
      <form
        className="value-edit"
        onSubmit={(event) => {
          event.preventDefault();
          const value = new FormData(event.currentTarget).get("value");
          if (typeof value !== "string" || value.trim() === "") {
            return;
          }
          setError(null);
          connection
            .request("setValue", { ...at, path, value })
            .then(() => setEditing(false))
            .catch((failure: Error) => setError(failure.message));
        }}
      >
        <input
          className="input"
          name="value"
          aria-label={`New value of ${row.name}`}
          defaultValue={row.text}
          // biome-ignore lint/a11y/noAutofocus: the person asked to edit this value
          autoFocus
          onKeyDown={(event) => {
            if (event.key === "Escape") {
              event.stopPropagation();
              setEditing(false);
            }
          }}
          onBlur={() => !error && setEditing(false)}
        />
        {error && <span className="error">{error}</span>}
      </form>
    );
  }
  const className = `value-text ${failed ? "error" : ""} ${changed ? "changed" : ""}`;
  if (!editable) {
    return (
      <span className={className} title={changed ? `was ${was}` : row.text}>
        {row.text}
        {changed && <span className="was"> was {was}</span>}
      </span>
    );
  }
  return (
    <button
      type="button"
      className={`${className} editable`}
      title={`${changed ? `was ${was}; ` : ""}double-click to change`}
      onDoubleClick={() => setEditing(true)}
      onKeyDown={(event) => {
        if (event.key === "Enter" || event.key === "F2") {
          event.preventDefault();
          setEditing(true);
        }
      }}
    >
      {row.text}
      {changed && <span className="was"> was {was}</span>}
    </button>
  );
}

/** A value's children, a page at a time. */
function ChildRows({ of, parent, depth }: { of: Children; parent: string; depth: number }) {
  const [pages, setPages] = useState(1);
  const known = of.indexed !== null || of.named !== null;
  const total = (of.indexed ?? 0) + (of.named ?? 0);
  const shown = Math.min(pages * PAGE, known ? total : PAGE);
  const starts = Array.from({ length: Math.ceil(shown / PAGE) }, (_, index) => index * PAGE);
  return (
    <>
      {starts.map((start) => (
        <ChildPage key={start} handle={of.handle} start={start} parent={parent} depth={depth} />
      ))}
      {known && shown < total && (
        <li className="value">
          <button
            type="button"
            className="more"
            style={{ marginLeft: 6 + depth * 14 }}
            onClick={() => setPages(pages + 1)}
          >
            Show {Math.min(PAGE, total - shown)} more of {total.toLocaleString()}
          </button>
        </li>
      )}
    </>
  );
}

function ChildPage({
  handle,
  start,
  parent,
  depth,
}: {
  handle: number;
  start: number;
  parent: string;
  depth: number;
}) {
  const { data, error, current } = useRequest("children", { handle, start, count: PAGE });
  if (error) {
    return (
      <li className="value">
        <div className="value-row" style={{ paddingLeft: 6 + depth * 14 }}>
          <span className="value-text error">{error.message}</span>
        </div>
      </li>
    );
  }
  if (!data) {
    return (
      <li className="value">
        <div className="value-row dim" style={{ paddingLeft: 6 + depth * 14 }}>
          …
        </div>
      </li>
    );
  }
  return (
    <Earlier when={!current}>
      <RowList rows={data.rows} parent={parent} depth={depth} />
    </Earlier>
  );
}

/** A row that only says something, such as why a watch failed. */
export function messageRow(name: string, text: string): Row {
  return {
    name,
    text,
    type: null,
    path: null,
    children: null,
    editable: false,
    memory: null,
    truncated: false,
  };
}
