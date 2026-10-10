// The Drawings view: each argument and local of the frame shown that a
// view draws, then each value the link pins, drawn by its renderer in the
// sandbox. A card keeps its drawing while the program runs, dimmed and
// labelled with its stop, and draws again at the next stop it shows.

import { useEffect, useId, useMemo, useRef, useState } from "react";
import { base } from "../base";
import { useRequest } from "../data";
import { type At, drawing, formatPinned, type Look, type Pinned, parsePinned } from "../focus";
import type { Drawing, Row } from "../protocol";
import { flash } from "../tab";
import { useChosenTheme } from "../theme";
import { decodeInputs, type Value } from "../visualize/decode";
import { type Picture, PictureError, validate } from "../visualize/picture";
import {
  currentTheme,
  type Outcome,
  palette,
  type RendererFailure,
  Sandbox,
} from "../visualize/sandbox";
import { buildSvg } from "../visualize/svg";
import { useLook } from "./navigation";
import { hidden } from "./Values";
import { LocalExpansion, ValueRow } from "./ValueTree";
import { useFocus } from "./Workspace";

/** One value drawn: where it comes from and which renderers it offers. */
export interface Card {
  key: string;
  path: string;
  /** The renderers the value's view offers, or null until known. */
  offered: string[] | null;
  /** The renderer the link names for it, if any. */
  chosen: string | null;
  /** Its place among the link's pinned values, or null for a variable. */
  pinned: number | null;
}

let shared: Sandbox | null = null;

/** The card just pinned, which comes into view when it appears. */
let reveal: string | null = null;

/** Pins `pinned` to the drawings and brings its card into view. */
export function pin(pinned: Pinned): (look: Look) => Look {
  reveal = formatPinned(pinned);
  return (look) => drawing(look, pinned);
}

/** The tab's sandbox, started the first time something is drawn. */
function sandbox(): Sandbox {
  shared ??= new Sandbox(`${base}visualizer-frame`);
  return shared;
}

/** The drawings the focus shows: the frame's variables that a view draws,
 * then the values the link pins. */
export function useCards(): Card[] {
  const focus = useFocus();
  const why = hidden(focus);
  const scopes = useRequest("scopes", why === null ? (focus.at as At) : null);
  const pins = focus.look.d;
  // While the program runs, the cards stay as they were.
  const last = useRef<Card[]>([]);
  return useMemo(() => {
    if (why !== null && focus.stale !== "passed" && last.current.length > 0) {
      return last.current;
    }
    const cards: Card[] = [];
    const seen = new Set<string>();
    for (const scope of scopes.data?.scopes ?? []) {
      if (scope.key === "statics") {
        continue;
      }
      for (const row of scope.rows) {
        if (row.path !== null && row.drawings.length > 0 && !seen.has(row.path)) {
          seen.add(row.path);
          cards.push({
            key: row.path,
            path: row.path,
            offered: row.drawings,
            chosen: null,
            pinned: null,
          });
        }
      }
    }
    (pins ?? []).forEach((entry, index) => {
      const pinned = parsePinned(entry);
      const key = formatPinned(pinned);
      if (seen.has(key)) {
        // A pin of a variable the frame draws pins that variable's card.
        const card = cards.find((each) => each.key === key);
        if (card && card.pinned === null) {
          card.pinned = index;
        }
        return;
      }
      seen.add(key);
      cards.push({
        key,
        path: pinned.path,
        offered: null,
        chosen: pinned.renderer,
        pinned: index,
      });
    });
    last.current = cards;
    return cards;
  }, [scopes.data, pins, why, focus.stale]);
}

export function Drawings() {
  const focus = useFocus();
  const cards = useCards();
  const why = hidden(focus, "Drawings");
  if (cards.length === 0) {
    return (
      <div className="drawings">
        <div className="empty center-message">
          {why ??
            "No value here has a drawing. Draw one from its row, or give its type a `visualize` in a view."}
        </div>
      </div>
    );
  }
  return (
    <div className="drawings" data-testid="drawings">
      {cards.map((card) => (
        <DrawingCard key={card.key} card={card} />
      ))}
    </div>
  );
}

/** What a card shows: a picture, or why there is none, from one stop. */
type Shown =
  | { stop: number; picture: Picture; renderer: Drawing["renderer"] }
  | { stop: number; problem: string };

/** The inputs a card drew at each stop, for `previous`, by session, path,
 * and renderer: stops count on across sessions. */
const drawn = new Map<string, { stop: number; inputs: Record<string, Value> }[]>();

function previousInputs(key: string, stop: number): Record<string, Value> | null {
  const before = (drawn.get(key) ?? []).filter((entry) => entry.stop < stop);
  return before.at(-1)?.inputs ?? null;
}

function remember(key: string, stop: number, inputs: Record<string, Value>): void {
  const entries = (drawn.get(key) ?? []).filter((entry) => entry.stop !== stop);
  entries.push({ stop, inputs });
  entries.sort((a, b) => a.stop - b.stop);
  drawn.set(key, entries.slice(-4));
}

