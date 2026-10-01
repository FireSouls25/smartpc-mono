import { cloudApi, type CloudStatus } from "./cloud.api";
import { auth } from "./auth.store.svelte";

/**
 * Supabase cloud state for the Settings → Account panel.
 *
 * The sidecar does the work (it owns the tokens); this is a read-mostly
 * mirror so the UI can show whether sync is on and when it last ran.
 * `null` status means "never loaded" — the panel then says nothing.
 */
let status = $state<CloudStatus | null>(null);
let syncing = $state(false);
let lastError = $state<string | null>(null);

async function load(): Promise<void> {
  if (!auth.token) {
    status = null;
    return;
  }
  try {
    status = await cloudApi.status(auth.token);
  } catch {
    // Cloud is optional: a sidecar without it answers 404, and that is not
    // an error worth showing.
    status = null;
  }
}

async function syncNow(): Promise<boolean> {
  if (!auth.token || syncing) return false;
  syncing = true;
  lastError = null;
  try {
    await cloudApi.sync(auth.token);
    await load();
    // A full reconcile just pushed everything local: any logout-time
    // warning about pending rows is resolved.
    auth.clearLogoutWarning();
    return true;
  } catch (err) {
    lastError = err instanceof Error ? err.message : "Sync failed";
    return false;
  } finally {
    syncing = false;
  }
}

export const cloud = {
  get status(): CloudStatus | null {
    return status;
  },
  get syncing(): boolean {
    return syncing;
  },
  get lastError(): string | null {
    return lastError;
  },
  load,
  syncNow,
};
