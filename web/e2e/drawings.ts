// Steps the specs that draw values share.

import type { Page } from "@playwright/test";
import { expect } from "./server";

/** Breaks at `location`, runs there, and shows the drawings. */
export async function drawAt(page: Page, location: string, stop = "#2"): Promise<void> {
  const adder = page.getByRole("textbox", { name: "Add a breakpoint" });
  await adder.fill(location);
  await adder.press("Enter");
  await expect(page.getByTestId("breakpoints")).toContainText(location);
  await adder.press("Escape");
  await page.keyboard.press("F5");
  await expect(page.getByTestId("stops")).toContainText(stop);
  await page.keyboard.press("Alt+v");
  await expect(page).toHaveURL(/[?&]view=drawings/);
}

/** The card that draws `path`. */
export const card = (page: Page, path: string) =>
  page.getByRole("region", { name: `Drawing of ${path}`, exact: true });
