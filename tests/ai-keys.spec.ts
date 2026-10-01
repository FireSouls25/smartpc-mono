import { test, expect } from "@playwright/test";
import { APP, seedUser } from "./helpers";

// No model and no real key needed: saving a bogus key must fail with a
// clear error inside the modal.
test.beforeEach(async ({ request, context }) => seedUser(request, context));

test("opencode without key opens the key modal; bogus key errors", async ({
  page,
}) => {
  await page.goto(APP, { waitUntil: "networkidle" });
  await expect(page.getByText("Chats").first()).toBeVisible({ timeout: 20000 });

  const menus = page.locator("[data-selectmenu]");
  await menus.nth(0).getByRole("button").click();
  // Exact: the catalog now also lists opencode-go next to opencode.
  await page.getByRole("option", { name: "opencode", exact: true }).click();

  // Centered modal asking for the key (not a silent select).
  await expect(page.getByRole("dialog")).toBeVisible({ timeout: 10000 });
  await page
    .getByPlaceholder(/Pega tu API key|Paste your API key/)
    .fill("badkey12");
  await page.getByRole("button", { name: /Guardar|Save/ }).click();
  await expect(page.getByText(/rejected|rechazado/i).first()).toBeVisible({
    timeout: 60000,
  });
  await page.screenshot({ path: "test-results/layout-keymodal.png" });
});
