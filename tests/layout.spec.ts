import { test, expect } from "@playwright/test";
import { APP, SIDECAR, GATE, seedUser, accessTokenFor } from "./helpers";

let creds: { email: string; password: string };

test.beforeEach(async ({ request, context }) => {
  creds = await seedUser(request, context);
});

test("shell locks to viewport; panes scroll internally", async ({ page }) => {
  await page.goto(APP, { waitUntil: "networkidle" });
  // Logged in via seeded refresh token: main shell with its three panes.
  await expect(page.getByText("Actividad").first()).toBeVisible({
    timeout: 20000,
  });
  await expect(page.getByText("Actividad").first()).toBeVisible();

  const before = await page.evaluate(() => ({
    pageScrollH: document.documentElement.scrollHeight,
    innerH: window.innerHeight,
  }));
  expect(before.pageScrollH).toBeLessThanOrEqual(before.innerH + 1);

  // Simulate a long chat: inject 60 bubbles into the chat scroller
  // (the orb's status span shares the aria-live attr, hence the div scope).
  const scroller = page.locator('div[aria-live="polite"]');
  await scroller.evaluate((el) => {
    for (let i = 0; i < 60; i++) {
      const d = document.createElement("div");
      d.className = "msg-in flex justify-end";
      const p = document.createElement("p");
      p.className = "bubble-user";
      p.textContent = `mensaje de prueba ${i} con algo de texto para ocupar líneas y forzar un overflow real del contenedor`;
      d.appendChild(p);
      el.appendChild(d);
    }
  });
  await page.waitForTimeout(400);

  const after = await page.evaluate(() => {
    const el = document.querySelector(
      'div[aria-live="polite"]',
    ) as HTMLElement | null;
    return {
      pageScrollH: document.documentElement.scrollHeight,
      innerH: window.innerHeight,
      scrollH: el ? el.scrollHeight : -1,
      clientH: el ? el.clientHeight : -1,
    };
  });
  // Page must NOT grow; the scroller must absorb the overflow.
  expect(after.pageScrollH).toBeLessThanOrEqual(after.innerH + 1);
  expect(after.scrollH).toBeGreaterThan(after.clientH + 200);
  await page.screenshot({ path: "test-results/layout-chat.png" });
});

test("opening a session never touches the global selection", async ({
  page,
  request,
}) => {
  // Seed a session directly: browsing history must be view-only (no
  // POST /v1/ai/select), with an explicit adopt instead.
  const token = await accessTokenFor(request, creds.email, creds.password);
  const created = await request.post(`${SIDECAR}/v1/chat/sessions`, {
    data: { title: "History probe" },
    headers: {
      "Content-Type": "application/json",
      "X-Sidecar-Token": GATE,
      Authorization: `Bearer ${token}`,
    },
  });
  expect(created.ok()).toBeTruthy();

  let selects = 0;
  page.on("request", (r) => {
    // Exact POST /select — must not match GET /selection.
    if (r.method() === "POST" && r.url().endsWith("/v1/ai/select")) selects++;
  });

  await page.goto(APP, { waitUntil: "networkidle" });
  await expect(page.getByText("Actividad").first()).toBeVisible({
    timeout: 20000,
  });
  // History lives in Settings now (§14.1): browse it there, read-only.
  await page.getByRole("button", { name: /Ajustes|Settings/ }).click();
  await page.getByText("Historial").click();
  expect(page.url()).toContain("#/settings/4");
  await page.getByText("History probe").click();
  // Back home: empty session loads (greeting replaced by nothing), the
  // adopt affordance appears (combo recorded), and no selection
  // round-trip happened (old code POSTed here).
  await page.getByRole("button", { name: /Volver|Back/ }).click();
  await expect(page.getByText(/qué hacemos|what shall/i)).toHaveCount(0, {
    timeout: 10000,
  });
  await expect(page.getByText(/Usar modelo|Use session/)).toBeVisible({
    timeout: 10000,
  });
  await page.waitForTimeout(1000);
  expect(selects).toBe(0);
});

