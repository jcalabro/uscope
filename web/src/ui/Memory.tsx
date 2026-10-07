// Memory at the address the link names, sixteen bytes to a row. The bytes
// of the value it was opened from are marked, a selection reads as the
// types its length could be, and controllers write bytes in place.

import { useState } from "react";
import { cache, useRequest } from "../data";
import { isAddress } from "../focus";
import {
  dumpRows,
  hex,
  target as parseTarget,
  printable,
  ROW,
  readAs,
  rowStart,
  type Target,
} from "../memory";
import { controls } from "../model";
import type { Row } from "../protocol";
import { useConnection, useModel } from "../store";
import { useLook } from "./navigation";
import { hidden } from "./Values";
import { useFocus } from "./Workspace";

/** Rows shown at once. */
const ROWS = 16;

export function Memory() {
  const { look } = useFocus();
  const shown = look.mem ? parseTarget(look.mem) : null;
  return (
    <div className="mem">
      <div className="view-head">
        <GoTo />
        {shown && (
          <span className="dim">
            {hex(shown.address)}
            {shown.bytes ? ` · ${shown.bytes} bytes` : ""}
          </span>
        )}
      </div>
      {shown ? (
        // A new address starts over: at its row, with its bytes selected.
        <Dump key={look.mem} shown={shown} />
      ) : (
        <div className="empty center-message">
          Type an address or an expression above, or open a value's memory from its row.
        </div>
      )}
    </div>
  );
}

/** Goes to an address, or to the memory an expression's value names. */
function GoTo() {
  const { at, stale } = useFocus();
  const look = useLook();
  const connection = useConnection();
  const [text, setText] = useState("");
  const [error, setError] = useState<string | null>(null);
  const go = (mem: string) => {
    look((current) => ({ ...current, mem, view: "memory" }), { replace: false });
    setText("");
  };
  return (
    <form
      className="goto"
      onSubmit={(event) => {
        event.preventDefault();
        const wanted = text.trim();
        setError(null);
        if (isAddress(wanted)) {
          go(wanted.toLowerCase());
          return;
        }
        if (!at || stale) {
          setError("Expressions are read at the stop shown, while it is the program's");
          return;
        }
        cache.get(connection, "evaluate", { ...at, expression: wanted }).promise.then((settled) => {
          const row = settled.ok ? (settled.value as Row) : null;
          if (!settled.ok) {
            setError(settled.error.message);
          } else if (!row?.memory) {
            setError(`${wanted} names no memory`);
          } else {
            go(row.memoryBytes ? `${row.memory}:${row.memoryBytes}` : row.memory);
          }
        });
      }}
    >
      <input
        className="input quiet"
        aria-label="Go to memory"
        placeholder="0xaddress or an expression"
        value={text}
        onChange={(event) => setText(event.target.value)}
      />
      {error && <span className="error">{error}</span>}
    </form>
  );
}

interface Selection {
  from: bigint;
  to: bigint;
}

function Dump({ shown }: { shown: Target }) {
  const focus = useFocus();
  const { at } = focus;
  const why = hidden(focus, "Bytes");
  const [moved, setMoved] = useState(0);
  const value = shown.bytes
    ? { from: shown.address, to: shown.address + BigInt(shown.bytes - 1) }
    : null;
  const [selection, setSelection] = useState<Selection | null>(value);
  const [editing, setEditing] = useState<bigint | null>(null);

  const first = rowStart(shown.address) + BigInt(moved * ROW);
  const start = first < 0n ? 0n : first;
  const read = useRequest(
    "readMemory",
    why === null && at ? { stop: at.stop, address: hex(start), count: ROWS * ROW } : null,
  );
  if (why) {
    return <div className="empty center-message">{why}</div>;
  }
  if (read.error) {
    return <div className="empty center-message error">{read.error.message}</div>;
  }
  if (!read.data) {
    return <div className="empty center-message">Reading memory…</div>;
  }
  const rows = dumpRows(start, read.data.bytes, ROWS);
  const within = (range: Selection | null, address: bigint) =>
    range !== null && address >= range.from && address <= range.to;
  const choose = (address: bigint, extend: boolean) =>
    setSelection((current) =>
      extend && current
        ? {
            from: address < current.from ? address : current.from,
            to: address > current.to ? address : current.to,
          }
        : { from: address, to: address },
    );

  return (
    <>
      <div className="mem-pages">
        <button type="button" className="button small" onClick={() => setMoved(moved - ROWS)}>
          ↑ Earlier
        </button>
        <button type="button" className="button small" onClick={() => setMoved(moved + ROWS)}>
          Later ↓
        </button>
        {read.data.unreadable && (
          <span className="dim">Nothing can be read from {read.data.unreadable} on.</span>
        )}
      </div>
      <div className={`mem-body ${read.current ? "" : "stale"}`} data-testid="memory">
        {rows.map((row) => (
          <div key={String(row.address)} className="mem-row">
            <span className="mem-address">{row.address.toString(16).padStart(12, "0")}</span>
            <span className="mem-bytes">
              {row.bytes.map((byte, column) => {
                const address = row.address + BigInt(column);
                if (editing === address) {
                  return (
                    <ByteEditor
                      key={String(address)}
                      address={address}
                      done={() => setEditing(null)}
                    />
                  );
                }
                return (
                  <ByteCell
                    key={String(address)}
                    byte={byte}
                    className={`${within(value, address) ? "in-value" : ""} ${within(selection, address) ? "selected" : ""}`}
                    onSelect={(extend) => choose(address, extend)}
                    onEdit={() => setEditing(address)}
                  />
                );
              })}
            </span>
            <span className="mem-text">{row.bytes.map(printable).join("")}</span>
          </div>
        ))}
      </div>
      <Readings rows={rows} selection={selection} />
    </>
  );
}

