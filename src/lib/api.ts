// Minimal typed fetch wrapper. Talks to the Rust sidecar:
// - inside Electron: url + per-launch token come from the preload bridge
//   (main spawns the sidecar and injects them).
// - plain web dev: VITE_API_URL / VITE_SIDECAR_TOKEN (run the sidecar manually).
interface SidecarInfo {
  url: string;
  token: string;
}

function sidecar(): SidecarInfo | null {
  return window.smartpc?.sidecar ?? null;
}

function base(): string {
  return (
    sidecar()?.url ?? import.meta.env.VITE_API_URL ?? "http://127.0.0.1:18080"
  );
}

function sidecarToken(): string {
  return sidecar()?.token ?? import.meta.env.VITE_SIDECAR_TOKEN ?? "";
}

/** Must match the sidecar PROTOCOL const; the shell warns on mismatch. */
export const SIDECAR_PROTOCOL = 5;

/** Default budget for sidecar calls. Chat/run/keys override per endpoint;
 * inference under cold VRAM is the only thing allowed past this. */
const DEFAULT_TIMEOUT_MS = 30000;

export async function fetchHealth(): Promise<{
  status: string;
  protocol: number;
}> {
  const res = await fetch(base() + "/health", {
    signal: AbortSignal.timeout(10000),
  });
  return (await res.json()) as { status: string; protocol: number };
}

export class ApiError extends Error {
  status: number;
  code?: string;
  constructor(status: number, message: string, code?: string) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

interface Options {
  method?: "GET" | "POST" | "PUT" | "PATCH" | "DELETE";
  body?: unknown;
  token?: string;
  /**
   * Abort budget. `undefined` → default 30 s; `null` → no timeout
   * (nothing uses this today — even inference is bounded, just longer);
   * a number overrides with that many ms.
   */
  timeoutMs?: number | null;
  /** User-initiated abort (Detener button): combined with the timeout. */
  signal?: AbortSignal;
}

export async function api<T>(path: string, opts: Options = {}): Promise<T> {
  const gate = sidecarToken();
  const timeout =
    opts.timeoutMs === null
      ? undefined
      : AbortSignal.timeout(opts.timeoutMs ?? DEFAULT_TIMEOUT_MS);
  const signal =
    timeout && opts.signal
      ? AbortSignal.any([timeout, opts.signal])
      : (opts.signal ?? timeout);
  let res: Response;
  try {
    res = await fetch(base() + path, {
      method: opts.method ?? "GET",
      headers: {
        "Content-Type": "application/json",
        ...(opts.token ? { Authorization: `Bearer ${opts.token}` } : {}),
        ...(gate ? { "X-Sidecar-Token": gate } : {}),
      },
      body: opts.body !== undefined ? JSON.stringify(opts.body) : undefined,
      signal,
    });
  } catch (err) {
    if (err instanceof DOMException && err.name === "TimeoutError") {
      throw new ApiError(504, "Request timed out", "timeout");
    }
    throw err;
  }
  if (res.status === 204) return undefined as T;
  const data = await res.json().catch(() => null);
  if (!res.ok) {
    throw new ApiError(
      res.status,
      data?.error?.message ?? "Request failed",
      data?.error?.code,
    );
  }
  return data as T;
}