test("settings route deep-links sections", async ({ page }) => {
  await page.goto(APP, { waitUntil: "networkidle" });
  await expect(page.getByText("Actividad").first()).toBeVisible({
    timeout: 20000,
  });

  // Gear → #/settings (general section, sidebar port renders).
  await page.getByRole("button", { name: /Ajustes|Settings/ }).click();
  await expect(
    page.getByRole("heading", { name: /Ajustes|Settings/ }),
  ).toBeVisible();
  await expect(page.getByText("General", { exact: true })).toBeVisible();

  // Sidebar hop → #/settings/1 (AI section, dot follows).
  await page.getByText(/Modelo de IA|AI model/).click();
  await expect(page.getByText(/Proveedor|Provider/).first()).toBeVisible();
  expect(page.url()).toContain("#/settings/1");
  await page.screenshot({ path: "test-results/layout-settings-ai.png" });

  // Voice section renders its controls without a mic present.
  await page
    .getByText(/Voz|Voice/)
    .first()
    .click();
  await expect(page.getByText(/Modo|Mode/).first()).toBeVisible();
  await expect(page.getByPlaceholder("hey")).toBeVisible();
  await expect(page.getByText(/Entrada|Input/).first()).toBeVisible();
  await expect(
    page.getByText(/Leer respuestas|Read replies/).first(),
  ).toBeVisible();
  expect(page.url()).toContain("#/settings/2");

  // Device picker: opens, lists deduped options (duplicate ALSA names
  // used to crash the keyed each block → empty menu), persists selection.
  const pageErrors: string[] = [];
  page.on("pageerror", (e) => pageErrors.push(String(e).slice(0, 120)));
  await page.getByRole("button", { name: /Entrada|Input/ }).click();
  const deviceOptions = page.getByRole("option");
  await expect(deviceOptions.first()).toBeVisible({ timeout: 5000 });
  expect(await deviceOptions.count()).toBeGreaterThanOrEqual(1);
  if ((await deviceOptions.count()) >= 2) {
    await deviceOptions.nth(1).click();
    await expect
      .poll(
        () =>
          page.evaluate(() =>
            window.localStorage.getItem("smartpc.voice.device"),
          ),
        { timeout: 5000 },
      )
      .not.toBeNull();
  }
  expect(pageErrors.filter((e) => e.includes("each_key"))).toEqual([]);
  await page.screenshot({ path: "test-results/layout-settings-voice.png" });

  // Unknown hashes fall back home.
  await page.goto(`${APP}#/nope`, { waitUntil: "networkidle" });
  await expect(page.getByText("Actividad").first()).toBeVisible({
    timeout: 20000,
  });

  // Back button leaves settings for the main shell.
  await page.getByRole("button", { name: /Ajustes|Settings/ }).click();
  await page.getByRole("button", { name: /Volver|Back/ }).click();
  await expect(page.getByText("Actividad").first()).toBeVisible();
  await page.screenshot({ path: "test-results/layout-settings.png" });
});

test(
  "real send persists and appears in sessions",
  { tag: "@slow" },
  async ({ page }) => {
    // Cold VRAM loads of multi-GB models are slow: allow generously.
    test.setTimeout(360000);
    await page.goto(APP, { waitUntil: "networkidle" });
    await expect(page.getByText("Actividad").first()).toBeVisible({
      timeout: 20000,
    });

    // Use a model that truly exists here (the default may not be installed).
    const detected = await page.evaluate(
      async ({ sidecar, gate }) => {
        const r = await fetch(`${sidecar}/v1/ai/providers`, {
          headers: { "X-Sidecar-Token": gate },
        });
        return (await r.json()) as {
          providers: { id: string; available: boolean; models: string[] }[];
        };
      },
      { sidecar: SIDECAR, gate: GATE },
    );
    const ollama = detected.providers.find((p) => p.id === "ollama");
    test.skip(
      !ollama?.available || ollama.models.length === 0,
      "no local model available",
    );
    const model = ollama?.models[0];
    if (!model) throw new Error("no local model available");
    const menus = page.locator("[data-selectmenu]");
    await menus.nth(1).getByRole("button").click();
    await page.getByRole("option", { name: model }).click();
    // The closed menu button must show the new model (proves select persisted).
    await expect(menus.nth(1).getByRole("button")).toContainText(model);

    const input = page.getByPlaceholder(/Pídele algo|Ask your PC/);
    await input.fill("Reply with exactly: hola-test");
    await input.press("Enter");
    // User bubble shows instantly; the reply needs real inference.
    await expect(page.getByText(/hola-test/).first()).toBeVisible({
      timeout: 10000,
    });
    // After the server round-trip the greeting is replaced by the real reply,
    // so there is exactly one assistant reply: assert on its content.
    await expect(page.locator(".reply-text").last()).toContainText(
      "hola-test",
      { timeout: 240000 },
    );
    const reply = await page.locator(".reply-text").last().textContent();
    expect(reply).not.toMatch(/^Error:/);
    // The turn lands in History (Settings): the unified chat keeps no
    // sessions pane on the main screen anymore.
    await page.getByRole("button", { name: /Ajustes|Settings/ }).click();
    await page.getByText("Historial").click();
    await expect(page.getByText(/Reply with exactly/i).first()).toBeVisible({
      timeout: 10000,
    });
    await page.screenshot({ path: "test-results/layout-main.png" });
  },
);
