import { api } from "../../lib/api";
import { auth } from "../auth/auth.store.svelte";

export interface ProviderInfo {
  id: string;
  name: string;
  available: boolean;
  models: string[];
  default_model: string;
  needs_key: boolean;
  /** False for OAuth/subscription ids (copilot, codex, bedrock): keys for
   * those are refused — authenticate with `pi auth` instead. */
  key_paste: boolean;
  context_window: number | null;
  /** The sidecar knows how to launch this server (today: ollama only). */
  startable: boolean;
  /** Binary resolves on PATH. True when answering; null when N/A. */
  installed: boolean | null;
}

export interface KeyStatus {
  provider: string;
  has_key: boolean;
}

export interface SaveKeyResponse {
  ok: boolean;
  models: string[];
  suggested_model: string | null;
  /** False when pi lists the provider but offers no live check (only
   * opencode verifies today) — the key is stored, first use proves it. */
  verified: boolean;
}

export interface SessionSummary {
  id: string;
  title: string;
  provider: string;
  model: string | null;
  updated_at: string;
  preview: string | null;
  message_count: number;
}

export interface SessionMessage {
  id: string;
  role: string;
  content: string;
  created_at: string;
}

export interface SessionAction {
  id: string;
  session_id: string | null;
  kind: string;
  title: string;
  status: string;
  created_at: string;
  updated_at: string;
}

function withAuth(token?: string): { token?: string } {
  return token ? { token } : {};
}

export interface RunStep {
  tool: string;
  args: unknown;
  action_id: string | null;
  ok: boolean;
  output_preview: string;
}

export interface RunResponse {
  reply: string;
  model: string;
  provider: string;
  session_id: string;
  steps: RunStep[];
  context: { used_tokens: number; window: number | null };
}

export const aiApi = {
  providers: () => api<{ providers: ProviderInfo[] }>("/v1/ai/providers"),

  /** Launch a startable local server and wait until it answers. */
  startProvider: (id: string) =>
    api<{ ok: boolean; already_running: boolean }>(
      `/v1/ai/providers/${encodeURIComponent(id)}/start`,
      { method: "POST", timeoutMs: 45000 },
    ),

  selection: () =>
    api<{ provider: string; model: string | null }>(
      "/v1/ai/selection",
      withAuth(auth.token ?? undefined),
    ),

  select: (provider: string, model?: string) =>
    api<{ provider: string; model: string | null }>("/v1/ai/select", {
      method: "POST",
      body: { provider, model },
      ...withAuth(auth.token ?? undefined),
    }),

  /** Persisted chat turn. Creates the session when sessionId is null. */
  chat: (
    message: string,
    sessionId: string | null,
    opts?: { provider?: string; model?: string },
  ) =>
    api<{ reply: string; model: string; provider: string; session_id: string }>(
      "/v1/ai/chat",
      {
        method: "POST",
        body: {
          session_id: sessionId,
          provider: opts?.provider,
          model: opts?.model,
          message,
        },
        ...withAuth(auth.token ?? undefined),
        // Single-turn inference under cold VRAM can take minutes.
        timeoutMs: 300000,
      },
    ),

  /** Agentic run: model reasons with tools; mutating calls become actions. */
  run: (
    message: string,
    sessionId: string | null,
    opts?: {
      provider?: string;
      model?: string;
      lang?: string;
      signal?: AbortSignal;
    },
  ) =>
    api<RunResponse>("/v1/ai/run", {
      method: "POST",
      body: {
        session_id: sessionId,
        provider: opts?.provider,
        model: opts?.model,
        message,
        lang: opts?.lang,
      },
      ...withAuth(auth.token ?? undefined),
      // Multi-step agent loop: the longest call in the app.
      timeoutMs: 600000,
      signal: opts?.signal,
    }),

  /** Flag the in-flight turn for cancellation (best-effort, idempotent). */
  cancel: () =>
    api<{ ok: boolean }>("/v1/ai/cancel", {
      method: "POST",
      timeoutMs: 15000,
      ...withAuth(auth.token ?? undefined),
    }),

  sessions: () =>
    api<{ sessions: SessionSummary[] }>(
      "/v1/chat/sessions",
      withAuth(auth.token ?? undefined),
    ),

  sessionDetail: (id: string) =>
    api<{
      session: SessionSummary & { created_at: string };
      messages: SessionMessage[];
      actions: SessionAction[];
    }>(`/v1/chat/sessions/${id}`, withAuth(auth.token ?? undefined)),

  deleteSession: (id: string) =>
    api<{ ok: boolean }>(`/v1/chat/sessions/${id}`, {
      method: "DELETE",
      ...withAuth(auth.token ?? undefined),
    }),

  actions: (sessionId: string) =>
    api<{ actions: SessionAction[] }>(
      `/v1/actions?session_id=${encodeURIComponent(sessionId)}`,
      withAuth(auth.token ?? undefined),
    ),

  keyStatus: () =>
    api<{ keys: KeyStatus[] }>(
      "/v1/ai/keys",
      withAuth(auth.token ?? undefined),
    ),

  saveKey: (provider: string, key: string) =>
    api<SaveKeyResponse>("/v1/ai/keys", {
      method: "POST",
      body: { provider, key },
      ...withAuth(auth.token ?? undefined),
      // Live verification spaces its attempts (bot protection).
      timeoutMs: 90000,
    }),

  deleteKey: (provider: string) =>
    api<{ ok: boolean }>(`/v1/ai/keys/${encodeURIComponent(provider)}`, {
      method: "DELETE",
      ...withAuth(auth.token ?? undefined),
    }),
};
