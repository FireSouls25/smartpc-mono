import { test, expect, type Page } from "@playwright/test";
import { APP, seedUser } from "./helpers";

// Static two-pane main row (§14.1: chat + actions; history moved to
// Settings). No dividers, no rails, no persisted geometry: pane-a is a
// fixed 320px rail, pane-b (chat) takes the remainder.
test.beforeEach(async ({ request, context }) => seedUser(request, context));

test.use({ viewport: { width: 1920, height: 1080 } });

async function paneWidths(page: Page) {
  const box = async (id: string) =>
    (await page.getByTestId(id).boundingBox()) ?? { width: 0, x: 0 };
  const a = await box("pane-a");
  const b = await box("pane-b");
  return { a: a.width, b: b.width, ax: a.x, bx: b.x };
}

test("two panes fit fullscreen, chat takes the remainder", async ({ page }) => {
  await page.goto(APP, { waitUntil: "networkidle" });
  await expect(page.getByText("Actividad").first()).toBeVisible({
    timeout: 20000,
  });
  const { a, b, ax, bx } = await paneWidths(page);
  // Actions rail is fixed at 320px; chat fills the rest, never squeezed.
  expect(a).toBeGreaterThanOrEqual(300);
  expect(a).toBeLessThanOrEqual(340);
  expect(b).toBeGreaterThanOrEqual(360);
  expect(a + b).toBeLessThanOrEqual(1920);
  // Rail sits left of chat, no overlap.
  expect(ax + a).toBeLessThanOrEqual(bx + 2);
  // No third pane, no dividers, no rails: the sessions era is over.
  await expect(page.getByTestId("pane-c")).toHaveCount(0);
  await expect(page.getByRole("separator")).toHaveCount(0);
  await expect(page.getByText("Chats").first()).toHaveCount(0);
});
