// The page's state logic, fed what real servers sent.

import { describe, expect, it, vi } from "vitest";
import { action } from "../src/actions";
import { Connection, type TransportHandlers } from "../src/connection";
import { initialModel, type Model, type ModelEvent, reduce } from "../src/model";
import type { ServerMessage } from "../src/protocol";
import { shown } from "../src/ui/Status";
import { received, transcript, transcriptNames } from "./transcripts";

function replay(messages: ServerMessage[]): Model[] {
  const models: Model[] = [];
  let model = initialModel;
  for (const [index, message] of messages.entries()) {
    model = reduce(model, { type: "message", message, at: index });
    models.push(model);
  }
  return models;
}

const output = (model: Model) => model.output.map((piece) => piece.text).join("");

describe("every recorded transcript", () => {
  it.each(transcriptNames())("%s replays into a consistent model", (name) => {
    const messages = received(name);
    expect(messages[0]?.type).toBe("hello");
    const models = replay(messages);
    for (const model of models) {
      // Every state names its session exactly when something is debugged.
      if (model.state) {
        expect(model.state.session === null).toBe(model.state.target === null);
      }
      // Each model is something the status and actions can describe.
      expect(shown(model).label).not.toBe("");
      for (const name of ["continue", "pause", "kill", "restart"] as const) {
        action(name, model);
      }
    }
    // Revisions within one session never go backward.
    let last: { session: string | null; revision: number } | null = null;
    for (const message of messages) {
      if (message.type === "state") {
        if (last && last.session === message.session) {
          expect(message.revision).toBeGreaterThanOrEqual(last.revision);
        }
        last = { session: message.session, revision: message.revision };
      }
    }
  });
});

describe("a program run to its exit", () => {
  const models = replay(received("run-to-exit"));
  const final = models.at(-1) as Model;

  it("collects both streams in order", () => {
    expect(output(final)).toContain("out 1\n");
    expect(final.output.some((piece) => piece.stream === "stderr")).toBe(true);
  });

  it("offers to run it again once it exits", () => {
    expect(final.state?.inferior.state).toBe("exited");
    expect(shown(final).label).toBe("Exited");
    expect(action("continue", final).enabled).toBe(true);
    expect(action("pause", final).enabled).toBe(false);
  });

  it("waited for continue before it started", () => {
    const firstState = models.find((model) => model.state)?.state;
    expect(firstState?.inferior.state).toBe("notStarted");
  });
});

describe("two tabs racing a stop", () => {
  const models = replay(received("two-tabs"));

  it("continues the very stop it showed", async () => {
    const stopped = models.find((model) => model.state?.inferior.state === "stopped") as Model;
    const inferior = stopped.state?.inferior;
    const continuing = action("continue", stopped);
    if (!continuing.enabled || inferior?.state !== "stopped") {
      throw new Error("the transcript never offered continue at a stop");
    }
    const { connection, sent } = await openConnection();
    void continuing.run(connection);
    expect(JSON.parse(sent[0] ?? "{}")).toMatchObject({
      method: "continue",
      params: { stop: inferior.stop },
    });
  });

  it("hears who started the program", () => {
    const notices = (models.at(-1) as Model).notices;
    expect(notices.map((notice) => notice.text)).toContain("started the program");
  });
});

describe("the picker replacing a session", () => {
  const models = replay(received("picker"));

  it("drops the old program's output when another starts", () => {
    const sessions = new Set(models.map((model) => model.state?.session).filter(Boolean));
    expect(sessions.size).toBeGreaterThanOrEqual(2);
    for (const [index, model] of models.entries()) {
      const previous = models[index - 1];
      if (previous?.state?.session && model.state?.session !== previous.state.session) {
        expect(model.output).toEqual([]);
      }
    }
  });

  it("ends with nothing debugged after a failed launch", () => {
    const final = models.at(-1) as Model;
    expect(final.state?.session).toBeNull();
    expect(shown(final).label).toBe("Nothing loaded");
    expect(action("continue", final)).toMatchObject({ enabled: false });
  });
});

describe("the connection, driven by recorded traffic", () => {
  it.each(transcriptNames())("%s: each request gets its own answer", async (name) => {
    const lines = transcript(name);
    const events: ModelEvent[] = [];
    const { connection, sent, transport } = await openConnection((event) => events.push(event));
    const ask = connection.request.bind(connection) as (
      method: string,
      params?: unknown,
    ) => Promise<unknown>;
    // The recorded client numbered its requests its own way.
    const local = new Map<number, number>();
    const answers = new Map<number, Promise<unknown>>();
    for (const line of lines) {
      if (line.to === "server") {
        const { id, ...request } = line.message;
        const promise =
          "params" in request ? ask(request.method, request.params) : ask(request.method);
        local.set(id, (JSON.parse(sent.at(-1) ?? "{}") as { id: number }).id);
        answers.set(
          id,
          promise.then(
            (result) => ({ ok: result }),
            (error: Error) => ({ error: error.message }),
          ),
        );
      } else if (line.message.type === "result" || line.message.type === "error") {
        transport.message(JSON.stringify({ ...line.message, id: local.get(line.message.id) }));
      } else {
        transport.message(JSON.stringify(line.message));
      }
    }
    for (const line of lines) {
      if (line.to === "page" && line.message.type === "result") {
        expect(await answers.get(line.message.id)).toEqual({ ok: line.message.result });
      } else if (line.to === "page" && line.message.type === "error") {
        expect(await answers.get(line.message.id)).toEqual({ error: line.message.error.message });
      }
    }
    // Answers never reach the model; everything else does, in order.
    const forwarded = events.flatMap((event) => (event.type === "message" ? [event.message] : []));
    const expected = lines.flatMap((line) =>
      line.to === "page" && line.message.type !== "result" && line.message.type !== "error"
        ? [line.message]
        : [],
    );
    expect(forwarded).toEqual(expected);
  });
});

/** A connection whose transport has opened, recording what it sends. */
async function openConnection(emit: (event: ModelEvent) => void = () => undefined) {
  let handlers: TransportHandlers | null = null;
  const sent: string[] = [];
  const connection = new Connection({
    connect: (given) => {
      handlers = given;
      return { send: (text) => sent.push(text), close: () => undefined };
    },
    authorized: async () => true,
    emit,
  });
  connection.start();
  const transport = await vi.waitFor(() => {
    if (!handlers) {
      throw new Error("the connection never connected");
    }
    return handlers as TransportHandlers;
  });
  transport.open();
  return { connection, sent, transport };
}
