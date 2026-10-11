// A profile explored in the page: the functions and their callers, a
// transform, the flame graph's zoom, and the timeline's range, each held
// in the address bar, so a reload or a link opens the same view.

import { execFileSync, spawn } from "node:child_process";
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

test("each thread's lane says how long it was off its CPU, and the footer what recording cost", async ({
  page,
  uscope,
}, info) => {
  const file = info.outputPath("off-cpu.usprof");
  execFileSync(
    path.join(root, "target", "debug", "uscope"),
    ["record", "-o", file, "--", fixture("profile-offcpu")],
    { stdio: "ignore" },
  );
  const served = await uscope.start([file]);
  await join(page, served.link);
  const lanes = page.getByTestId("timeline");
  // The sleeper naps five times for 20 ms; the spinners share one CPU.
  await expect(lanes.getByRole("button", { name: /^sleeper/ })).toHaveAttribute(
    "title",
    /off its CPU (\d{3} ms|[\d.]+ s) blocked/,
  );
  const spinners = lanes.getByRole("button", { name: /^spinner/ });
  await expect(spinners).toHaveCount(2);
  for (const spinner of await spinners.all()) {
    await expect(spinner).toHaveAttribute("title", /, (?!0 ns)[\d.]+ \S+ preempted$/);
  }
  await expect(page.getByTestId("accounting")).toContainText(/uscope [\d.]+% of a CPU/);
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

test("a profile shows where tasks waited, by task, and a lane per task", async ({
  page,
  uscope,
}, info) => {
  // The program waits on its standard input the whole time it is recorded.
  const file = info.outputPath("waits.usprof");
  const program = spawn(fixture("profile-go-waits"), [], { stdio: ["pipe", "ignore", "ignore"] });
  const exited = new Promise((done) => program.once("exit", done));
  try {
    execFileSync(
      path.join(root, "target", "debug", "uscope"),
      ["record", "-o", file, "--pid", String(program.pid), "--task-snapshots", "0.05"].concat([
        "--duration",
        "2",
      ]),
      { stdio: "ignore" },
    );
  } finally {
    program.stdin?.end("x");
    await exited;
  }
  const served = await uscope.start([file]);
  await join(page, served.link);
  await expect(page.getByTestId("accounting")).toContainText("task snapshots");

  // The waits' functions are the receivers', and by task each is its own.
  await page.getByRole("button", { name: "Waits" }).click();
  await expect(page).toHaveURL(/src=waits/);
  await page.getByRole("button", { name: "Functions" }).click();
  const functions = page.getByRole("listbox", { name: "Functions" });
  await expect(functions.getByRole("option").filter({ hasText: "main.receive" })).toHaveCount(1);
  await page.getByRole("checkbox", { name: "By task" }).check();
  await page.getByRole("button", { name: "Call tree" }).click();
  const tree = page.getByRole("tree", { name: "Call tree" }).getByRole("treeitem");
  await expect(tree.first()).toContainText("goroutine");

  // The CPU's lanes by task: the spinner's, which selects its samples.
  await page.getByRole("button", { name: "CPU" }).click();
  await page.getByRole("checkbox", { name: "By task" }).uncheck();
  await page.getByRole("checkbox", { name: "Task lanes" }).check();
  const spinner = page.getByTestId("timeline").getByRole("button", { name: /phase=spin/ });
  await spinner.click();
  await expect(page).toHaveURL(/tk=\d+/);
  await expect(
    page.getByRole("button", { name: /Remove goroutine \d+ \{phase=spin\}/ }),
  ).toBeVisible();
  await expect(tree.filter({ hasText: "main.spin" }).first()).toBeVisible();
});

test("a program's zones show in tracks, and Find Zone measures one and filters by a label", async ({
  page,
  uscope,
}, info) => {
  const file = info.outputPath("zones.usprof");
  execFileSync(
    path.join(root, "target", "debug", "uscope"),
    ["record", "-o", file, "--", fixture("probes"), "zones"],
    { stdio: "ignore" },
  );
  const served = await uscope.start([file]);
  await join(page, served.link);
  await expect(page.getByTestId("accounting")).toContainText("probe events, lost 0");
  await page.getByRole("button", { name: "Zones", exact: true }).click();
  await expect(page).toHaveURL(/view=zones/);
  await expect(page.getByRole("img", { name: /^Zones over/ })).toBeVisible();
  await expect(page.getByRole("img", { name: "Plot round, 3 values from 0 – 2" })).toBeVisible();

  // A site's zones, measured; its longest selects its span.
  const find = page.getByTestId("find-zone");
  await find.getByRole("button", { name: "outer", exact: true }).click();
  await expect(page).toHaveURL(/zs=outer/);
  const stats = page.getByTestId("zone-stats");
  await expect(stats.locator("dd").first()).toHaveText("3");
  await stats.getByRole("listitem").first().getByRole("button").click();
  await expect(page).toHaveURL(/range=\d+-\d+/);

  // A label's values are filters of every view: in that span, the round
  // around it.
  await find.getByRole("button", { name: "round", exact: true }).click();
  const value = stats.getByRole("button", { name: /^round=\d$/ });
  await expect(value).toHaveCount(1);
  const round = (await value.textContent()) ?? "";
  await value.click();
  await expect(page).toHaveURL(new RegExp(`lb=${encodeURIComponent(round)}`));
  await expect(page.getByRole("button", { name: `Remove ${round}` })).toBeVisible();
  await stats.getByRole("button", { name: "By round" }).click();
  await expect(page).toHaveURL(/bl=round/);
  await expect(page.getByRole("combobox", { name: "Root by label" })).toHaveValue("round");
});

test("a Go program's own pprof profile opens in its unit, without a timeline", async ({
  page,
  uscope,
}, info) => {
  const allocs = info.outputPath("allocs.pb.gz");
  execFileSync(fixture("pprof-files"), [allocs, info.outputPath("goroutine.pb.gz")], {
    stdio: "ignore",
  });
  const served = await uscope.start([allocs]);
  await join(page, served.link);
  await expect(page.getByTestId("untimed")).toContainText("pprof's alloc_space (bytes)");
  await page.getByRole("button", { name: "Functions" }).click();
  const allocate = page
    .getByRole("listbox", { name: "Functions" })
    .getByRole("option")
    .filter({ hasText: "main.allocate" });
  await expect(allocate).toHaveAttribute("title", "62.5 KiB");
});

test("a profile is diffed with a range of its own, and with another profile", async ({
  page,
  uscope,
}, info) => {
  const recorded = (symbol: string, name: string) => {
    const file = info.outputPath(name);
    execFileSync(
      path.join(root, "target", "debug", "uscope"),
      ["record", "-o", file, "--function", symbol, "--", fixture("profile-stacks-gcc-o0"), "1"],
      { stdio: "ignore" },
    );
    return file;
  };
  const served = await uscope.start([
    recorded("leaf", "calls.usprof"),
    recorded("middle", "middle.usprof"),
  ]);
  await join(page, served.link);

  // The selected range becomes the base, and nothing changed against it.
  const lanes = page.getByTestId("timeline").getByRole("img");
  const area = await lanes.boundingBox();
  if (!area) {
    throw new Error("the timeline has no box");
  }
  await page.mouse.move(area.x + 1, area.y + 4);
  await page.mouse.down();
  await page.mouse.move(area.x + area.width - 1, area.y + 4, { steps: 4 });
  await page.mouse.up();
  await expect(page).toHaveURL(/range=\d+-\d+/);
  const compare = page.getByRole("combobox", { name: "Compare with" });
  await compare.selectOption("range");
  await expect(page).toHaveURL(/br=\d+-\d+/);
  await expect(page).not.toHaveURL(/[?&]range=/);
  await page.getByRole("button", { name: "Functions" }).click();
  const functions = page.getByRole("listbox", { name: "Functions" });
  const row = (name: string) =>
    functions.getByRole("option").filter({ has: page.getByText(name, { exact: true }) });
  await expect(row("leaf")).toContainText("±0");

  // Against the calls of middle, leaf is all new and middle all gone.
  await page.getByRole("button", { name: /^Remove diff with this profile/ }).click();
  await expect(page).not.toHaveURL(/br=/);
  await compare.selectOption({ label: "diff with middle.usprof" });
  await expect(page).toHaveURL(/bp=[0-9a-f]{16}/);
  await expect(row("leaf")).toContainText("+100.0 pp");
  await expect(row("middle")).toContainText("−100.0 pp");
  await expect(page.getByRole("button", { name: "by change" })).toBeVisible();
});
