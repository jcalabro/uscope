// Every renderer example in docs/visualizers.md runs in the real worker and
// returns a picture the page accepts, and the tutorial's files are the
// fixture's, so the reference cannot drift from what uscope does.

import { afterEach, expect, it } from "vitest";
import { commands } from "vitest/browser";
import { validate } from "../src/visualize/picture";
import workerSource from "../src/visualize/worker.js?raw";
import { palette } from "./palette";

const docs = await commands.readFile("../docs/visualizers.md");

const workers: Worker[] = [];
afterEach(() => {
  for (const worker of workers.splice(0)) {
    worker.terminate();
  }
});

/** What `source` returns for `input`: the worker's answer to one draw. */
async function run(source: string, input: unknown): Promise<Record<string, unknown>> {
  const url = URL.createObjectURL(new Blob([workerSource], { type: "text/javascript" }));
  const worker = new Worker(url);
  workers.push(worker);
  const answers: Record<string, unknown>[] = [];
  let wake = () => {};
  worker.addEventListener("message", (event) => {
    answers.push(event.data);
    wake();
  });
  const next = async () => {
    while (answers.length === 0) {
      await new Promise<void>((resolve) => {
        wake = resolve;
      });
    }
    return answers.shift() as Record<string, unknown>;
  };
  worker.postMessage({ type: "load", source, name: "example" });
  const loaded = await next();
  expect(loaded.error, JSON.stringify(loaded.error)).toBeNull();
  worker.postMessage({
    type: "draw",
    id: 1,
    input,
    context: { previous: input, width: 600, theme: "light", palette },
  });
  return next();
}

const examples = [
  ...docs.matchAll(/```uscope-renderer-example\n([\s\S]*?)\n---\n([\s\S]*?)\n```/g),
].map(([, source, input]) => ({ source: source as string, input: input as string }));

it("has renderer examples", () => {
  expect(examples.length).toBeGreaterThanOrEqual(3);
});

for (const [index, example] of examples.entries()) {
  it(`runs example ${index + 1} to a picture the page accepts`, async () => {
    const input = new Function(`return ${example.input};`)();
    const answer = await run(example.source, input);
    expect(answer.type, JSON.stringify(answer)).toBe("picture");
    expect(() => validate(answer.picture)).not.toThrow();
  });
}

it("shows the life fixture's own files", async () => {
  const blocks = [...docs.matchAll(/```(?:js|text)\n([\s\S]*?)```/g)].map(([, body]) => body);
  expect(blocks).toContain(await commands.readFile("../tests/fixtures/c/life/life.js"));
  expect(blocks).toContain(await commands.readFile("../tests/fixtures/c/life/life.views"));
});
