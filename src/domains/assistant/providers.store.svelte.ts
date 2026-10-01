// Providers + selection + API keys + availability watch. Standalone: imports
// nothing from sibling stores (chat/sessions read this, never the reverse).
import { aiApi, type ProviderInfo } from "./assistant.api";
import { ApiError } from "../../lib/api";
import { t, type I18nKey } from "../../lib/i18n.svelte";

/** Provider availability re-check interval (see docs/07-provider-availability.md). */
export const PROVIDER_POLL_MS = 3000;

let providers = $state<ProviderInfo[]>([]);
let providersLoading = $state(false);
let providersError = $state("");
let selectError = $state("");
let keyStatus = $state<Record<string, boolean>>({});
let keyModal = $state<{ provider: string; hasKey: boolean } | null>(null);
let keyBusy = $state(false);
let keyError = $state("");
/** Shown in the modal after an unverified save (see saveKey). */
let keyNotice = $state("");
let startingProvider = $state<string | null>(null);
let startError = $state("");
let activeProvider = $state("ollama");
let activeModel = $state("");
let contextWindow = $state<number | null>(null);

export function defaultModelFor(list: ProviderInfo[], id: string): string {
  const p = list.find((x) => x.id === id);
  return p?.default_model || p?.models[0] || "";
}

function syncContextWindow(): void {
  contextWindow =
    providers.find((p) => p.id === activeProvider)?.context_window ?? null;
}

function setContextWindow(w: number | null): void {
  contextWindow = w;
}

async function loadProviders(opts?: { silent?: boolean }): Promise<void> {
  const silent = opts?.silent ?? false;
  if (!silent) {
    providersLoading = true;
    providersError = "";
  }
  try {
    const res = await aiApi.providers();
    providers = res.providers;
    await refreshKeyStatus();
    if (!silent) {
      try {
        const sel = await aiApi.selection();
        activeProvider = sel.provider;
        activeModel = sel.model || defaultModelFor(providers, activeProvider);
      } catch {
        /* first run: no selection stored yet */
      }
    }
    syncContextWindow();
  } catch (err) {
    if (!silent) providersError = err instanceof Error ? err.message : "Error";
  } finally {
    if (!silent) providersLoading = false;
  }
}

// Availability watch: re-probes providers every PROVIDER_POLL_MS so a server
// started after boot (e.g. `ollama serve` in another terminal) flips the UI
// from "not detected" to usable without a manual refresh. Silent on purpose:
// no loading flicker, no error surfaces, no selection clobbering (the server
// selection is only read on the initial load; user changes go through
// selectProvider which persists + updates local state itself).
let pollTimer: number | null = null;
let pollInFlight = false;

async function pollProviders(): Promise<void> {
  if (pollInFlight) return;
  if (typeof document !== "undefined" && document.hidden) return;
  pollInFlight = true;
  try {
    await loadProviders({ silent: true });
  } catch {
    /* silent: the next tick retries */
  } finally {
    pollInFlight = false;
  }
}

function startProviderWatch(): void {
  if (pollTimer !== null || typeof window === "undefined") return;
  void pollProviders();
  pollTimer = window.setInterval(() => void pollProviders(), PROVIDER_POLL_MS);
}

function stopProviderWatch(): void {
  if (pollTimer !== null) {
    window.clearInterval(pollTimer);
    pollTimer = null;
  }
}

async function selectProvider(
  id: string,
  model?: string | null,
): Promise<void> {
  selectError = "";
  const p = providers.find((x) => x.id === id);
  if (!p || !p.available) {
    selectError = t("providers.offline");
    return;
  }
  if (p.needs_key && !keyStatus[p.id]) {
    // OAuth/subscription ids take no pasted keys: point at pi auth instead
    // of opening a modal that would only refuse.
    if (!p.key_paste) {
      selectError = t("aikey.piAuth");
      return;
    }
    openKeyModal(id);
    return;
  }
  try {
    const res = await aiApi.select(id, model ?? undefined);
    activeProvider = res.provider;
    activeModel = res.model || defaultModelFor(providers, activeProvider);
    syncContextWindow();
  } catch (err) {
    selectError = err instanceof Error ? err.message : "Error";
  }
}

async function selectModel(m: string): Promise<void> {
  await selectProvider(activeProvider, m);
}

