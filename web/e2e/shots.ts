// Screenshots of every screen in light and dark at a few widths, for
// reviewing the look without clicking through: `just web-shot [PROGRAM]`.

import { mkdir } from "node:fs/promises";
import * as path from "node:path";
import { chromium, type Page } from "@playwright/test";
import { fixture, root, startUscope } from "./server.ts";

const out = path.join(root, "target", "web-shots");
const program = process.argv[2] ?? fixture("threads");
const widths = [1440, 1024, 720];

async function shoot(page: Page, name: string): Promise<void> {
  for (const scheme of ["dark", "light"] as const) {
    await page.emulateMedia({ colorScheme: scheme });
    for (const width of widths) {
      await page.setViewportSize({ width, height: 800 });
      await page.screenshot({ path: path.join(out, `${name}-${scheme}-${width}.png`) });
    }
  }
  console.log(name);
}

await mkdir(out, { recursive: true });
const server = await startUscope([program], "");
const browser = await chromium.launch();
try {
  const page = await browser.newPage({ viewport: { width: 1440, height: 800 } });
  const status = page.getByTestId("status");
  await page.goto(server.link);
  await page.waitForURL(/\/s\//);
  await shoot(page, "1-loaded");

  await page.keyboard.press("F5");
  await status.getByText("Running").waitFor();
  await page.waitForTimeout(1000);
  await shoot(page, "2-running");

  await page.keyboard.press("F6");
  await status.getByText("Stopped").waitFor();
  await shoot(page, "3-stopped");

  await page.getByRole("button", { name: "Share" }).click();
  await page
    .getByRole("textbox", { name: "Join link" })
    .and(page.locator("[value*=join]"))
    .waitFor();
  await shoot(page, "4-share");
  await page.keyboard.press("Escape");

  await page.getByRole("button", { name: /Debug something/ }).click();
  await page.getByRole("combobox", { name: "Program" }).pressSequentially("build/test-programs/");
  await page.getByRole("option").first().waitFor();
  await shoot(page, "5-picker");
  await page.getByRole("button", { name: "Attach" }).click();
  await page.waitForTimeout(300);
  await shoot(page, "6-attach");

  const viewer = await browser.newPage({ viewport: { width: 1440, height: 800 } });
  await page.goto(page.url().replace(/\/pick.*/, "/"));
  await page.getByRole("button", { name: "Share" }).click();
  const field = page.getByRole("textbox", { name: "Join link" });
  await field.and(page.locator("[value*=join]")).waitFor();
  await viewer.goto(await field.inputValue());
  await viewer.getByTestId("status").getByText("Stopped").waitFor();
  await shoot(viewer, "7-viewer");

  await page.keyboard.press("Escape");
  await page.keyboard.press("Shift+F5");
  await status.getByText("Exited").waitFor();
  await shoot(page, "8-exited");
} finally {
  await browser.close();
  await server.stop();
}
console.log(`screenshots in ${out}`);
