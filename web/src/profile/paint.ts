// How profile views name numbers and color frames. Colors follow a frame's
// family, its module or its outermost namespace, so they stay the same
// across views, profiles, and queries.

import type { ProfileKey } from "../protocol";

/** A weight in its unit: CPU time, bytes, or a count of what the
 * profile counts, such as executions or an imported profile's goroutines. */
export function formatWeight(weight: number, unit: string): string {
  if (unit === "nanoseconds") {
    return formatDuration(weight);
  }
  if (unit === "bytes") {
    return formatBytes(weight);
  }
  const count = Math.round(weight).toLocaleString("en-US");
  const one = unit.endsWith("s") ? unit.slice(0, -1) : unit;
  return `${count} ${weight === 1 ? one : unit.endsWith("s") ? unit : `${unit}s`}`;
}

/** Bytes, in the largest binary unit that keeps them above one. */
export function formatBytes(bytes: number): string {
  const units: [number, string][] = [
    [2 ** 30, "GiB"],
    [2 ** 20, "MiB"],
    [2 ** 10, "KiB"],
  ];
  for (const [size, name] of units) {
    if (Math.abs(bytes) >= size) {
      return `${(bytes / size).toPrecision(3)} ${name}`;
    }
  }
  return `${Math.round(bytes)} B`;
}

/** Nanoseconds, in the largest unit that keeps them above one. */
export function formatDuration(nanoseconds: number): string {
  const units: [number, string][] = [
    [1e9, "s"],
    [1e6, "ms"],
    [1e3, "µs"],
  ];
  for (const [size, name] of units) {
    if (Math.abs(nanoseconds) >= size) {
      return `${(nanoseconds / size).toPrecision(3)} ${name}`;
    }
  }
  return `${Math.round(nanoseconds)} ns`;
}

export function formatPercent(part: number, whole: number): string {
  if (whole <= 0) {
    return "0%";
  }
  const percent = (part * 100) / whole;
  return `${percent >= 10 ? percent.toFixed(1) : percent.toFixed(2)}%`;
}

/** What a frame's color follows: its module, or failing that the first
 * part of its name, such as a Rust crate or a Go package. */
export function family(key: ProfileKey): string {
  const name = key.name.replace(/^\[task\] /, "").replace(/^</, "");
  const scope = /^([A-Za-z_][\w]*)(::|\.)/.exec(name);
  return scope?.[1] ?? key.module ?? name;
}

/** A stable hue for a family. */
export function hue(text: string): number {
  // FNV-1a, which spreads similar names apart.
  let hash = 0x811c9dc5;
  for (let at = 0; at < text.length; at += 1) {
    hash ^= text.charCodeAt(at);
    hash = Math.imul(hash, 0x01000193);
  }
  return (hash >>> 0) % 360;
}

/** A frame's fill: warm warnings for truncations, the family's hue for
 * code. Dark themes darken every fill. */
export function fill(key: ProfileKey, dark: boolean): string {
  if (key.kind === "truncation") {
    return dark ? "hsl(40 70% 30%)" : "hsl(42 90% 75%)";
  }
  const h = key.kind === "task" ? 225 : hue(family(key));
  const saturation = key.kind === "address" ? 10 : 50;
  return dark ? `hsl(${h} ${saturation - 15}% 30%)` : `hsl(${h} ${saturation}% 78%)`;
}
