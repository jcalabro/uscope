// Getting into a session: links, cookies, and what a tab shows without one.

import { expect, expectErrors, join, test } from "./server";

test("a join link opens the session and leaves no token in the address", async ({
  page,
  uscope,
}) => {
  await join(page, uscope.link);
  expect(page.url()).not.toContain("#");
  await expect(page.locator(".status")).toContainText("Not started");
  await expect(page).toHaveTitle(/spin/);
});

test("a cookie from an earlier uscope on the same port asks for a link", async ({
  page,
  uscope,
}) => {
  await join(page, uscope.link);
  // A later server on this port mints new tokens: copy the old cookie there.
  const next = await uscope.start([]);
  const port = new URL(next.origin).port;
  const [old] = await page.context().cookies(uscope.origin);
  await page
    .context()
    .addCookies([{ name: `uscope-${port}`, value: old?.value ?? "", url: next.origin }]);
  expectErrors(`POST ${next.origin}/api/check: 403`);
  await page.goto(`${next.origin}/`);
  await expect(page.getByRole("heading", { name: "Open a join link" })).toBeVisible();
  // Nothing here can control a program it cannot see.
  await expect(page.locator("[data-action]")).toHaveCount(0);
});
