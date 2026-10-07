// Running a program from the page with VS Code's keys, and choosing another.

import { expect, fixture, join, test } from "./server";

test("F5, F6, and Shift+F5 run, pause, and kill without the browser acting", async ({
  page,
  uscope,
}) => {
  await join(page, uscope.link);
  const status = page.getByTestId("status");
  // A reload would lose this marker.
  await page.evaluate(() => {
    (window as { marker?: boolean }).marker = true;
  });
  await page.keyboard.press("F5");
  await expect(status).toContainText("Running");
  await page.keyboard.press("F6");
  await expect(status).toContainText("Stopped");
  await expect(page.getByRole("region", { name: "Threads" })).toContainText("stopped here");
  await page.keyboard.press("F5");
  await expect(status).toContainText("Running");
  await page.keyboard.press("Shift+F5");
  await expect(status).toContainText("Exited");
  await expect(status).toContainText("SIGKILL");
  expect(await page.evaluate(() => (window as { marker?: boolean }).marker)).toBe(true);
});

test("reloading keeps the session and its output", async ({ page, uscope }) => {
  await uscope.stop();
  const server = await uscope.start([fixture("output-streams")]);
  await join(page, server.link);
  await page.keyboard.press("F5");
  // It reads its input to the end, which Ctrl+D closes.
  await page.getByRole("textbox", { name: "Program input" }).press("Control+d");
  await expect(page.getByTestId("status")).toContainText("Exited");
  const output = page.getByTestId("output");
  await expect(output).toContainText("stdin: eof");
  const url = page.url();
  await page.reload();
  await expect(page).toHaveURL(url);
  await expect(output).toContainText("stdin: eof");
  await expect(page.getByTestId("status")).toContainText("Exited");
});

test("the picker completes a path and loads it in place of the program", async ({
  page,
  uscope,
}) => {
  await join(page, uscope.link);
  const first = page.url();
  const watcher = await page.context().newPage();
  await watcher.goto(first);
  await expect(watcher.getByTestId("status")).toContainText("Not started");
  await page.getByRole("button", { name: /Debug something/ }).click();
  await expect(page).toHaveURL(/\/pick$/);
  const program = page.getByRole("combobox", { name: "Program" });
  await program.pressSequentially("build/test-prog");
  await page.keyboard.press("Tab");
  await expect(program).toHaveValue("build/test-programs/");
  await program.pressSequentially("basic");
  await page.getByRole("option", { name: /^basic\s*executable$/ }).click();
  await expect(program).toHaveValue("build/test-programs/basic");
  await page.getByRole("button", { name: "Load", exact: true }).click();
  // Spin is still loaded, so loading asks first.
  await page.getByRole("button", { name: /End it and continue/ }).click();
  await expect(page).toHaveURL(/\/s\/[0-9a-f]+$/);
  expect(page.url()).not.toBe(first);
  await expect(page).toHaveTitle(/^basic/);

  // A tab that watched the old session follows to the new one; a link to
  // it opened later says it ended, and offers the current one.
  await expect(watcher).toHaveURL(page.url());
  const late = await page.context().newPage();
  await late.goto(first);
  await expect(late.getByTestId("session-ended")).toBeVisible();
  await late.getByTestId("session-ended").getByRole("link").click();
  await expect(late).toHaveURL(page.url());
});
