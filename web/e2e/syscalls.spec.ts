// A system call trace explored in the page: what the calls cost, where the
// failed one came from, its line in the log and what it was passed, and the
// files the program used, each held in the address bar.

import { spawnSync } from "node:child_process";
import * as path from "node:path";
import { expect, fixture, join, root, test } from "./server";

// The test serves a trace it records, not a program.
test.use({ program: [] });

test("a trace is explored from its costs to one call's arguments and stack", async ({
  page,
  uscope,
}, info) => {
  const file = info.outputPath("files.usprof");
  // The program runs in the output directory, where it makes its files,
  // and exits with 3.
  const traced = spawnSync(
    path.join(root, "target", "debug", "uscope"),
    ["syscalls", "--engine", "ptrace", "-f", "-k", "-o", file, "--", fixture("syscalls"), "files"],
    { cwd: info.outputDir, stdio: "ignore" },
  );
  expect(traced.status).toBe(3);
  const served = await uscope.start([file]);
  await join(page, served.link);

  // A trace alone opens in its own view, without sampling's controls.
  const view = page.getByTestId("syscalls");
  await expect(page.getByTestId("syscall-counts")).toContainText("ptrace");
  await expect(page.getByTestId("accounting")).toContainText("calls traced");
  await expect(page.getByText("Inverted")).toHaveCount(0);
  await expect(view.getByRole("img", { name: "Each thread's time in system calls" })).toBeVisible();

  // Narrowed to failed opens, the one call left is placed at its line.
  await page.getByLabel("Selection").fill("trace=openat status=failed");
  await page.getByLabel("Selection").press("Enter");
  await expect(page).toHaveURL(/sf=trace%3Dopenat/);
  const table = page.getByTestId("syscall-table");
  await expect(table.locator("tbody tr")).toHaveCount(1);
  await table.locator('tr[data-row="openat"] td').nth(1).click();
  await expect(page).toHaveURL(/sc=openat/);
  const detail = page.getByTestId("syscall-detail");
  await expect(detail).toContainText("ENOENT");
  await expect(page.getByTestId("syscall-callers")).toContainText("files · syscalls.c:23");
  await expect(detail.getByRole("img", { name: "Latency heat map" })).toBeVisible();

  // Its line reads as strace prints it, and opens to its arguments.
  await view.getByRole("button", { name: "Log", exact: true }).click();
  await expect(page).toHaveURL(/st=log/);
  const line = page
    .getByTestId("syscall-log")
    .getByRole("button")
    .filter({ hasText: '"missing", O_RDONLY) = -1 ENOENT' });
  await line.click();
  await expect(page).toHaveURL(/ev=\d+/);
  const call = page.getByTestId("syscall-call");
  await expect(call.getByRole("rowheader", { name: "filename" })).toBeVisible();
  await expect(call.getByRole("listitem").filter({ hasText: /^files$/ })).toBeVisible();

  // The address opens the same view.
  await page.reload();
  await expect(page.getByTestId("syscall-call")).toContainText('"missing"');

  // A selection the trace cannot name says why.
  await page.getByLabel("Selection").fill("trace=nosuchcall");
  await page.getByLabel("Selection").press("Enter");
  await expect(view.getByRole("alert")).toContainText("nosuchcall");

  // The file it wrote and read back, and the one it could not open.
  await page.getByLabel("Selection").fill("");
  await page.getByLabel("Selection").press("Enter");
  await view.getByRole("button", { name: "Files", exact: true }).click();
  const files = page.getByTestId("syscall-files");
  await expect(files.getByRole("cell", { name: /\/present$/ })).toHaveCount(1);
  await expect(files.getByRole("cell", { name: /\/missing$/ })).toHaveCount(1);

  // Dragging across the lanes selects a range, which a chip names.
  await view.getByRole("button", { name: "Summary", exact: true }).click();
  const lanes = view.getByRole("img", { name: "Each thread's time in system calls" });
  const area = await lanes.boundingBox();
  if (!area) {
    throw new Error("the lanes have no box");
  }
  await page.mouse.move(area.x + 2, area.y + 4);
  await page.mouse.down();
  await page.mouse.move(area.x + area.width / 2, area.y + 4, { steps: 4 });
  await page.mouse.up();
  await expect(page).toHaveURL(/range=\d+-\d+/);
});

test("a family's processes are listed parents first, each with how it ended", async ({
  page,
  uscope,
}, info) => {
  const file = info.outputPath("family.usprof");
  spawnSync(
    path.join(root, "target", "debug", "uscope"),
    ["syscalls", "--engine", "ptrace", "-f", "-o", file, "--", fixture("syscalls"), "family"],
    { cwd: info.outputDir, stdio: "ignore" },
  );
  const served = await uscope.start([file]);
  await join(page, served.link);
  await page.getByTestId("syscalls").getByRole("button", { name: "Processes" }).click();
  const rows = page.getByTestId("syscall-processes").locator("tbody tr");
  await expect(rows).toHaveCount(2);
  await expect(rows.nth(1)).toContainText("exited with 4");
});
