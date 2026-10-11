// Each test gets its own `uscope web`, started from the development build
// (which serves build/web from disk), and stopped when the test ends.

import { type ChildProcess, spawn } from "node:child_process";
import { mkdir, rm, writeFile } from "node:fs/promises";
import * as path from "node:path";
import { createInterface } from "node:readline";
import { test as base, expect, type Page } from "@playwright/test";

export { expect };

export const root = path.join(import.meta.dirname, "..", "..");

export function fixture(name: string): string {
  return path.join(root, "build", "test-programs", name);
}

export interface Uscope {
  /** The server's origin, such as `http://127.0.0.1:41234`. */
  origin: string;
  /** The link `uscope web` printed, which gives control. */
  link: string;
  /** Starts another `uscope web` with these arguments. */
  start(args: string[]): Promise<Uscope>;
  /** Everything the server printed so far, for failure messages. */
  log: string[];
  stop(): Promise<void>;
}

/** Starts `uscope web`, flight-recording to `recording`. */
export async function startUscope(args: string[], recording: string): Promise<Uscope> {
  const child = spawn(
    // A release build can stand in, to see how fast a large program is.
    process.env.USCOPE_WEB_BINARY ?? path.join(root, "target", "debug", "uscope"),
    ["web", "--port", "0", ...args],
    {
      cwd: root,
      env: { ...process.env, USER: "tester", USCOPE_FLIGHT_RECORDING: recording },
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  const log: string[] = [];
  const lines = createInterface({ input: child.stdout });
  createInterface({ input: child.stderr }).on("line", (line) => log.push(`stderr: ${line}`));
  const link = await new Promise<string>((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error(`uscope web never printed a link: ${log}`)),
      10_000,
    );
    lines.on("line", (line) => {
      log.push(line);
      if (line.startsWith("open ")) {
        clearTimeout(timer);
        resolve(line.slice("open ".length));
      }
    });
    child.on("exit", (code) => reject(new Error(`uscope web exited with ${code}: ${log}`)));
  });
  return {
    origin: new URL(link).origin,
    link,
    log,
    start: (more) => startUscope(more, `${recording}.next`),
    stop: () => stopChild(child),
  };
}

function stopChild(child: ChildProcess): Promise<void> {
  if (child.exitCode !== null || child.signalCode !== null) {
    return Promise.resolve();
  }
  return new Promise((resolve) => {
    child.on("exit", () => resolve());
    // SIGINT ends the session the way Ctrl+C does, killing its program.
    child.kill("SIGINT");
  });
}

/**
 * `uscope` serves `program`. A failing test keeps each server's output and
 * flight recording in its output directory.
 */
export const test = base.extend<{ program: string[]; uscope: Uscope; quiet: undefined }>({
  // Every page stays free of console errors, such as a blocked font or a
  // failed request, unless a test expects one.
  quiet: [
    async ({ context }, use) => {
      const errors: string[] = [];
      const watch = (page: Page) => {
        page.on("console", (message) => {
          // A failed request is recorded once, by its response, below. Firefox
          // also reports a font download it aborted as the page went away
          // (NS_BINDING_ABORTED), which no blocked or missing font causes.
          const aborted = /downloadable font: download failed.*status=2152398850/.test(
            message.text(),
          );
          if (
            message.type() === "error" &&
            !aborted &&
            !/failed to load resource/i.test(message.text())
          ) {
            errors.push(`${page.url()}: ${message.text().slice(0, 300)}`);
          }
        });
        page.on("pageerror", (error) => errors.push(`${page.url()}: ${error.message}`));
        page.on("response", (response) => {
          if (response.status() >= 400) {
            errors.push(`${response.request().method()} ${response.url()}: ${response.status()}`);
          }
        });
      };
      context.pages().forEach(watch);
      context.on("page", watch);
      await use(undefined);
      const expected = (expectedErrors.get(test.info().testId) ?? []).map((error) =>
        typeof error === "string" ? error : expect.stringMatching(error),
      );
      expect(errors).toEqual(expected);
    },
    { auto: true },
  ],
  program: [[fixture("spin")], { option: true }],
  uscope: async ({ program, context }, use, info) => {
    const servers: Uscope[] = [];
    await mkdir(info.outputDir, { recursive: true });
    const recording = (index: number) => info.outputPath(`uscope-${index}.flight.log`);
    const first = await startUscope(program, recording(0));
    servers.push(first);
    await use({
      ...first,
      start: async (args) => {
        const another = await startUscope(args, recording(servers.length));
        servers.push(another);
        return another;
      },
    });
    // Pages go before their server, so none sees it vanish and reconnects
    // while the test's errors are counted.
    for (const page of context.pages()) {
      await page.close();
    }
    for (const server of servers) {
      await server.stop();
    }
    if (info.status === info.expectedStatus) {
      await rm(info.outputDir, { recursive: true, force: true });
    } else {
      for (const [index, server] of servers.entries()) {
        await writeFile(info.outputPath(`uscope-${index}.log`), server.log.join("\n"));
      }
    }
  },
});

const expectedErrors = new Map<string, (string | RegExp)[]>();

/** Declares the errors this test's pages will show, in order, as text or
 * as patterns. */
export function expectErrors(...errors: (string | RegExp)[]): void {
  expectedErrors.set(test.info().testId, errors);
}

/** Opens the join link and waits until the page shows the session. */
export async function join(page: Page, link: string): Promise<void> {
  await page.goto(link);
  await page.waitForURL(/\/(s\/[0-9a-f]+|pick)(\/|\?|$)/);
}
