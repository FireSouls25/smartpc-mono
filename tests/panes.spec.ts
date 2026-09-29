import { test, expect, type Page } from "@playwright/test";
import { APP, SIDECAR, GATE, seedUser } from "./helpers";

// Resizable main layout: no overflow at desktop widths, the center keeps a
// usable minimum, dividers actually move panes, widths persist reloads.
test.beforeEach(async ({ request, context }) => seedUser(request, context));

test.use({ viewport: { width: 1920, height: 1080 } });

async function paneWidths(page: Page) {
  const box = async (id: string) =>
    (await page.getByTestId(id).boundingBox()) ?? { width: 0, x: 0 };
  const a = await box("pane-a");
  const b = await box("pane-b");
  const c = await box("pane-c");
  return { a: a.width, b: b.width, c: c.width };
}

test("panes fit fullscreen, center keeps its minimum", async ({ page }) => {
  await page.goto(APP, { waitUntil: "networkidle" });
  await expect(page.getByText("Chats").first()).toBeVisible({ timeout: 20000 });
  const { a, b, c } = await paneWidths(page);
  // Nothing overflows the row: right pane never swallows the center.
  expect(a).toBeGreaterThanOrEqual(200);
  expect(b).toBeGreaterThanOrEqual(360);
  expect(c).toBeGreaterThanOrEqual(220);
  expect(a + b + c).toBeLessThanOrEqual(1920);
  // Panes never paint outside their own box (the overlap backstop).
  const boxOf = (id: string) => page.getByTestId(id).boundingBox();
  const pa = await boxOf("pane-a");
  const pb = await boxOf("pane-b");
  const pc = await boxOf("pane-c");
  if (pa && pb && pc) {
    expect(pa.x + pa.width).toBeLessThanOrEqual(pb.x + 2);
    expect(pb.x + pb.width).toBeLessThanOrEqual(pc.x + 2);
  }
});

test("divider drag resizes and persists", async ({ page, browser }) => {
  await page.goto(APP, { waitUntil: "networkidle" });
  await expect(page.getByText("Chats").first()).toBeVisible({ timeout: 20000 });
  const before = await paneWidths(page);
  const divider = page.getByRole("separator").nth(1);
  // Press until the row acknowledges the drag: layout can still settle
  // under the cursor (async provider/session fills) and a 12px handle is
  // easy to miss. Retries only re-press; moves happen once, below.
  let pressed = false;
  for (let i = 0; i < 5 && !pressed; i++) {
    const d = (await divider.boundingBox()) ?? {
      x: 0,
      y: 0,
      width: 0,
      height: 0,
    };
    await page.mouse.move(d.x + d.width / 2, d.y + d.height / 2);
    await page.mouse.down();
    pressed = await page.evaluate(
      () => document.querySelector(".resizable-row.resizing") !== null,
    );
    if (!pressed) await page.mouse.up();
  }
  expect(pressed).toBe(true);
  const d = (await divider.boundingBox()) ?? { x: 0, y: 0 };
  // Divider C sits left of the sessions pane: dragging it right shrinks it.
  await page.mouse.move(d.x + 6, d.y + 400, { steps: 5 });
  await page.mouse.move(d.x + 156, d.y + 400, { steps: 10 });
  await page.mouse.up();
  // Settle: Svelte flushes DOM writes async and input dispatch can lag the
  // protocol round-trip — wait in-page until the pane AND storage agree.
  // Interval polling (not the rAF default): this page is intentionally
  // idle (static orb, reduced motion), so headless Chromium may produce no
  // frames and rAF-starve the predicate even when it already holds.
  await page.waitForFunction(
    (exp) => {
      const w =
        document
          .querySelector('[data-testid="pane-c"]')
          ?.getBoundingClientRect().width ?? 0;
      const s = JSON.parse(
        window.localStorage.getItem("smartpc.panes.v2") ?? "{}",
      ) as { right?: number };
      return (
        w < exp - 50 && typeof s.right === "number" && Math.abs(s.right - w) < 5
      );
    },
    before.c,
    { timeout: 15000, polling: 500 },
  );
  const after = await paneWidths(page);
  // Dragging the sessions divider right shrinks the right pane.
  expect(after.c).toBeLessThan(before.c - 50);
  // Widths persist (reload restores them through the mount clamp).
  const stored = (await page.evaluate(() =>
    JSON.parse(window.localStorage.getItem("smartpc.panes.v2") ?? "{}"),
  )) as { right?: number };
  expect(typeof stored.right).toBe("number");
  expect(Math.abs((stored.right as number) - after.c)).toBeLessThan(5);
  // Reload restores session + widths. Fresh login in a clean context: the
  // seeded init-script token is single-use by design (rotation reuse trips
  // detection), and init scripts re-run on every load, so no reload inside
  // the seeded context can ever restore — production keeps the rotated
  // token, the test mints a new one where no init script interferes.
  const tag = `r${Date.now()}${Math.floor(Math.random() * 1e6)}`;
  const gate = { "Content-Type": "application/json", "X-Sidecar-Token": GATE };
  await page.request.post(`${SIDECAR}/v1/auth/register`, {
    data: { email: `${tag}@test.co`, password: "correct-horse-1" },
    headers: gate,
  });
  const login = await page.request.post(`${SIDECAR}/v1/auth/login`, {
    data: { email: `${tag}@test.co`, password: "correct-horse-1" },
    headers: gate,
  });
  const fresh = ((await login.json()) as { tokens: { refresh_token: string } })
    .tokens.refresh_token;
  const ctx2 = await browser.newContext({
    viewport: { width: 1920, height: 1080 },
  });
  const page2 = await ctx2.newPage();
  await page2.goto(APP, { waitUntil: "networkidle" });
  await page2.evaluate(
    (t) => window.localStorage.setItem("smartpc.refresh", t),
    fresh,
  );
  // Same widths travel along so the mount clamp sees the dragged values.
  await page2.evaluate((w) => {
    const cur = JSON.parse(
      window.localStorage.getItem("smartpc.panes.v2") ?? "{}",
    );
    window.localStorage.setItem(
      "smartpc.panes.v2",
      JSON.stringify({ ...cur, right: w }),
    );
  }, after.c);
  await page2.reload({ waitUntil: "networkidle" });
  await expect(page2.getByText("Chats").first()).toBeVisible({
    timeout: 30000,
  });
  const persisted = await paneWidths(page2);
  expect(Math.abs(persisted.c - after.c)).toBeLessThan(5);
  await ctx2.close();
});

test("side panes collapse to rails and come back", async ({ page }) => {
  await page.goto(APP, { waitUntil: "networkidle" });
  await expect(page.getByText("Chats").first()).toBeVisible({ timeout: 20000 });
  const paneA = page.getByTestId("pane-a");
  await paneA.getByRole("button", { name: /Ocultar|Hide/ }).click();
  await expect(page.getByTestId("rail-a")).toBeVisible();
  await expect(paneA).toBeHidden();
  // Visibility persists alongside widths.
  const stored = await page.evaluate(() =>
    JSON.parse(window.localStorage.getItem("smartpc.panes.v2") ?? "{}"),
  );
  expect(stored.showLeft).toBe(false);
  await page.getByTestId("rail-a").click();
  await expect(paneA).toBeVisible();
});
