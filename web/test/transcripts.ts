// Recorded traffic between real `uscope web` servers and the Rust test
// clients (tests/web): `just web-transcripts` refreshes it.

import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import type { Envelope, ServerMessage } from "../src/protocol";

export type Line = { to: "server"; message: Envelope } | { to: "page"; message: ServerMessage };

const directory = join(import.meta.dirname, "transcripts");

export function transcriptNames(): string[] {
  return readdirSync(directory)
    .filter((name) => name.endsWith(".jsonl"))
    .map((name) => name.slice(0, -".jsonl".length))
    .sort();
}

export function transcript(name: string): Line[] {
  return readFileSync(join(directory, `${name}.jsonl`), "utf8")
    .split("\n")
    .filter((line) => line.trim() !== "")
    .map((line) => JSON.parse(line) as Line);
}

/** The messages the page received, in order. */
export function received(name: string): ServerMessage[] {
  return transcript(name).flatMap((line) => (line.to === "page" ? [line.message] : []));
}
