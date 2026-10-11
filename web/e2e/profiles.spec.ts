// A profile explored in the page: the functions and their callers, a
// transform, the flame graph's zoom, and the timeline's range, each held
// in the address bar, so a reload or a link opens the same view.

import { execFileSync } from "node:child_process";
import * as path from "node:path";
import { expect, fixture, join, root, test } from "./server";

// The test serves a profile it records, not a program.
test.use({ program: [] });

test("a profile is explored, and its address opens the same view", async ({
  page,
  uscope,
}, info) => {
  const file = info.outputPath("calls.usprof");
  execFileSync(
    path.join(root, "target", "debug", "uscope"),
    ["record", "-o", file, "--function", "leaf", "--", fixture("profile-stacks-gcc-o0"), "1"],
    { stdio: "ignore" },
  );
  const served = await uscope.start([file]);
  await join(page, served.link);
  await expect(page).toHaveURL(/\/profile\/[0-9a-f]{16}$/);
  const accounting = page.getByTestId("accounting");
  await expect(accounting).toContainText("6 executions sampled");
  await expect(accounting).toContainText("each call of leaf");

  // leaf owns every sample, and its callers are middle, then outer.
  await page.getByRole("button", { name: "Functions" }).click();
  const functions = page.getByRole("listbox", { name: "Functions" });
  await expect(functions.getByRole("option").first()).toContainText("100.0%");
  await expect(functions.getByRole("option").first()).toContainText("leaf");
  await functions.getByRole("option").filter({ hasText: "leaf" }).click();
  await expect(page).toHaveURL(/fn=leaf/);
  const callers = page.getByRole("tree", { name: "Callers" }).getByRole("treeitem");
  await expect(callers.nth(0)).toContainText("leaf");
  await expect(callers.nth(1)).toContainText("middle");
  await expect(callers.nth(2)).toContainText("outer");

  // Merged away, middle gives its place to outer, in every view.
  await page.getByRole("button", { name: "Call tree" }).click();
  await page
    .getByRole("tree", { name: "Call tree" })
    .getByRole("treeitem")
    .filter({ hasText: "middle" })
    .first()
    .click();
  await page.getByTestId("details").getByRole("button", { name: "Merge" }).click();
  await expect(page).toHaveURL(/tf=merge:middle/);
  await expect(page.getByText("merge middle")).toBeVisible();
  const tree = page.getByRole("tree", { name: "Call tree" });
  await expect(tree.getByRole("treeitem").filter({ hasText: "leaf" }).first()).toBeVisible();
  await expect(tree.getByRole("treeitem").filter({ hasText: "middle" })).toHaveCount(0);

  // The address opens the same view.
  await page.reload();
  await expect(page.getByText("merge middle")).toBeVisible();
  await expect(page.getByRole("button", { name: "Call tree" })).toHaveAttribute(
    "aria-pressed",
    "true",
  );

  // A frame of the flame graph zooms, and says what it costs.
  await page.getByRole("button", { name: "Flame graph" }).click();
  const flame = page.getByRole("img", { name: "Flame graph" });
  const box = await flame.boundingBox();
  if (!box) {
    throw new Error("the flame graph has no box");
  }
  await page.mouse.move(box.x + 4, box.y + 4);
  await expect(page.getByTestId("flame-tip")).toContainText("total");
  await page.mouse.click(box.x + 4, box.y + 4);
  await expect(page).toHaveURL(/z=/);
  await expect(page.getByRole("button", { name: "Reset zoom" })).toBeVisible();

  // Dragging across the timeline selects a range, which a chip names.
  const lanes = page.getByTestId("timeline").getByRole("img");
  const area = await lanes.boundingBox();
  if (!area) {
    throw new Error("the timeline has no box");
  }
  await page.mouse.move(area.x + 2, area.y + 4);
  await page.mouse.down();
  await page.mouse.move(area.x + area.width / 2, area.y + 4, { steps: 4 });
  await page.mouse.up();
  await expect(page).toHaveURL(/range=\d+-\d+/);
  await expect(page.getByRole("button", { name: /^Remove .* – / })).toBeVisible();
});

test("a program recorded from the page grows live, stops, and saves", async ({ page, uscope }) => {
  await join(page, uscope.link);
  await page.goto(new URL("/pick?tab=profile", uscope.origin).href);
  await page.getByLabel("Program").fill(fixture("spin"));
  await page.getByRole("button", { name: "Record" }).click();
  await expect(page).toHaveURL(/\/profile\/[0-9a-f]{16}/);
  await expect(page.getByTestId("recording")).toContainText("Recording spin");

  // Its samples arrive while it runs.
  await page.getByRole("button", { name: "Functions" }).click();
  const functions = page.getByRole("listbox", { name: "Functions" });
  const main = functions
    .getByRole("option")
    .filter({ has: page.getByText("main", { exact: true }) });
  await expect(main).toBeVisible({ timeout: 10_000 });

  await page.getByRole("button", { name: "Stop" }).click();
  await expect(page.getByTestId("recording")).toContainText("spin ended: signal: 9");
  await expect(page.getByRole("button", { name: "Stop" })).toHaveCount(0);

  const saving = page.waitForEvent("download");
  await page.getByRole("button", { name: "Save" }).click();
  expect((await saving).suggestedFilename()).toBe("spin.usprof");
});
