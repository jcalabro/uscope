// The URL is the focus. The path names the debugger state: session, stop,
// thread, frame. The query names how a tab looks at it: the file and lines
// shown, the view, watches, and the expanded rows of the variable tree.
// Every function here is pure, so links round-trip exactly.

/** What a tab looks at, beyond the stop: everything in the query. */
export interface Look {
  /** The source shown: `PATH:LINE` or `PATH:FIRST-LAST`. */
  src?: string;
  /** Disassembly at an address. */
  asm?: string;
  /** Memory at an address. */
  mem?: string;
  /** Which of the three views of the place is shown; source when absent. */
  view?: View;
  /** Watch expressions, in order. */
  w?: string[];
  /** Expanded paths in the variable tree, such as `args/req`. */
  x?: string[];
}

export type View = "source" | "disassembly" | "memory";

const VIEWS: readonly View[] = ["source", "disassembly", "memory"];

/** The keys every route's query may carry, and whether each repeats. */
const LISTS = new Set(["w", "x"]);

/**
 * Parses a query, keeping a repeated key's values in order. Lists repeat
 * their key, `w=a&w=b`, because a watch expression can hold any character.
 */
export function parseSearch(search: string): Record<string, string | string[]> {
  const parsed: Record<string, string | string[]> = {};
  for (const [key, value] of new URLSearchParams(search)) {
    const existing = parsed[key];
    if (LISTS.has(key)) {
      parsed[key] = [...(Array.isArray(existing) ? existing : []), value];
    } else if (existing === undefined) {
      parsed[key] = value;
    }
  }
  return parsed;
}

export function stringifySearch(search: Record<string, unknown>): string {
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(search)) {
    if (Array.isArray(value)) {
      for (const item of value) {
        params.append(key, String(item));
      }
    } else if (value !== undefined && value !== null && value !== "") {
      params.append(key, String(value));
    }
  }
  const text = params.toString();
  // `/` and `:` read better than their escapes, and are safe in a query.
  return text ? `?${text.replaceAll("%2F", "/").replaceAll("%3A", ":")}` : "";
}

/** The typed query, dropping anything malformed. */
export function validateLook(search: Record<string, unknown>): Look {
  const look: Look = {};
  const text = (key: string) => {
    const value = search[key];
    return typeof value === "string" && value !== "" ? value : undefined;
  };
  const list = (key: string) => {
    const value = search[key];
    const items = Array.isArray(value) ? value : typeof value === "string" ? [value] : [];
    const strings = items.filter((item): item is string => typeof item === "string" && item !== "");
    return strings.length > 0 ? strings : undefined;
  };
  const src = text("src");
  if (src && parsePlace(src)) {
    look.src = src;
  }
  const asm = text("asm");
  if (asm && isAddress(asm)) {
    look.asm = asm;
  }
  const mem = text("mem");
  if (mem && isAddress(mem)) {
    look.mem = mem;
  }
  const view = text("view");
  if (view && (VIEWS as readonly string[]).includes(view) && view !== "source") {
    look.view = view as View;
  }
  const w = list("w");
  if (w) {
    look.w = w;
  }
  const x = list("x");
  if (x) {
    look.x = x;
  }
  return look;
}

export function isAddress(text: string): boolean {
  return /^0x[0-9a-f]{1,16}$/i.test(text);
}

/** A place in a source file: a line, or a range of them. */
export interface SourcePlace {
  path: string;
  line: number;
  end: number;
}

/** Parses `PATH:LINE` or `PATH:FIRST-LAST`, where the path may hold colons. */
export function parsePlace(text: string): SourcePlace | null {
  const match = /^(.+):(\d+)(?:-(\d+))?$/.exec(text);
  if (!match) {
    return null;
  }
  const [, path, first, last] = match as unknown as [string, string, string, string | undefined];
  const line = Number(first);
  const end = last === undefined ? line : Number(last);
  if (line < 1 || end < line) {
    return null;
  }
  return { path, line, end };
}

export function formatPlace(place: SourcePlace): string {
  return place.end > place.line
    ? `${place.path}:${place.line}-${place.end}`
    : `${place.path}:${place.line}`;
}

/**
 * A path as a link names it: relative to the directory uscope runs in, when
 * it is inside it, which keeps links short. A compiler can record relative
 * paths too, so a path stays whole when its short form names another of the
 * recorded `files`, or while they are unknown.
 */
export function linkPath(
  path: string,
  cwd: string | undefined,
  files: readonly string[] | undefined,
): string {
  if (cwd && files && path.startsWith(`${cwd}/`)) {
    const short = path.slice(cwd.length + 1);
    if (!files.includes(short)) {
      return short;
    }
  }
  return path;
}

/**
 * The path a link names, as the debug information records it, or null while
 * the recorded `files` that tell a relative path's meaning are unknown.
 */
export function recordedPath(
  path: string,
  cwd: string | undefined,
  files: readonly string[] | undefined,
): string | null {
  if (path.startsWith("/")) {
    return path;
  }
  if (!files) {
    return null;
  }
  if (files.includes(path) || !cwd) {
    return path;
  }
  return `${cwd}/${path}`;
}

/**
 * Whether a link someone else made names a path on this page, never another
 * host: browsers read `//host` and `/\\host` as one.
 */
export function isPagePath(text: string | undefined): text is string {
  const control = (char: string) => char < " " || char === "\u007f";
  return text !== undefined && /^\/(?![/\\])/.test(text) && ![...text].some(control);
}

/** The part of the path after the session: the stop, thread, and frame. */
export interface At {
  stop: number;
  thread: number;
  frame: number;
}

export function stopPath(session: string, at: At): string {
  return `/s/${session}/stop/${at.stop}/t/${at.thread}/f/${at.frame}`;
}

/** Parses a route's path parameters, or null when they are not numbers. */
export function parseAt(params: { stop: string; thread: string; frame: string }): At | null {
  const numbers = [params.stop, params.thread, params.frame].map((text) =>
    /^\d+$/.test(text) ? Number(text) : Number.NaN,
  );
  const [stop, thread, frame] = numbers as [number, number, number];
  if (numbers.some(Number.isNaN)) {
    return null;
  }
  return { stop, thread, frame };
}

/**
 * What a followed link keeps at a new stop: how the tab looks, but not where
 * it looked, since the code view follows the program counter.
 */
export function followedLook(look: Look): Look {
  const { src: _src, asm: _asm, view, ...kept } = look;
  return view === "memory" ? { ...kept, view } : kept;
}

/** The same look, showing the source at `src`. */
export function showingSource(look: Look, src: string): Look {
  const { view: _view, ...rest } = look;
  return { ...rest, src };
}
