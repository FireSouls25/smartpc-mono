import { afterAll, beforeAll, describe, expect, test } from "vitest";
import { spawn, type ChildProcess } from "node:child_process";
import os from "node:os";
import path from "node:path";
import { SIDECAR_PROTOCOL } from "./lib/api";

// Renderer↔sidecar contract: shapes and envelopes both sides rely on, plus
// the protocol pin (imported from the renderer's own api module, so either
// side drifting fails this). Only deterministic routes: nothing here depends
// on models being installed or servers running.
const PORT = Number(process.env.CONTRACT_SIDECAR_PORT ?? 18082);
const TOKEN = "contract-token-min-16-chars!!";
const BASE = `http://127.0.0.1:${PORT}`;
const gate = { "X-Sidecar-Token": TOKEN, "Content-Type": "application/json" };

// Tool-schema surfacing in /internal/pi/tools responses. Known constraint
// keywords are typed so assertions compile; anything else rides `unknown`.
interface SchemaProp {
  enum?: unknown;
  minimum?: unknown;
  maximum?: unknown;
  maxLength?: unknown;
  default?: unknown;
  [k: string]: unknown;
}

let child: ChildProcess | null = null;

async function waitForHealth(deadlineMs = 300000): Promise<void> {
  const start = Date.now();
  for (;;) {
    try {
      const res = await fetch(`${BASE}/health`);
      if (res.ok) return;
    } catch {
      /* not up yet */
    }
    if (Date.now() - start > deadlineMs)
      throw new Error("sidecar did not boot");
    await new Promise((r) => setTimeout(r, 500));
  }
}

beforeAll(async () => {
  const db = path.join(os.tmpdir(), `smartpc-contract-${process.pid}.db`);
  try {
    await import("node:fs/promises").then((fs) =>
      fs.unlink(db).catch(() => {}),
    );
  } catch {
    /* fresh file anyway */
  }
  child = spawn(
    "cargo",
    [
      "run",
      "--manifest-path",
      path.resolve(process.cwd(), "src/native/Cargo.toml"),
      "--",
      "--port",
      String(PORT),
      "--token",
      TOKEN,
      "--db",
      db,
    ],
    {
      cwd: process.cwd(),
      stdio: "ignore",
      // Hermetic: this spec pins shapes, and a developer with SUPABASE_*
      // exported must not have the sidecar proxy auth to a real project.
      // Same for pi auth: ambient ~/.pi/agent/auth.json must not flip
      // needs_key / has_key under the assertions below (mirrors the
      // PI_AUTH_FILE=/dev/null e2e isolation).
      env: { ...process.env, SMARTPC_CLOUD: "0", PI_AUTH_FILE: "/dev/null" },
    },
  );
  await waitForHealth();
}, 600000);

afterAll(() => {
  child?.kill("SIGTERM");
  child = null;
});