async function refreshKeyStatus(): Promise<void> {
  try {
    const res = await aiApi.keyStatus();
    const next: Record<string, boolean> = {};
    for (const k of res.keys) next[k.provider] = k.has_key;
    keyStatus = next;
  } catch {
    /* key status is advisory; providers still list */
  }
}

function openKeyModal(provider: string): void {
  keyError = "";
  keyNotice = "";
  keyModal = { provider, hasKey: !!keyStatus[provider] };
}

function closeKeyModal(): void {
  keyModal = null;
  keyError = "";
  keyNotice = "";
}

async function saveKey(provider: string, key: string): Promise<void> {
  keyBusy = true;
  keyError = "";
  keyNotice = "";
  try {
    const res = await aiApi.saveKey(provider, key);
    // The verify response already carries the fresh catalog for this
    // provider — fold it into local state instead of refetching the whole
    // list (was: keyStatus + full load incl. selection + keyStatus again).
    keyStatus = { ...keyStatus, [provider]: true };
    if (res.models.length > 0) {
      providers = providers.map((p) =>
        p.id === provider
          ? { ...p, available: true, models: [...res.models] }
          : p,
      );
    }
    if (!res.verified) {
      // Stored, but pi offers no live check for this provider: stay open
      // with an honest notice instead of auto-selecting a key that might
      // be a typo. The next manual select runs normally.
      keyNotice = t("aikey.unverified");
      return;
    }
    closeKeyModal();
    // Persist the pre-selection for a model that actually answers — the
    // dropdown keeps offering every catalog model.
    if (!providers.some((p) => p.id === provider)) {
      await loadProviders({ silent: true });
    }
    await selectProvider(provider, res.suggested_model ?? undefined);
  } catch (err) {
    keyError = err instanceof Error ? err.message : "Error";
  } finally {
    keyBusy = false;
  }
}

async function deleteKey(provider: string): Promise<void> {
  keyBusy = true;
  keyError = "";
  try {
    await aiApi.deleteKey(provider);
    keyStatus = { ...keyStatus, [provider]: false };
    closeKeyModal();
  } catch (err) {
    keyError = err instanceof Error ? err.message : "Error";
  } finally {
    keyBusy = false;
  }
}

function startErrorFor(code: string | undefined): I18nKey {
  if (code === "not_installed") return "providers.notInstalled";
  if (code === "start_timeout" || code === "timeout")
    return "providers.startTimeout";
  return "providers.startFailed";
}

/** Ask the sidecar to launch a startable server; the watch picks it up. */
async function startProvider(id: string): Promise<void> {
  if (startingProvider) return;
  startingProvider = id;
  startError = "";
  try {
    await aiApi.startProvider(id);
    await loadProviders({ silent: true });
  } catch (err) {
    const code = err instanceof ApiError ? err.code : undefined;
    startError = t(startErrorFor(code));
  } finally {
    startingProvider = null;
  }
}

function activeModels(): string[] {
  return providers.find((p) => p.id === activeProvider)?.models ?? [];
}

function activeAvailable(): boolean {
  return providers.find((p) => p.id === activeProvider)?.available ?? false;
}

export const providerStore = {
  get providers(): ProviderInfo[] {
    return providers;
  },
  get providersLoading(): boolean {
    return providersLoading;
  },
  get providersError(): string {
    return providersError;
  },
  get selectError(): string {
    return selectError;
  },
  get keyStatus(): Record<string, boolean> {
    return keyStatus;
  },
  get keyModal(): { provider: string; hasKey: boolean } | null {
    return keyModal;
  },
  get keyBusy(): boolean {
    return keyBusy;
  },
  get keyError(): string {
    return keyError;
  },
  get keyNotice(): string {
    return keyNotice;
  },
  get startingProvider(): string | null {
    return startingProvider;
  },
  get startError(): string {
    return startError;
  },
  get activeProvider(): string {
    return activeProvider;
  },
  get activeModel(): string {
    return activeModel;
  },
  get contextWindow(): number | null {
    return contextWindow;
  },
  activeModels,
  activeAvailable,
  setContextWindow,
  loadProviders,
  startProviderWatch,
  stopProviderWatch,
  startProvider,
  selectProvider,
  selectModel,
  refreshKeyStatus,
  openKeyModal,
  closeKeyModal,
  saveKey,
  deleteKey,
};