function ByteCell({
  byte,
  className,
  onSelect,
  onEdit,
}: {
  byte: number | null;
  className: string;
  onSelect(extend: boolean): void;
  onEdit(): void;
}) {
  const { stale } = useFocus();
  const control = useModel(controls);
  if (byte === null) {
    return <span className="byte unread">??</span>;
  }
  return (
    <button
      type="button"
      className={`byte ${className}`}
      tabIndex={-1}
      onClick={(event) => onSelect(event.shiftKey)}
      onDoubleClick={() => control && !stale && onEdit()}
    >
      {byte.toString(16).padStart(2, "0")}
    </button>
  );
}

/** Writes bytes from `address` on: one pair, or several. */
function ByteEditor({ address, done }: { address: bigint; done(): void }) {
  const { at } = useFocus();
  const connection = useConnection();
  const [error, setError] = useState<string | null>(null);
  return (
    <form
      className="byte-edit"
      onSubmit={(event) => {
        event.preventDefault();
        const bytes = String(new FormData(event.currentTarget).get("bytes") ?? "").replaceAll(
          /\s/g,
          "",
        );
        if (!at || !/^([0-9a-f]{2})+$/i.test(bytes)) {
          setError("pairs of hexadecimal digits");
          return;
        }
        connection
          .request("writeMemory", { stop: at.stop, address: hex(address), bytes })
          .then(done)
          .catch((failure: Error) => setError(failure.message));
      }}
    >
      <input
        className="input"
        name="bytes"
        aria-label={`New byte at ${hex(address)}`}
        title={error ?? "hexadecimal pairs; several write the bytes after it too"}
        aria-invalid={error !== null}
        // biome-ignore lint/a11y/noAutofocus: the person asked to edit this byte
        autoFocus
        onKeyDown={(event) => {
          if (event.key === "Escape") {
            event.stopPropagation();
            done();
          }
        }}
        onBlur={() => error === null && done()}
      />
    </form>
  );
}

/** The selected bytes, read as the types their length could be. */
function Readings({
  rows,
  selection,
}: {
  rows: ReturnType<typeof dumpRows>;
  selection: Selection | null;
}) {
  if (!selection) {
    return (
      <div className="mem-selection dim" data-testid="selection">
        Click a byte, or shift-click to select several.
      </div>
    );
  }
  const bytes: (number | null)[] = [];
  for (const row of rows) {
    for (const [column, byte] of row.bytes.entries()) {
      const address = row.address + BigInt(column);
      if (address >= selection.from && address <= selection.to) {
        bytes.push(byte);
      }
    }
  }
  const count = Number(selection.to - selection.from) + 1;
  const whole = bytes.length === count && bytes.every((byte) => byte !== null);
  return (
    <div className="mem-selection" data-testid="selection">
      <span className="dim">
        {count} {count === 1 ? "byte" : "bytes"} at {hex(selection.from)}
      </span>
      {whole ? (
        readAs(bytes as number[]).map((reading) => (
          <span key={reading.as} className="reading">
            <span className="dim">{reading.as}</span> {reading.text}
          </span>
        ))
      ) : (
        <span className="dim">not all of it is shown or readable</span>
      )}
    </div>
  );
}