function failureText(failure: RendererFailure): string {
  const where = failure.file
    ? `${failure.file}${failure.line !== null ? `:${failure.line}${failure.column !== null ? `:${failure.column}` : ""}` : ""}: `
    : "";
  return `${where}${failure.message}`;
}

function outcomeProblem(outcome: Outcome): string | null {
  switch (outcome.kind) {
    case "timeout":
      return "The renderer took longer than 2 s, so it was stopped.";
    case "failed":
      return failureText(outcome.failure);
    case "picture":
      return null;
  }
}

/** A value's path with a part of it, as `select` names one. */
export function partPath(path: string, select: string): string {
  const whole = /^[A-Za-z_$][\w$]*(?:\.[A-Za-z_$][\w$]*|\[\d+\])*$/.test(path) ? path : `(${path})`;
  return select.startsWith("[") ? `${whole}${select}` : `${whole}.${select}`;
}

function DrawingCard({ card }: { card: Card }) {
  const focus = useFocus();
  const { at, stale } = focus;
  const live = at !== null && stale === null ? at : null;
  const look = useLook();
  const theme = useChosenTheme();
  const id = useId();
  const holder = useRef<HTMLDivElement>(null);
  const body = useRef<HTMLDivElement>(null);
  const [visible, setVisible] = useState(false);
  const [shown, setShown] = useState<Shown | null>(null);
  const [selected, setSelected] = useState<string | null>(null);

  // A pinned value with no renderer named draws with what its view offers.
  const evaluated = useRequest(
    "evaluate",
    card.offered === null && card.chosen === null && live
      ? { ...live, expression: card.path }
      : null,
  );
  const offered = card.offered ?? evaluated.data?.drawings ?? null;
  const [tab, setTab] = useState<string | null>(null);
  const renderer =
    card.chosen ?? (tab !== null && offered?.includes(tab) ? tab : (offered?.[0] ?? null));

  const asked = useRequest(
    "draw",
    live && renderer !== null && visible ? { ...live, path: card.path, renderer } : null,
  );
  const code = useRequest(
    "renderer",
    asked.data && asked.current ? { digest: asked.data.renderer.digest } : null,
  );

  // Only cards on screen draw.
  useEffect(() => {
    const element = holder.current;
    if (!element) {
      return;
    }
    if (reveal === card.key) {
      reveal = null;
      element.scrollIntoView({ block: "nearest" });
    }
    const observer = new IntersectionObserver((entries) => {
      setVisible(entries.some((entry) => entry.isIntersecting));
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, [card.key]);

  // Each card has its own worker, which ends with it.
  useEffect(() => () => shared?.release(id), [id]);

  const stop = live?.stop ?? null;
  const drawingData = asked.current ? asked.data : undefined;
  const source = code.current ? code.data : undefined;
  const problem = asked.current ? asked.error?.message : undefined;
  // biome-ignore lint/correctness/useExhaustiveDependencies: a new theme draws again, in its colors
  useEffect(() => {
    if (stop === null) {
      return;
    }
    if (problem !== undefined) {
      setShown({ stop, problem });
      return;
    }
    if (!drawingData) {
      return;
    }
    if (drawingData.problem !== null || drawingData.inputs === null) {
      setShown({ stop, problem: `Cannot draw: ${drawingData.problem ?? "no inputs"}` });
      return;
    }
    if (!source) {
      return;
    }
    let current = true;
    const inputs = decodeInputs(drawingData.inputs, drawingData.payload ?? new Uint8Array());
    const key = `${focus.session}~${card.path}~${drawingData.renderer.name}`;
    const chosenTheme = currentTheme();
    void sandbox()
      .draw(id, source, inputs, {
        previous: previousInputs(key, stop),
        width: body.current?.clientWidth ?? 600,
        theme: chosenTheme,
        palette: palette(chosenTheme),
      })
      .then((outcome) => {
        if (!current) {
          return;
        }
        const failed = outcomeProblem(outcome);
        if (failed !== null) {
          setShown({ stop, problem: failed });
          return;
        }
        try {
          const picture = validate((outcome as { picture: unknown }).picture);
          remember(key, stop, inputs);
          setShown({ stop, picture, renderer: drawingData.renderer });
        } catch (error) {
          setShown({
            stop,
            problem:
              error instanceof PictureError
                ? `${drawingData.renderer.name}.js returned a picture the page cannot show: ${error.message}`
                : String(error),
          });
        }
      });
    return () => {
      current = false;
    };
  }, [stop, drawingData, source, problem, theme, id, card.path, focus.session]);

  // The page builds the picture's elements itself.
  const picture = shown && "picture" in shown ? shown.picture : null;
  useEffect(() => {
    const element = body.current;
    if (!element) {
      return;
    }
    element.replaceChildren();
    if (picture) {
      element.append(buildSvg(picture, (path) => setSelected(path)));
    }
  }, [picture]);

  const dim = shown !== null && (stale !== null || shown.stop !== at?.stop);
  const origin = shown && "renderer" in shown ? shown.renderer.origin : null;
  const remove =
    card.pinned === null
      ? null
      : () =>
          look((current) => {
            const d = (current.d ?? []).filter((_, index) => index !== card.pinned);
            const { d: _d, ...rest } = current;
            return d.length > 0 ? { ...rest, d } : rest;
          });

  return (
    <section
      ref={holder}
      className={`drawing ${dim ? "stale" : ""}`}
      aria-label={`Drawing of ${card.path}${card.chosen !== null ? ` as ${card.chosen}` : ""}`}
    >
      <header className="drawing-head">
        <span className="drawing-path">{card.path}</span>
        {offered && offered.length > 1 && card.chosen === null ? (
          <fieldset className="segmented small" aria-label={`Drawings of ${card.path}`}>
            {offered.map((name) => (
              <button
                key={name}
                type="button"
                aria-pressed={name === renderer}
                onClick={() => setTab(name)}
              >
                {name}
              </button>
            ))}
          </fieldset>
        ) : (
          <span className="drawing-renderer">{renderer ?? "…"}</span>
        )}
        {origin && <span className="muted">{originLabel(origin)}</span>}
        <span className="drawing-end">
          {shown && <span className="muted">stop #{shown.stop}</span>}
          <DrawAs path={card.path} />
          {remove && (
            <button
              type="button"
              className="icon"
              aria-label={`Stop drawing ${card.path}`}
              title="Remove"
              onClick={remove}
            >
              ×
            </button>
          )}
        </span>
      </header>
      {offered !== null && offered.length === 0 && card.chosen === null ? (
        <div className="drawing-problem">
          No view draws {card.path}. Choose a renderer with Draw as….
        </div>
      ) : shown && "problem" in shown ? (
        <div className="drawing-problem" role="alert">
          {shown.problem}
        </div>
      ) : null}
      <div ref={body} className="drawing-body" hidden={!picture} />
      {picture?.caption !== undefined && <div className="drawing-caption">{picture.caption}</div>}
      {!shown && <div className="drawing-wait muted">{live ? "Drawing…" : hidden(focus)}</div>}
      {selected !== null && live && (
        <Selected
          at={live}
          path={partPath(card.path, selected)}
          onClose={() => setSelected(null)}
        />
      )}
    </section>
  );
}

/** Where a renderer comes from, as a card names it. */
function originLabel(origin: string): string {
  return origin === "built-in" ? "built-in" : (origin.split("/").at(-1) ?? origin);
}

/** A part of the drawn value, as a row, after a click on its shape. */
function Selected({ at, path, onClose }: { at: At; path: string; onClose(): void }) {
  const value = useRequest("evaluate", { ...at, expression: path });
  const look = useLook();
  const row: Row | undefined = value.data;
  return (
    <div className="drawing-selected" role="dialog" aria-label={`Part ${path}`}>
      <div className="drawing-selected-head">
        <span className="drawing-path">{path}</span>
        <button
          type="button"
          className="link-button"
          onClick={() => {
            look((current) => ({ ...current, w: [...(current.w ?? []), path] }));
            flash(`Watching ${path}`);
          }}
        >
          Watch
        </button>
        <button type="button" className="icon" aria-label="Close" onClick={onClose}>
          ×
        </button>
      </div>
      <LocalExpansion>
        <ul className="value-list">
          {value.error ? (
            <li className="empty error">{value.error.message}</li>
          ) : row ? (
            <ValueRow row={row} parent="drawing" depth={0} />
          ) : (
            <li className="empty">Reading…</li>
          )}
        </ul>
      </LocalExpansion>
    </div>
  );
}

/** Draws a value with any renderer, pinning it to the drawings. */
export function DrawAs({ path }: { path: string }) {
  const [open, setOpen] = useState(false);
  const renderers = useRequest("renderers", open ? undefined : null);
  const look = useLook();
  const choose = (pinned: Pinned) => {
    setOpen(false);
    look(pin(pinned), { replace: false });
  };
  return (
    <span className="anchor">
      <button
        type="button"
        className="link-button"
        aria-expanded={open}
        aria-label={`Draw ${path} as…`}
        onClick={() => setOpen(!open)}
      >
        Draw as…
      </button>
      {open && (
        <div className="popover menu" role="menu" aria-label={`Draw ${path} as`}>
          {renderers.error && <div className="empty error">{renderers.error.message}</div>}
          {!renderers.data && !renderers.error && <div className="empty">Reading…</div>}
          {renderers.data?.renderers.map((renderer) => (
            <button
              key={renderer.name}
              type="button"
              role="menuitem"
              className="menu-item"
              onClick={() => choose({ path, renderer: renderer.name })}
            >
              <span>{renderer.name}</span>
              <span className="muted">{originLabel(renderer.origin)}</span>
            </button>
          ))}
        </div>
      )}
    </span>
  );
}
