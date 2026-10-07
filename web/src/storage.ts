// Per-browser conveniences kept in localStorage: a chosen name, the theme,
// recent launches. Storage may be unavailable, so every access is guarded.

export function read<T>(key: string, fallback: T, valid: (value: unknown) => value is T): T {
  try {
    const text = localStorage.getItem(key);
    if (text === null) {
      return fallback;
    }
    const value: unknown = JSON.parse(text);
    return valid(value) ? value : fallback;
  } catch {
    return fallback;
  }
}

export function write(key: string, value: unknown): void {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {
    // A private window; the convenience is lost, nothing else.
  }
}

export const isString = (value: unknown): value is string => typeof value === "string";

export interface RecentLaunch {
  program: string;
  arguments: string;
  cwd: string;
  environment: string;
}

export const isRecentLaunches = (value: unknown): value is RecentLaunch[] =>
  Array.isArray(value) &&
  value.every(
    (item) =>
      typeof item === "object" &&
      item !== null &&
      ["program", "arguments", "cwd", "environment"].every(
        (key) => typeof (item as Record<string, unknown>)[key] === "string",
      ),
  );

/** Remembers a launch, most recent first, without duplicates. */
export function rememberLaunch(launch: RecentLaunch): RecentLaunch[] {
  const previous = read("uscope-recent", [], isRecentLaunches).filter(
    (item) => item.program !== launch.program || item.arguments !== launch.arguments,
  );
  const recent = [launch, ...previous].slice(0, 8);
  write("uscope-recent", recent);
  return recent;
}
