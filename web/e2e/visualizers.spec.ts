// Drawing values in the page: a program's own renderer, a session's views
// file, what changed since the stop before, a drawing's parts, the link,
// failures, and reloading views.

import { copyFile, readFile, writeFile } from "node:fs/promises";
import * as path from "node:path";
import type { Page } from "@playwright/test";
import { card, drawAt } from "./drawings";
import { expect, fixture, join, root, test, type Uscope } from "./server";

/** The squares a board's drawing outlines as changed since the stop before. */
async function outlined(page: Page): Promise<string[]> {
  return card(page, "self")
    .locator("rect[stroke]")
    .evaluateAll((rects) =>
      rects.map((rect) => rect.querySelector("title")?.textContent?.split(":")[0] ?? ""),
    );
}

test.describe("a program's own renderer", () => {
  test.use({ program: [fixture("chess")] });

  test("draws a board, marks what moved, and opens a square's part", async ({ page, uscope }) => {
    await join(page, uscope.link);
    await drawAt(page, "main.rs:108");
    const board = card(page, "self");
    await expect(page.getByRole("button", { name: /Drawings\s*1/ })).toBeVisible();
    // Pieces are named by their squares' titles.
    await expect(board.getByRole("button", { name: "e1: White King" })).toBeVisible();
    await expect(board.getByRole("button", { name: "d8: Black Queen" })).toBeVisible();
    await expect(board.getByRole("button", { name: "e4", exact: true })).toBeVisible();
    await expect(board.locator(".drawing-caption")).toHaveText("White to move");
    await expect(board).toContainText("chess.views[1]");
    expect(await outlined(page)).toEqual([]);

    // The next move: e2e4, which the board marks against the stop before.
    await page.keyboard.press("F5");
    await expect(board).toContainText("stop #3");
    await expect(board.getByRole("button", { name: "e4: White Pawn" })).toBeVisible();
    await expect(board.locator(".drawing-caption")).toHaveText("Black to move");
    await expect.poll(() => outlined(page)).toEqual(["e2", "e4"]);

    // A square opens its part of the value, which can be watched.
    await board.getByRole("button", { name: "e4: White Pawn" }).click();
    const part = page.getByRole("dialog", { name: "Part self.mailbox[28]" });
    await expect(part).toContainText("Some");
    await part.getByRole("button", { name: "Watch", exact: true }).click();
    await expect(page).toHaveURL(/[?&]w=self.mailbox%5B28%5D/);
    await part.getByRole("button", { name: "Close" }).click();
    await expect(part).toBeHidden();

    // The link shows the same drawing.
    await page.reload();
    await expect(card(page, "self").getByRole("button", { name: "e4: White Pawn" })).toBeVisible();
  });

  test("Draw as… pins any value to any renderer, which says why it cannot draw it", async ({
    page,
    uscope,
  }) => {
    await join(page, uscope.link);
    await drawAt(page, "main.rs:108");
    const add = page.getByRole("textbox", { name: "Add a watch", exact: true });
    await add.fill("self.fullmove");
    await add.press("Enter");
    await page.getByRole("button", { name: "Draw self.fullmove as…" }).first().click();
    await page.getByRole("menuitem", { name: /^chess-board/ }).click();
    await expect(page).toHaveURL(/[?&]d=self.fullmove~chess-board/);
    // The board's renderer reads squares the number does not have.
    const pinned = page.getByRole("region", { name: "Drawing of self.fullmove as chess-board" });
    await expect(pinned.getByRole("alert")).toContainText(/chess-board\.js:\d+:\d+: TypeError/);
    // Removing it unpins it.
    await pinned.getByRole("button", { name: "Stop drawing self.fullmove" }).click();
    await expect(pinned).toBeHidden();
    await expect(page).not.toHaveURL(/[?&]d=/);
  });
});

/** Starts uscope on life with `views`, and joins it. */
async function life(page: Page, uscope: Uscope, views: string): Promise<Uscope> {
  const server = await uscope.start(["--views", views, fixture("life")]);
  await join(page, server.link);
  return server;
}

/** How many of a life board's cells are alive, and how many were just born,
 * from the pixels the page shows. */
async function cells(page: Page): Promise<{ alive: number; born: number }> {
  return card(page, "life")
    .locator("canvas")
    .evaluate((canvas: HTMLCanvasElement) => {
      const { data } = canvas
        .getContext("2d")
        ?.getImageData(0, 0, canvas.width, canvas.height) as ImageData;
      let alive = 0;
      let born = 0;
      for (let at = 0; at < data.length; at += 4) {
        if (data[at] === 26) {
          alive += 1;
        } else if (data[at] === 176) {
          alive += 1;
          born += 1;
        }
      }
      return { alive, born };
    });
}

