// Every renderer example in docs/visualizers.md runs in the real worker and
// returns a picture the page accepts, and the tutorial's files are the
// fixture's, so the reference cannot drift from what uscope does.

import { expect, it } from "vitest";
import { commands } from "vitest/browser";
import { validate } from "../src/visualize/picture";
import { run as runRenderer } from "./renderer";

const docs = await commands.readFile("../docs/visualizers.md");

/** What `source` returns for `input`, which is also its previous input, as
 * if each input were the value's member of the same name. */
function run(source: string, input: unknown): Promise<Record<string, unknown>> {
  return runRenderer(source, input, {
    previous: input,
    paths: Object.fromEntries(Object.keys(input as object).map((name) => [name, name])),
    width: 600,
  });
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
