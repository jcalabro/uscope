// The command palette, key help, and themes.

import type { Page } from "@playwright/test";
import { stopInHandleRequest } from "./kvstore";
import { expect, fixture, join, test } from "./server";

test.use({ program: [fixture("kvstore")] });

const palette = (page: Page) => page.getByRole("dialog", { name: "Command palette" });
const search = (page: Page) => palette(page).getByRole("combobox");

test("the palette finds functions, files, lines, and commands", async ({ page, uscope }) => {
  await join(page, uscope.link);
  await expect(page.locator(".cm-line").first()).toBeVisible();

  // A function opens where it is declared.
  await page.keyboard.press("Control+k");
  await search(page).fill("hreq");
  await expect(
    palette(page)
      .getByRole("option", { name: /handle_request/ })
      .first(),
  ).toBeVisible();
  await page.keyboard.press("Enter");
  await expect(palette(page)).toHaveCount(0);
  await expect(page).toHaveURL(/[?&]src=[^&]*kvstore\.c:89/);

  // Each function offers a breakpoint.
  await page.keyboard.press("Control+k");
  await search(page).fill("table_find");
  await palette(page).getByRole("option", { name: "Break at table_find" }).click();
  await expect(page.getByTestId("breakpoints")).toContainText("table_find");

  // Ctrl+P starts at files, Ctrl+G at a line of the file shown.
  await page.keyboard.press("Control+p");
  await search(page).fill("kvst");
  await expect(palette(page).getByRole("option").first()).toContainText("kvstore.c");
  await page.keyboard.press("Enter");
  await expect(page).toHaveURL(/[?&]src=[^&]*kvstore\.c:1(&|$)/);
  await page.keyboard.press("Control+g");
  await search(page).fill("120");
  await page.keyboard.press("Enter");
  await expect(page).toHaveURL(/[?&]src=[^&]*kvstore\.c:120(&|$)/);

  // Commands run from it, and Escape closes it without running anything.
  await page.keyboard.press("Control+k");
  await search(page).fill("theme dark");
  await page.keyboard.press("Enter");
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
  // The toolbar says so, and its next choice follows from it.
  await expect(page.getByRole("button", { name: "Theme: dark" })).toBeVisible();
  await page.reload();
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
  await expect(page.locator(".cm-line").first()).toBeVisible();
  await page.keyboard.press("Control+k");
  await search(page).fill("continue");
  await page.keyboard.press("Escape");
  await expect(palette(page)).toHaveCount(0);
  await expect(page.getByTestId("status")).toContainText("Not started");
});

test("every stop is in the palette, and ? lists every key", async ({ page, uscope }) => {
  await join(page, uscope.link);
  await stopInHandleRequest(page);
  await page.keyboard.press("F5");
  await expect(page.getByTestId("stops")).toContainText("#3");

  await page.keyboard.press("Control+k");
  await search(page).fill("#2");
  await palette(page)
    .getByRole("option", { name: /#2 breakpoint/ })
    .click();
  await expect(page).toHaveURL(/\/stop\/2\//);
  await expect(page.getByTestId("passed")).toContainText("Stop #2 has passed");

  // A stop the session never reached is not one that passed.
  await page.goto(page.url().replace(/\/stop\/2\//, "/stop/99/"));
  await expect(page.getByTestId("passed")).toContainText("This session has not reached stop #99");

  await page.keyboard.press("?");
  const help = page.getByRole("dialog", { name: "Keys" });
  await expect(help).toContainText("Step over");
  await expect(help.getByRole("row", { name: /^Step over F10/ })).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(help).toHaveCount(0);
});

test("closing the file shown shows its neighbor, and a middle click closes a tab", async ({
  page,
  uscope,
}) => {
  await join(page, uscope.link);
  const files = page.getByRole("navigation", { name: "Open files" });
  const tab = (name: string) => files.getByRole("button", { name, exact: true });
  await expect(tab("kvstore.c")).toBeVisible();
  for (const name of ["pthread.h", "stdio.h"]) {
    await page.keyboard.press("Control+p");
    await search(page).fill(name);
    await expect(palette(page).getByRole("option").first()).toContainText(name);
    await page.keyboard.press("Enter");
    await expect(tab(name)).toHaveAttribute("aria-current", "page");
  }

  await tab("stdio.h").click({ button: "middle" });
  await expect(tab("stdio.h")).toHaveCount(0);
  await expect(tab("pthread.h")).toHaveAttribute("aria-current", "page");
  await expect(page).toHaveURL(/[?&]src=[^&]*pthread\.h:1(&|$)/);

  await files.getByRole("button", { name: "Close pthread.h" }).click();
  await expect(tab("pthread.h")).toHaveCount(0);
  await expect(page).toHaveURL(/[?&]src=[^&]*kvstore\.c:1(&|$)/);

  // The last file closed leaves nothing shown.
  await files.getByRole("button", { name: "Close kvstore.c" }).click();
  await expect(tab("kvstore.c")).toHaveCount(0);
  await expect(page.locator(".cm-line")).toHaveCount(0);
  await expect(page.getByText("No file open.")).toBeVisible();
});

test("a new stop opens its file again, even on the line it was closed at", async ({
  page,
  uscope,
}) => {
  await join(page, uscope.link);
  await stopInHandleRequest(page);
  const files = page.getByRole("navigation", { name: "Open files" });
  await files.getByRole("button", { name: "Close kvstore.c" }).click();
  await expect(page.getByText("No file open.")).toBeVisible();

  await page.keyboard.press("F5");
  await expect(page.getByTestId("stops")).toContainText("#3");
  await expect(files.getByRole("button", { name: "kvstore.c", exact: true })).toBeVisible();
  await expect(page.locator(".cm-pc-line")).toContainText("int status = 0;");
});