test.describe("a session's views file", () => {
  test("draws memory read at once, as pixels, at each stop", async ({ page, uscope }) => {
    await life(page, uscope, path.join(root, "tests/fixtures/c/life/life.views"));
    await drawAt(page, "life.c:59");
    const board = card(page, "life");
    await expect(board.locator(".drawing-caption")).toHaveText("generation 1");
    await expect(board).toContainText("life.js");
    // A glider and a blinker: eight cells, none born before the first stop.
    expect(await cells(page)).toEqual({ alive: 8, born: 0 });

    await page.keyboard.press("F5");
    await expect(board.locator(".drawing-caption")).toHaveText("generation 2");
    const second = await cells(page);
    expect(second.alive).toBe(8);
    expect(second.born).toBeGreaterThan(0);

    // Drawing a value the frame already draws pins that card, which can
    // then stop being pinned.
    await page.getByRole("button", { name: "Draw life", exact: true }).first().click();
    await expect(page).toHaveURL(/[?&]d=life(&|$)/);
    await board.getByRole("button", { name: "Stop drawing life" }).click();
    await expect(page).not.toHaveURL(/[?&]d=/);
    await expect(board).toBeVisible();
  });

  test("a new session's first drawing has nothing before it", async ({ page, uscope }) => {
    await life(page, uscope, path.join(root, "tests/fixtures/c/life/life.views"));
    await drawAt(page, "life.c:59");
    await page.keyboard.press("F5");
    await expect(card(page, "life").locator(".drawing-caption")).toHaveText("generation 2");

    // The same program again, whose first board differs from the last one
    // drawn before it, at an earlier stop.
    await page.getByRole("button", { name: /Debug something/ }).click();
    const program = page.getByRole("combobox", { name: "Program" });
    await program.fill(fixture("life"));
    await page.keyboard.press("Escape");
    await page.getByRole("button", { name: "Load", exact: true }).click();
    await page.getByRole("button", { name: /End it and continue/ }).click();
    await expect(page.getByTestId("status")).toContainText("Not started");
    const adder = page.getByRole("textbox", { name: "Add a breakpoint" });
    await adder.fill("life.c:59");
    await adder.press("Enter");
    await adder.press("Escape");
    await page.keyboard.press("F5");
    await expect(page.getByTestId("stops")).toContainText("#4 breakpoint");
    await page.keyboard.press("Alt+v");
    await expect(card(page, "life").locator(".drawing-caption")).toHaveText("generation 1");
    expect(await cells(page)).toEqual({ alive: 8, born: 0 });
  });

  test("a pointer draws as what it points to", async ({ page, uscope }) => {
    await life(page, uscope, path.join(root, "tests/fixtures/c/life/life.views"));
    await drawAt(page, "step");
    await expect(card(page, "life").locator(".drawing-caption")).toHaveText("generation 0");
  });

  test("reloading views draws with the renderer as it is now", async ({ page, uscope }) => {
    const directory = test.info().outputPath("views");
    const { mkdir } = await import("node:fs/promises");
    await mkdir(directory, { recursive: true });
    const views = path.join(directory, "life.views");
    const renderer = path.join(directory, "life.js");
    await copyFile(path.join(root, "tests/fixtures/c/life/life.views"), views);
    await copyFile(path.join(root, "tests/fixtures/c/life/life.js"), renderer);
    await life(page, uscope, views);
    await drawAt(page, "life.c:59");
    const board = card(page, "life");
    await expect(board.locator(".drawing-caption")).toHaveText("generation 1");

    const source = await readFile(renderer, "utf8");
    await writeFile(renderer, source.replace("caption: `generation", "caption: `gen"));
    await page.keyboard.press("Control+k");
    await page.keyboard.type("Reload views");
    await page.keyboard.press("Enter");
    await expect(board.locator(".drawing-caption")).toHaveText("gen 1");
  });

  test("a card says why it cannot draw, and the page stays responsive", async ({
    page,
    uscope,
  }) => {
    const directory = test.info().outputPath("views");
    const { mkdir } = await import("node:fs/promises");
    await mkdir(directory, { recursive: true });
    const views = path.join(directory, "failing.views");
    await writeFile(
      views,
      `uscope-views 1
extend c life {
    visualize "throws" { generation = generation }
    visualize "forever" { generation = generation }
    visualize "invalid" { generation = generation }
    visualize "bitmap" {
        pixels = bytes(0x10, 64)
        columns = 8
    }
}
`,
    );
    await writeFile(
      path.join(directory, "throws.js"),
      // biome-ignore lint/suspicious/noTemplateCurlyInString: the renderer's own template
      "uscope.draw(({ generation }) => {\n  throw new RangeError(`no generation ${generation}`);\n});\n",
    );
    await writeFile(path.join(directory, "forever.js"), "uscope.draw(() => { for (;;) {} });\n");
    await writeFile(
      path.join(directory, "invalid.js"),
      `uscope.draw(() => uscope.picture({ width: 10, height: 10, shapes: [
  uscope.rect({ x: 0, y: 0, width: 10, height: 10, fill: "url(https://example.com/x)" }),
] }));
`,
    );
    await life(page, uscope, views);
    await drawAt(page, "life.c:59");
    const board = card(page, "life");
    const tab = (name: string) => board.getByRole("button", { name, exact: true });

    await tab("throws").click();
    await expect(board.getByRole("alert")).toHaveText("throws.js:2:9: RangeError: no generation 1");

    await tab("invalid").click();
    await expect(board.getByRole("alert")).toContainText(
      "invalid.js returned a picture the page cannot show: shape 0 (rect): `fill` is not a color",
    );

    await tab("bitmap").click();
    await expect(board.getByRole("alert")).toContainText("Cannot draw: `pixels`:");
    await expect(board.getByRole("alert")).toContainText("0x10");

    // A renderer that never returns is stopped, and the page answers meanwhile.
    await tab("forever").click();
    await expect(board.getByRole("alert")).toHaveText(
      "The renderer took longer than 2 s, so it was stopped.",
    );
    await page.keyboard.press("Alt+s");
    await expect(page).not.toHaveURL(/view=drawings/);

    // A renderer nobody provides is named.
    const url = new URL(page.url());
    url.searchParams.set("view", "drawings");
    url.searchParams.append("d", "life~nowhere");
    await page.goto(url.href);
    const nowhere = page.getByRole("region", { name: "Drawing of life as nowhere" });
    await nowhere.scrollIntoViewIfNeeded();
    await expect(nowhere.getByRole("alert")).toContainText("nowhere");
  });
});