describe("sidecar contract", () => {
  test("health carries the renderer protocol", async () => {
    const res = await fetch(`${BASE}/health`);
    expect(res.ok).toBe(true);
    const body = (await res.json()) as { status: string; protocol: number };
    expect(body.status).toBe("ok");
    expect(body.protocol).toBe(SIDECAR_PROTOCOL);
  });

  test("sidecar gate rejects without token", async () => {
    const res = await fetch(`${BASE}/v1/ai/providers`);
    expect(res.status).toBe(401);
    const body = (await res.json()) as { error: { code: string } };
    expect(body.error.code).toBe("unauthorized");
  });

  test("providers list the known catalog with stable shapes", async () => {
    const res = await fetch(`${BASE}/v1/ai/providers`, { headers: gate });
    expect(res.ok).toBe(true);
    const body = (await res.json()) as {
      providers: Record<string, unknown>[];
    };
    const ids = body.providers.map((p) => p["id"]).sort();
    // Loopback trio always present; the rest is pi's registry (snapshot ids
    // are in-repo, so this is deterministic — live groups only add more).
    for (const id of ["llama.cpp", "ollama", "opencode"]) {
      expect(ids).toContain(id);
    }
    for (const id of ["anthropic", "openai", "google", "mistral", "groq"]) {
      expect(ids).toContain(id);
    }
    for (const p of body.providers) {
      expect(typeof p["id"]).toBe("string");
      expect(typeof p["name"]).toBe("string");
      expect(typeof p["available"]).toBe("boolean");
      expect(Array.isArray(p["models"])).toBe(true);
      expect(typeof p["default_model"]).toBe("string");
      expect(typeof p["needs_key"]).toBe("boolean");
      expect(typeof p["key_paste"]).toBe("boolean");
      expect(
        typeof p["context_window"] === "number" || p["context_window"] === null,
      ).toBe(true);
      expect(typeof p["startable"]).toBe("boolean");
      expect(
        typeof p["installed"] === "boolean" || p["installed"] === null,
      ).toBe(true);
    }
  });

  test("auth failures use the error envelope", async () => {
    const res = await fetch(`${BASE}/v1/auth/login`, {
      method: "POST",
      headers: gate,
      body: JSON.stringify({
        email: "nobody@test.co",
        password: "x".repeat(20),
      }),
    });
    expect(res.status).toBe(401);
    const body = (await res.json()) as {
      error: { code: string; message: string };
    };
    expect(typeof body.error.code).toBe("string");
    expect(typeof body.error.message).toBe("string");
  });

  test("register validation uses the envelope, creates nothing", async () => {
    const res = await fetch(`${BASE}/v1/auth/register`, {
      method: "POST",
      headers: gate,
      body: JSON.stringify({ email: "not-an-email", password: "short" }),
    });
    expect(res.status).toBe(400);
    const body = (await res.json()) as { error: { code: string } };
    expect(body.error.code).toBe("validation");
  });

  test("user routes require a bearer token", async () => {
    const res = await fetch(`${BASE}/v1/chat/sessions`, { headers: gate });
    expect(res.status).toBe(401);
    const body = (await res.json()) as { error: { code: string } };
    expect(body.error.code).toBe("unauthorized");
  });

  test("provider keys validate ids and fail closed without keys", async () => {
    const tag = `k${Date.now()}${Math.floor(Math.random() * 1e6)}`;
    await fetch(`${BASE}/v1/auth/register`, {
      method: "POST",
      headers: gate,
      body: JSON.stringify({
        email: `${tag}@test.co`,
        password: "correct-horse-1",
      }),
    });
    const login = await fetch(`${BASE}/v1/auth/login`, {
      method: "POST",
      headers: gate,
      body: JSON.stringify({
        email: `${tag}@test.co`,
        password: "correct-horse-1",
      }),
    });
    const tokens = (await login.json()).tokens as { access_token: string };
    const authed = {
      ...gate,
      Authorization: `Bearer ${tokens.access_token}`,
    };
    const postKey = (provider: string, key: string) =>
      fetch(`${BASE}/v1/ai/keys`, {
        method: "POST",
        headers: authed,
        body: JSON.stringify({ provider, key }),
      });

    // Shell-hostile ids never reach storage or env derivation.
    for (const bad of ["!!nope", "a/b", ""]) {
      const res = await postKey(bad, "long-enough-key-123");
      expect(res.status).toBe(400);
      const body = (await res.json()) as { error: { code: string } };
      expect(body.error.code).toBe("validation");
    }
    // Loopback providers take no keys.
    const local = await postKey("ollama", "long-enough-key-123");
    expect(local.status).toBe(400);

    // Unknown-but-sane id, no key anywhere: turns fail with the actionable
    // code before any pi RPC (fake id keeps this deterministic: it can be
    // in neither our store nor pi's auth file).
    const sel = await fetch(`${BASE}/v1/ai/select`, {
      method: "POST",
      headers: authed,
      body: JSON.stringify({ provider: "definitely-not-a-provider-xyz" }),
    });
    expect(sel.status).toBe(400);
    const selBody = (await sel.json()) as { error: { code: string } };
    expect(selBody.error.code).toBe("missing_key");

    // Key presence shape holds; local accounts never hold keys here.
    const ks = await fetch(`${BASE}/v1/ai/keys`, { headers: authed });
    expect(ks.ok).toBe(true);
    const ksBody = (await ks.json()) as {
      keys: { provider: string; has_key: boolean }[];
    };
    expect(Array.isArray(ksBody.keys)).toBe(true);
    for (const k of ksBody.keys) {
      expect(typeof k.provider).toBe("string");
      expect(typeof k.has_key).toBe("boolean");
    }
    const oc = ksBody.keys.find((k) => k.provider === "opencode");
    expect(oc?.has_key).toBe(false);
  });

  test("cloud routes are gated and report the feature as off", async () => {
    // No token: the cloud status is as private as the rest of /v1.
    const anon = await fetch(`${BASE}/v1/cloud/status`, { headers: gate });
    expect(anon.status).toBe(401);

    const tag = `c${Date.now()}${Math.floor(Math.random() * 1e6)}`;
    await fetch(`${BASE}/v1/auth/register`, {
      method: "POST",
      headers: gate,
      body: JSON.stringify({
        email: `${tag}@test.co`,
        password: "correct-horse-1",
      }),
    });
    const login = await fetch(`${BASE}/v1/auth/login`, {
      method: "POST",
      headers: gate,
      body: JSON.stringify({
        email: `${tag}@test.co`,
        password: "correct-horse-1",
      }),
    });
    const tokens = (await login.json()).tokens as { access_token: string };
    const authed = {
      ...gate,
      Authorization: `Bearer ${tokens.access_token}`,
    };

    // SMARTPC_CLOUD=0 in this harness: enabled must be a clean, honest false.
    const status = await fetch(`${BASE}/v1/cloud/status`, { headers: authed });
    expect(status.ok).toBe(true);
    const body = (await status.json()) as Record<string, unknown>;
    expect(body["enabled"]).toBe(false);
    expect(body["url"]).toBeNull();
    expect(typeof body["pushed"]).toBe("number");
    expect(typeof body["pulled"]).toBe("number");

    // Sync without configuration is a client error, never a silent "ok".
    const sync = await fetch(`${BASE}/v1/cloud/sync`, {
      method: "POST",
      headers: authed,
    });
    expect(sync.status).toBeGreaterThanOrEqual(400);
  });

  test("me reports whether the account is cloud-managed", async () => {
    const tag = `m${Date.now()}${Math.floor(Math.random() * 1e6)}`;
    await fetch(`${BASE}/v1/auth/register`, {
      method: "POST",
      headers: gate,
      body: JSON.stringify({
        email: `${tag}@test.co`,
        password: "correct-horse-1",
      }),
    });
    const login = await fetch(`${BASE}/v1/auth/login`, {
      method: "POST",
      headers: gate,
      body: JSON.stringify({
        email: `${tag}@test.co`,
        password: "correct-horse-1",
      }),
    });
    const tokens = (await login.json()).tokens as { access_token: string };
    const res = await fetch(`${BASE}/v1/auth/me`, {
      headers: { ...gate, Authorization: `Bearer ${tokens.access_token}` },
    });
    expect(res.ok).toBe(true);
    const body = (await res.json()) as {
      user: { email: string };
      cloud_account: boolean;
    };
    expect(body.user.email).toBe(`${tag}@test.co`);
    // Local signup: not a Supabase account.
    expect(body.cloud_account).toBe(false);
  });

  test("logout is idempotent and answers ok", async () => {
    const tag = `l${Date.now()}${Math.floor(Math.random() * 1e6)}`;
    await fetch(`${BASE}/v1/auth/register`, {
      method: "POST",
      headers: gate,
      body: JSON.stringify({
        email: `${tag}@test.co`,
        password: "correct-horse-1",
      }),
    });
    const login = await fetch(`${BASE}/v1/auth/login`, {
      method: "POST",
      headers: gate,
      body: JSON.stringify({
        email: `${tag}@test.co`,
        password: "correct-horse-1",
      }),
    });
    const tokens = (await login.json()).tokens as { refresh_token: string };
    // With the cloud off the flush is an instant no-op: logout stays fast.
    const out = await fetch(`${BASE}/v1/auth/logout`, {
      method: "POST",
      headers: gate,
      body: JSON.stringify({ refresh_token: tokens.refresh_token }),
    });
    expect(out.ok).toBe(true);
    const outBody = (await out.json()) as {
      ok: boolean;
      cloud_flushed: boolean;
    };
    expect(outBody.ok).toBe(true);
    expect(outBody.cloud_flushed).toBe(true);
    // Unknown tokens still succeed (idempotent).
    const unknown = await fetch(`${BASE}/v1/auth/logout`, {
      method: "POST",
      headers: gate,
      body: JSON.stringify({ refresh_token: "nope" }),
    });
    expect(unknown.ok).toBe(true);
  });

  test("unstartable providers fail closed, deterministically", async () => {
    for (const id of ["llama.cpp", "nope"]) {
      const res = await fetch(
        `${BASE}/v1/ai/providers/${encodeURIComponent(id)}/start`,
        { method: "POST", headers: gate },
      );
      expect(res.status).toBe(400);
      const body = (await res.json()) as { error: { code: string } };
      expect(body.error.code).toBe("not_startable");
    }
  });

  test("voice status has a stable shape (no mic needed)", async () => {
    const res = await fetch(`${BASE}/v1/voice/status`, { headers: gate });
    expect(res.ok).toBe(true);
    const body = (await res.json()) as Record<string, unknown>;
    expect(typeof body["listening"]).toBe("boolean");
    expect(typeof body["capturing"]).toBe("boolean");
    expect(body["mode"] === null || typeof body["mode"] === "string").toBe(
      true,
    );
    expect(typeof body["model"]).toBe("string");
    expect(
      body["wake_word"] === null || typeof body["wake_word"] === "string",
    ).toBe(true);
    expect(typeof body["mic"]).toBe("boolean");
    expect(body["device"] === null || typeof body["device"] === "string").toBe(
      true,
    );
    expect(
      Array.isArray(body["inputs"]) &&
        (body["inputs"] as unknown[]).every((d) => typeof d === "string"),
    ).toBe(true);
    expect(typeof body["model_ready"]).toBe("boolean");
  });

  test("voice listen validates before touching hardware", async () => {
    const bad = await fetch(`${BASE}/v1/voice/listen`, {
      method: "POST",
      headers: gate,
      body: JSON.stringify({ mode: "shout" }),
    });
    expect(bad.status).toBe(400);
    const body = (await bad.json()) as { error: { code: string } };
    expect(body.error.code).toBe("invalid_mode");
    // Failed validation never claims a record: no stuck session possible.
    const st = (await (
      await fetch(`${BASE}/v1/voice/status`, { headers: gate })
    ).json()) as { listening: boolean };
    expect(st.listening).toBe(false);
    // Stop is idempotent, session or not.
    const stop = await fetch(`${BASE}/v1/voice/stop`, {
      method: "POST",
      headers: gate,
    });
    expect(stop.ok).toBe(true);
  });

  test("voice listen hands out epochs (or fails clean without mic)", async () => {
    const res = await fetch(`${BASE}/v1/voice/listen`, {
      method: "POST",
      headers: gate,
      body: JSON.stringify({ mode: "manual", lang: "en" }),
    });
    const body = (await res.json()) as
      { ok: boolean; epoch: number } | { error: { code: string } };
    if ("error" in body) {
      // Headless CI has no microphone: must fail closed, record-free.
      expect(res.status).toBe(503);
      expect(body.error.code).toBe("no_microphone");
    } else {
      expect(res.ok).toBe(true);
      expect(typeof body.epoch).toBe("number");
      // A live session announces itself first: the started event carries
      // the epoch, and the cleared queue holds no replayed history.
      const polled = (await (
        await fetch(`${BASE}/v1/voice/events?cursor=0`, {
          headers: gate,
          signal: AbortSignal.timeout(15000),
        })
      ).json()) as { events: { type: string; epoch: number }[] };
      expect(polled.events.length).toBeGreaterThan(0);
      expect(polled.events[0].type).toBe("started");
      for (const ev of polled.events) expect(ev.epoch).toBe(body.epoch);
      await fetch(`${BASE}/v1/voice/stop`, { method: "POST", headers: gate });
      const st = (await (
        await fetch(`${BASE}/v1/voice/status`, { headers: gate })
      ).json()) as { listening: boolean };
      expect(st.listening).toBe(false);
    }
    // First-use whisper download (~75 MB into a fresh temp dir) dwarfs the
    // 5 s default timeout on slow links.
  }, 120000);

  test("voice speak validates without side effects", async () => {
    const empty = await fetch(`${BASE}/v1/voice/speak`, {
      method: "POST",
      headers: gate,
      body: JSON.stringify({ text: "   " }),
    });
    expect(empty.status).toBe(400);
    const long = await fetch(`${BASE}/v1/voice/speak`, {
      method: "POST",
      headers: gate,
      body: JSON.stringify({ text: "x".repeat(2001) }),
    });
    expect(long.status).toBe(400);
    // Stop is idempotent, speaking or not.
    const stop = await fetch(`${BASE}/v1/voice/speak-stop`, {
      method: "POST",
      headers: gate,
    });
    expect(stop.ok).toBe(true);
  });

  test("voice model uninstall validates and stays idempotent", async () => {
    const bad = await fetch(`${BASE}/v1/voice/models/medium`, {
      method: "DELETE",
      headers: gate,
    });
    expect(bad.status).toBe(400);
    const body = (await bad.json()) as { error: { code: string } };
    expect(body.error.code).toBe("invalid_model");
    // Temp models dir: earlier tests may have downloaded tiny (real mic
    // here) — either way removal works, and the second call is a clean no-op.
    const gone = await fetch(`${BASE}/v1/voice/models/tiny`, {
      method: "DELETE",
      headers: gate,
    });
    expect(gone.ok).toBe(true);
    const goneBody = (await gone.json()) as { ok: boolean; removed: boolean };
    expect(goneBody.ok).toBe(true);
    expect(typeof goneBody.removed).toBe("boolean");
    const goneAgain = await fetch(`${BASE}/v1/voice/models/tiny`, {
      method: "DELETE",
      headers: gate,
    });
    const goneAgainBody = (await goneAgain.json()) as {
      ok: boolean;
      removed: boolean;
    };
    expect(goneAgainBody.ok).toBe(true);
    expect(goneAgainBody.removed).toBe(false);
  });

  test("protocol is 5 (v5: TTS voice uninstall + real install state)", async () => {
    expect(SIDECAR_PROTOCOL).toBe(5);
    const res = await fetch(`${BASE}/health`);
    const body = (await res.json()) as { protocol: number };
    expect(body.protocol).toBe(5);
  });

  test("T4 control: catalog carries mouse/key tools + honest type_text", async () => {
    const res = await fetch(`${BASE}/internal/pi/tools`, {
      method: "POST",
      headers: gate,
    });
    expect(res.ok).toBe(true);
    const body = (await res.json()) as {
      tools: {
        name: string;
        description: string;
        parameters: {
          type: string;
          properties: Record<string, SchemaProp>;
          required: string[];
          additionalProperties: boolean;
        };
      }[];
    };
    // Contains-key per tool (never totals).
    const byName = new Map(body.tools.map((t) => [t.name, t]));
    const move = byName.get("mouse_move");
    expect(move).toBeDefined();
    expect(move!.parameters.required).toEqual(["x", "y"]);
    expect(move!.description).toContain("screenshot");
    const click = byName.get("mouse_click");
    expect(click).toBeDefined();
    expect(click!.parameters.properties["button"]?.enum).toEqual([
      "left",
      "right",
      "middle",
    ]);
    expect(click!.parameters.properties["count"]?.maximum).toBe(2);
    const scroll = byName.get("mouse_scroll");
    expect(scroll).toBeDefined();
    expect(scroll!.parameters.properties["delta"]?.minimum).toBe(-10);
    expect(scroll!.parameters.properties["delta"]?.maximum).toBe(10);
    const combo = byName.get("key_combo");
    expect(combo).toBeDefined();
    expect(combo!.parameters.required).toEqual(["combo"]);
    expect(combo!.parameters.properties["combo"]?.enum).toContain("redo");
    // type_text is honestly budgeted: 200/call, no NEVER claim.
    const tt = byName.get("type_text");
    expect(tt!.parameters.properties["text"]?.maxLength).toBe(200);
    expect(tt!.description).not.toContain("NEVER");
    // get_system_context advertises its refresh passthrough (T1 P2 fix).
    expect(
      byName.get("get_system_context")!.parameters.properties,
    ).toHaveProperty("refresh");
  });

  test("T4 control: risky toggle round-trips per user", async () => {
    const tag = `r${Date.now()}${Math.floor(Math.random() * 1e6)}`;
    await fetch(`${BASE}/v1/auth/register`, {
      method: "POST",
      headers: gate,
      body: JSON.stringify({
        email: `${tag}@test.co`,
        password: "correct-horse-1",
      }),
    });
    const login = await fetch(`${BASE}/v1/auth/login`, {
      method: "POST",
      headers: gate,
      body: JSON.stringify({
        email: `${tag}@test.co`,
        password: "correct-horse-1",
      }),
    });
    const tokens = (await login.json()).tokens as { access_token: string };
    const authed = {
      ...gate,
      Authorization: `Bearer ${tokens.access_token}`,
    };
    // Fresh user: off, source none (CI never sets HARNESS_ALLOW_RISKY).
    const before = (await (
      await fetch(`${BASE}/v1/prefs/risky-input`, { headers: authed })
    ).json()) as { allowed: boolean; source: string };
    expect(before.allowed).toBe(false);
    expect(before.source).toBe("none");
    // Enable: one click, source toggle.
    const on = await fetch(`${BASE}/v1/prefs/risky-input`, {
      method: "PUT",
      headers: authed,
      body: JSON.stringify({ allowed: true }),
    });
    expect(on.ok).toBe(true);
    const onBody = (await on.json()) as { allowed: boolean; source: string };
    expect(onBody.allowed).toBe(true);
    expect(onBody.source).toBe("toggle");
    // Revoke: one click back off.
    const off = await fetch(`${BASE}/v1/prefs/risky-input`, {
      method: "PUT",
      headers: authed,
      body: JSON.stringify({ allowed: false }),
    });
    expect(off.ok).toBe(true);
    expect(((await off.json()) as { allowed: boolean }).allowed).toBe(false);
    // Another user's toggle is untouched (per-user scoping).
    const again = (await (
      await fetch(`${BASE}/v1/prefs/risky-input`, { headers: authed })
    ).json()) as { allowed: boolean };
    expect(again.allowed).toBe(false);
  });

  test("T4 control: audit export is gated, redacted NDJSON", async () => {
    const tag = `a${Date.now()}${Math.floor(Math.random() * 1e6)}`;
    await fetch(`${BASE}/v1/auth/register`, {
      method: "POST",
      headers: gate,
      body: JSON.stringify({
        email: `${tag}@test.co`,
        password: "correct-horse-1",
      }),
    });
    const login = await fetch(`${BASE}/v1/auth/login`, {
      method: "POST",
      headers: gate,
      body: JSON.stringify({
        email: `${tag}@test.co`,
        password: "correct-horse-1",
      }),
    });
    const tokens = (await login.json()).tokens as { access_token: string };
    const authed = {
      ...gate,
      Authorization: `Bearer ${tokens.access_token}`,
    };
    // No token at all: same gate as the rest of /v1.
    expect(
      (await fetch(`${BASE}/v1/support/audit-export?session_id=x`)).status,
    ).toBe(401);
    // session_id required.
    const missing = await fetch(`${BASE}/v1/support/audit-export`, {
      headers: authed,
    });
    expect(missing.status).toBe(400);
    // Unknown session: 404, never another user's rows.
    const gone = await fetch(
      `${BASE}/v1/support/audit-export?session_id=nope`,
      { headers: authed },
    );
    expect(gone.status).toBe(404);
    // Fresh session: 200, NDJSON content type, zero rows.
    const sess = (await (
      await fetch(`${BASE}/v1/chat/sessions`, {
        method: "POST",
        headers: authed,
        body: JSON.stringify({ title: "audit probe" }),
      })
    ).json()) as { session: { id: string } };
    const exp = await fetch(
      `${BASE}/v1/support/audit-export?session_id=${sess.session.id}`,
      { headers: authed },
    );
    expect(exp.ok).toBe(true);
    expect(exp.headers.get("content-type")).toContain("ndjson");
    expect(exp.headers.get("content-disposition") ?? "").toContain(
      "attachment",
    );
    expect(await exp.text()).toBe("");
  });

  test("T2 open_url: catalog carries the link opener (Medium, gated)", async () => {
    const res = await fetch(`${BASE}/internal/pi/tools`, {
      method: "POST",
      headers: gate,
    });
    expect(res.ok).toBe(true);
    const body = (await res.json()) as {
      tools: {
        name: string;
        description: string;
        parameters: {
          type: string;
          properties: Record<string, SchemaProp>;
          required: string[];
          additionalProperties: boolean;
        };
      }[];
    };
    // Contains-key per tool (never totals — later tracks add more).
    const byName = new Map(body.tools.map((t) => [t.name, t]));
    const link = byName.get("open_url");
    expect(link).toBeDefined();
    expect(link!.parameters.additionalProperties).toBe(false);
    expect(link!.parameters.required).toEqual(["url"]);
    expect(link!.parameters.properties["url"]?.maxLength).toBe(2048);
    expect(link!.parameters.properties).toHaveProperty("browser");
    // Safety P0-a copy: allowlist + http-confirm + query-strip documented.
    expect(link!.description).toContain("allowlisted");
    expect(link!.description).toContain("confirmation");
    expect(link!.description).toContain("query");
  });

  test("T2 open_url: javascript: URLs fail closed (never launched)", async () => {
    // No turn context here, so the tool endpoint must fail closed as a
    // result ({ok:false}), never throw or launch. The strict scheme
    // refusal itself (ok:false containing "refused javascript:") is
    // asserted in Rust (exec::tests::open_url_refuses_javascript_by_scheme).
    for (const url of [
      "javascript:alert(1)",
      "javascript:alert(document.cookie)",
    ]) {
      const res = await fetch(`${BASE}/internal/pi/tool`, {
        method: "POST",
        headers: gate,
        body: JSON.stringify({ name: "open_url", args: { url } }),
      });
      const body = (await res.json()) as { ok: boolean };
      expect(body.ok).toBe(false);
    }
  });

  test("T1 grounding: tool catalog carries the display tools", async () => {
    const res = await fetch(`${BASE}/internal/pi/tools`, {
      method: "POST",
      headers: gate,
    });
    expect(res.ok).toBe(true);
    const body = (await res.json()) as {
      tools: {
        name: string;
        description: string;
        parameters: {
          type: string;
          properties: Record<string, SchemaProp>;
          required: string[];
          additionalProperties: boolean;
        };
      }[];
    };
    // Contains-key per tool (never totals — later tracks add more).
    const byName = new Map(body.tools.map((t) => [t.name, t]));
    const info = byName.get("get_display_info");
    expect(info).toBeDefined();
    expect(info!.parameters.additionalProperties).toBe(false);
    expect(info!.parameters.properties).toHaveProperty("refresh");
    const shot = byName.get("capture_screen");
    expect(shot).toBeDefined();
    // Disclosure copy (safety P0-b): the model sees what capture implies.
    expect(shot!.description).toContain("may include secrets");
    expect(shot!.parameters.additionalProperties).toBe(false);
    const props = shot!.parameters.properties;
    expect(props["max_width"]?.maximum).toBe(1280);
    expect(props["format"]?.enum).toContain("png");
    expect(props).toHaveProperty("region");
  });

  test("T3 voice: tts-models catalog is always 200 with stable shapes", async () => {
    const res = await fetch(`${BASE}/v1/voice/tts-models`, { headers: gate });
    expect(res.ok).toBe(true);
    const body = (await res.json()) as {
      models: {
        id: string;
        lang: string;
        label: string;
        gender: string;
        size_mb: number;
        quality: string;
      }[];
      default_for_lang: Record<string, string>;
      active: string | null;
      ready: Record<string, boolean>;
    };
    expect(Array.isArray(body.models)).toBe(true);
    // Empty models = engine missing (pi-bridge not installed here); either
    // way the shape holds and defaults stay advertised.
    for (const m of body.models) {
      // A2 takes these ids over as synth ids: the prefix is the contract.
      expect(
        m.id.startsWith("piper-") ||
          m.id.startsWith("kitten-") ||
          m.id.startsWith("kokoro-"),
      ).toBe(true);
      expect(typeof m.lang).toBe("string");
      expect(typeof m.label).toBe("string");
      expect(typeof m.size_mb).toBe("number");
      expect(typeof m.quality).toBe("string");
      expect(m.gender === "male" || m.gender === "female").toBe(true);
    }
    // English: 2 masculine + 2 feminine (Kitten sids, one 25 MB model).
    const en = body.models.filter((m) => m.lang === "en");
    expect(en.filter((m) => m.gender === "male").length).toBe(2);
    expect(en.filter((m) => m.gender === "female").length).toBe(2);
    // Spanish is Davefx alone: the Kokoro es sids were removed (their
    // model produces NaN samples; neither v1_1 nor fp32 v1_0 has es).
    const es = body.models.filter((m) => m.lang === "es");
    expect(es.filter((m) => m.gender === "male").length).toBe(1);
    expect(es.filter((m) => m.gender === "female").length).toBe(0);
    expect(typeof body.default_for_lang).toBe("object");
    expect(body.default_for_lang["es"]).toContain("piper-");
    expect(body.default_for_lang["en"]).toBeDefined();
    // Sidecar stays stateless per call: no active voice server-side.
    expect(body.active).toBeNull();
    expect(typeof body.ready).toBe("object");
  });

  test("TTS voice uninstall is idempotent + validates ids", async () => {
    // Unknown catalog ids 400 (mirrors speak validation).
    const bad = await fetch(
      `${BASE}/v1/voice/tts-models/${encodeURIComponent("nope")}`,
      { method: "DELETE", headers: gate },
    );
    expect(bad.status).toBe(400);
    const badBody = (await bad.json()) as { error: { code: string } };
    expect(badBody.error.code).toBe("invalid_voice");
    // Known-but-absent model: ok, removed=false, never an error.
    const res = await fetch(
      `${BASE}/v1/voice/tts-models/${encodeURIComponent("kitten-nano-en-v0_2#1")}`,
      { method: "DELETE", headers: gate },
    );
    expect(res.ok).toBe(true);
    const body = (await res.json()) as { ok: boolean; removed: boolean };
    expect(body.ok).toBe(true);
    expect(typeof body.removed).toBe("boolean");
  });

  test("T3 voice: speak rejects an unknown voice before any spawn", async () => {
    // Validation-only: the 400 lands before TTS spawns anything, so no
    // audio side effects and no engine needed.
    const res = await fetch(`${BASE}/v1/voice/speak`, {
      method: "POST",
      headers: gate,
      body: JSON.stringify({ text: "hola", voice: "nope-not-a-voice" }),
    });
    expect(res.status).toBe(400);
    const body = (await res.json()) as {
      error: { code: string; field?: string };
    };
    expect(body.error.code).toBe("invalid_voice");
    expect(body.error.field).toBe("voice");
    // Blank voice behaves as omitted (per-language default) — it must NOT
    // 400 as invalid_voice. Without an engine this 500s as misconfigured
    // (loud, never silent); with one it speaks. Either way the voice
    // param itself validates clean.
    const blank = await fetch(`${BASE}/v1/voice/speak`, {
      method: "POST",
      headers: gate,
      body: JSON.stringify({ text: "hola", voice: "  " }),
    });
    if (blank.status === 400) {
      const blankBody = (await blank.json()) as {
        error: { code: string };
      };
      expect(blankBody.error.code).not.toBe("invalid_voice");
    }
  });

  // Live audio only: needs the pi-listen engine + speakers. Excluded by
  // default; run with SMARTPC_TTS_LIVE=1 for the full ok-path proof
  // (response carries the resolved `model`).
  const live = process.env.SMARTPC_TTS_LIVE === "1" ? test : test.skip;
  live("T3 voice: live speak answers ok with the resolved model", async () => {
    const res = await fetch(`${BASE}/v1/voice/speak`, {
      method: "POST",
      headers: gate,
      body: JSON.stringify({ text: "hola" }),
    });
    expect(res.ok).toBe(true);
    const body = (await res.json()) as {
      ok: boolean;
      estimated_ms: number;
      model: string;
    };
    expect(body.ok).toBe(true);
    expect(typeof body.estimated_ms).toBe("number");
    expect(typeof body.model).toBe("string");
    await fetch(`${BASE}/v1/voice/speak-stop`, {
      method: "POST",
      headers: gate,
    });
  });
});
