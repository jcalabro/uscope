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
  await expect(page.getByTestId("passed")).toBeVisible();

  await page.keyboard.press("?");
  const help = page.getByRole("dialog", { name: "Keys" });
  await expect(help).toContainText("Step over");
  await expect(help.getByRole("row", { name: /^Step over F10/ })).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(help).toHaveCount(0);
});
