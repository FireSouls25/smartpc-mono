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
      env: { ...process.env, SMARTPC_CLOUD: "0" },
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
    expect(ids).toEqual(["llama.cpp", "ollama", "opencode"]);
    for (const p of body.providers) {
      expect(typeof p["id"]).toBe("string");
      expect(typeof p["name"]).toBe("string");
      expect(typeof p["available"]).toBe("boolean");
      expect(Array.isArray(p["models"])).toBe(true);
      expect(typeof p["default_model"]).toBe("string");
      expect(typeof p["needs_key"]).toBe("boolean");
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
    expect(((await out.json()) as { ok: boolean }).ok).toBe(true);
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
});
