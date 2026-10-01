import { authApi, type User } from "./auth.api";

const REFRESH_KEY = "smartpc.refresh";

let user = $state<User | null>(null);
// Access token lives only in memory. The refresh token lives in the
// OS-keychain vault under Electron, localStorage on plain web (see below).
let accessToken: string | null = null;

type Vault = NonNullable<NonNullable<Window["smartpc"]>["vault"]>;

function vault(): Vault | null {
  return window.smartpc?.vault ?? null;
}

// Refresh-token storage, in order of preference:
// 1. OS-keychain vault (Electron): encrypted at rest via safeStorage.
// 2. localStorage (plain web): XSS-readable — acceptable for local dev, and
//    the reason the vault exists. A vault failure also falls back here
//    rather than locking the user out.
let vaultUsable: boolean | null = null;

async function vaultReady(): Promise<boolean> {
  if (vaultUsable !== null) return vaultUsable;
  try {
    vaultUsable = (await vault()?.available()) ?? false;
  } catch {
    vaultUsable = false;
  }
  return vaultUsable;
}

function readLocal(): string | null {
  try {
    return window.localStorage.getItem(REFRESH_KEY);
  } catch {
    return null;
  }
}

function writeLocal(token: string | null): void {
  try {
    if (token) window.localStorage.setItem(REFRESH_KEY, token);
    else window.localStorage.removeItem(REFRESH_KEY);
  } catch {
    /* private mode */
  }
}

async function readRefresh(): Promise<string | null> {
  if (await vaultReady()) {
    try {
      const { value } = await vault()!.get(REFRESH_KEY);
      if (value) return value;
      // One-time migration: a pre-vault localStorage token moves into the
      // vault, then the plaintext copy is dropped.
      const legacy = readLocal();
      if (legacy) {
        const { ok } = await vault()!.set(REFRESH_KEY, legacy);
        if (ok) writeLocal(null);
        return legacy;
      }
      return null;
    } catch {
      return readLocal();
    }
  }
  return readLocal();
}

async function writeRefresh(token: string | null): Promise<void> {
  if (await vaultReady()) {
    try {
      if (token) {
        const { ok } = await vault()!.set(REFRESH_KEY, token);
        if (ok) writeLocal(null);
        else writeLocal(token);
      } else {
        await vault()!.delete(REFRESH_KEY);
        writeLocal(null);
      }
      return;
    } catch {
      /* fall through to local */
    }
  }
  writeLocal(token);
}

async function login(email: string, password: string): Promise<void> {
  const { user: u, tokens } = await authApi.login(email, password);
  user = u;
  accessToken = tokens.access_token;
  await writeRefresh(tokens.refresh_token);
  // A fresh sign-in runs the cloud reconcile, which repairs anything a
  // previous logout left behind — the warning has served its purpose.
  // (Single-account devices are the norm; the flag is approximate by design.
  // See logoutWarning below.)
  clearLogoutWarning();
}

async function register(
  email: string,
  password: string,
): Promise<{ needsConfirmation: boolean }> {
  const res = await authApi.register(email, password);
  // Supabase with email confirmation on answers 202 and no session: the
  // account exists but cannot sign in yet. Surface it instead of failing
  // the follow-up login with a confusing "invalid credentials".
  if (res?.needs_confirmation || !res?.user) {
    return { needsConfirmation: true };
  }
  await login(email, password);
  return { needsConfirmation: false };
}

async function restore(): Promise<void> {
  const rt = await readRefresh();
  if (!rt) return;
  try {
    const { user: u, tokens } = await authApi.refresh(rt);
    user = u;
    accessToken = tokens.access_token;
    await writeRefresh(tokens.refresh_token);
  } catch {
    user = null;
    accessToken = null;
    await writeRefresh(null);
  }
}

async function logout(): Promise<void> {
  const rt = await readRefresh();
  let flushed = true;
  try {
    if (rt) {
      const res = await authApi.logout(rt);
      // Older sidecars answer `{ok: true}` without the field: no warning
      // then, since they predate the pre-logout drain entirely.
      flushed = res.cloud_flushed ?? true;
    }
  } catch {
    /* already gone server-side */
  }
  // Warn only on a positive timeout signal. Anything still queued lives on
  // in SQLite and uploads at the next sign-in on this device — the login
  // page says exactly that.
  if (flushed) clearLogoutWarning();
  else setLogoutWarning();
  user = null;
  accessToken = null;
  await writeRefresh(null);
}

async function deleteAccount(): Promise<void> {
  if (!accessToken) throw new Error("No session");
  await authApi.deleteAccount(accessToken);
  user = null;
  accessToken = null;
  await writeRefresh(null);
  clearLogoutWarning();
}

// Whether the last logout left rows unpushed (flush timed out: dead
// network). Persisted, not just in memory: quit-the-app-after-logout must
// not swallow it, since the rows are still sitting in this device's SQLite.
// Boolean, not per-user: reconcile-on-login repairs the signing-in account,
// which is the case this warning is about.
const LOGOUT_WARN_KEY = "smartpc.logout-warning";

function readWarnFlag(): boolean {
  try {
    return window.localStorage.getItem(LOGOUT_WARN_KEY) === "1";
  } catch {
    return false;
  }
}

function writeWarnFlag(on: boolean): void {
  try {
    if (on) window.localStorage.setItem(LOGOUT_WARN_KEY, "1");
    else window.localStorage.removeItem(LOGOUT_WARN_KEY);
  } catch {
    /* private mode */
  }
}

let logoutWarning = $state(readWarnFlag());

function setLogoutWarning(): void {
  logoutWarning = true;
  writeWarnFlag(true);
}

function clearLogoutWarning(): void {
  logoutWarning = false;
  writeWarnFlag(false);
}

export const auth = {
  get user(): User | null {
    return user;
  },
  get token(): string | null {
    return accessToken;
  },
  get logoutWarning(): boolean {
    return logoutWarning;
  },
  login,
  register,
  restore,
  logout,
  deleteAccount,
  clearLogoutWarning,
};
