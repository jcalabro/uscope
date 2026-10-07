// How the connection behaves when the server goes away, refuses the
// browser, or speaks another version.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Connection, type TransportHandlers } from "../src/connection";
import type { Link, ModelEvent } from "../src/model";
import { PROTOCOL_VERSION } from "../src/protocol";

const hello = (version = PROTOCOL_VERSION) =>
  JSON.stringify({
    type: "hello",
    version,
    connection: 1,
    role: "control",
    name: "tester",
    cwd: "/",
  });

/** A connection whose every attempt is recorded and driven by the test. */
function harness(authorized: () => Promise<boolean> = async () => true) {
  const attempts: TransportHandlers[] = [];
  const sent: string[] = [];
  const events: ModelEvent[] = [];
  const connection = new Connection({
    connect: (handlers) => {
      attempts.push(handlers);
      return { send: (text) => sent.push(text), close: () => handlers.close() };
    },
    authorized,
    emit: (event) => events.push(event),
    backoff: [100, 1000],
  });
  const links = () =>
    events.flatMap((event): Link[] => (event.type === "link" ? [event.link] : []));
  return { connection, attempts, sent, events, links };
}

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("a dropped connection", () => {
  it("fails what was asked and reconnects with backoff", async () => {
    const { connection, attempts, links } = harness();
    connection.start();
    await vi.advanceTimersByTimeAsync(0);
    attempts[0]?.open();
    attempts[0]?.message(hello());
    const asked = connection.request("pause");

    attempts[0]?.close();
    await expect(asked).rejects.toMatchObject({ kind: "disconnected" });
    expect(links().at(-1)).toBe("reconnecting");
    await expect(connection.request("pause")).rejects.toMatchObject({ kind: "disconnected" });

    // The first retry waits the first delay, and the next the longer one.
    await vi.advanceTimersByTimeAsync(99);
    expect(attempts).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(attempts).toHaveLength(2);
    attempts[1]?.close();
    await vi.advanceTimersByTimeAsync(999);
    expect(attempts).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(1);
    expect(attempts).toHaveLength(3);

    // A hello starts the delays over.
    attempts[2]?.open();
    attempts[2]?.message(hello());
    attempts[2]?.close();
    await vi.advanceTimersByTimeAsync(100);
    expect(attempts).toHaveLength(4);
    connection.stop();
  });

  it("retries when the server cannot even be asked", async () => {
    let up = false;
    const { connection, attempts } = harness(async () => {
      if (!up) {
        throw new Error("connection refused");
      }
      return true;
    });
    connection.start();
    await vi.advanceTimersByTimeAsync(100);
    expect(attempts).toHaveLength(0);
    up = true;
    await vi.advanceTimersByTimeAsync(1000);
    expect(attempts).toHaveLength(1);
    connection.stop();
  });

  it("stays down once stopped", async () => {
    const { connection, attempts } = harness();
    connection.start();
    await vi.advanceTimersByTimeAsync(0);
    connection.stop();
    await vi.advanceTimersByTimeAsync(10_000);
    expect(attempts).toHaveLength(1);
  });
});

describe("a refused browser", () => {
  it("stops trying when it holds no token", async () => {
    const { connection, attempts, links } = harness(async () => false);
    connection.start();
    await vi.advanceTimersByTimeAsync(10_000);
    expect(attempts).toHaveLength(0);
    expect(links()).toEqual(["connecting", "unauthorized"]);
  });

  it("stops at a server of another protocol version", async () => {
    const { connection, attempts, events, links } = harness();
    connection.start();
    await vi.advanceTimersByTimeAsync(0);
    attempts[0]?.open();
    attempts[0]?.message(hello(PROTOCOL_VERSION + 1));
    await vi.advanceTimersByTimeAsync(10_000);
    expect(attempts).toHaveLength(1);
    expect(links()).toEqual(["connecting", "incompatible"]);
    // The model never sees the foreign server's hello.
    expect(events.some((event) => event.type === "message")).toBe(false);
  });
});

describe("answers", () => {
  it("ignores unparseable frames and answers nobody asked for", async () => {
    const { connection, attempts, sent, events } = harness();
    connection.start();
    await vi.advanceTimersByTimeAsync(0);
    attempts[0]?.open();
    const asked = connection.request("processes");
    attempts[0]?.message("{not json");
    attempts[0]?.message(JSON.stringify({ type: "result", id: 99, result: null }));
    const { id } = JSON.parse(sent[0] ?? "{}") as { id: number };
    attempts[0]?.message(
      JSON.stringify({ type: "error", id, error: { kind: "forbidden", message: "view only" } }),
    );
    await expect(asked).rejects.toMatchObject({ kind: "forbidden", message: "view only" });
    expect(events.filter((event) => event.type === "message")).toEqual([]);
    connection.stop();
  });
});
