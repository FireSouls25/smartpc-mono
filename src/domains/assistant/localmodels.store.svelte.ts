// App-managed local models (curated GGUFs on our llama-server).
// Standalone like the providers store: the models view reads this,
// chat/sessions follow the persisted selection afterwards.
import { aiApi, type LocalModelInfo } from "./assistant.api";
import { providerStore as providers } from "./providers.store.svelte";

let models = $state<LocalModelInfo[]>([]);
let binaryPresent = $state(false);
let externalServer = $state(false);
let loading = $state(false);
let error = $state("");
let busy = $state<string | null>(null);

function fmtSize(bytes: number): string {
  if (bytes >= 1_000_000_000) return `${(bytes / 1_000_000_000).toFixed(1)} GB`;
  return `${Math.round(bytes / 1_000_000)} MB`;
}

async function refresh(): Promise<void> {
  loading = true;
  error = "";
  try {
    const res = await aiApi.localModels();
    models = res.models;
    binaryPresent = res.binary_present;
    externalServer = res.external_server;
  } catch (err) {
    error = err instanceof Error ? err.message : "Error";
  } finally {
    loading = false;
  }
}

async function download(id: string): Promise<void> {
  busy = id;
  error = "";
  try {
    await aiApi.downloadLocalModel(id);
    await refresh();
    startPoll();
  } catch (err) {
    error = err instanceof Error ? err.message : "Error";
  } finally {
    busy = null;
  }
}

async function cancel(id: string): Promise<void> {
  try {
    await aiApi.cancelLocalModelDownload(id);
    await refresh();
  } catch (err) {
    error = err instanceof Error ? err.message : "Error";
  }
}

async function remove(id: string): Promise<void> {
  busy = id;
  error = "";
  try {
    await aiApi.deleteLocalModel(id);
    // A deleted active pick clears the server selection: reload ours.
    await providers.loadProviders();
    await refresh();
  } catch (err) {
    error = err instanceof Error ? err.message : "Error";
  } finally {
    busy = null;
  }
}

/** Download-complete gate is server-side (`not_downloaded` otherwise). */
async function use(id: string): Promise<void> {
  busy = id;
  error = "";
  try {
    await aiApi.useLocalModel(id);
    await providers.loadProviders();
    await refresh();
    stopPoll();
  } catch (err) {
    error = err instanceof Error ? err.message : "Error";
  } finally {
    busy = null;
  }
}

// Progress watch while anything downloads (2 s): silent refresh, no
// selection clobbering (loadProviders is only called after use/remove).
let pollTimer: number | null = null;

async function pollOnce(): Promise<void> {
  try {
    const res = await aiApi.localModels();
    models = res.models;
    binaryPresent = res.binary_present;
    externalServer = res.external_server;
    if (!models.some((m) => m.state === "downloading")) stopPoll();
  } catch {
    /* next tick retries */
  }
}

function startPoll(): void {
  if (pollTimer !== null || typeof window === "undefined") return;
  pollTimer = window.setInterval(() => void pollOnce(), 2000);
}

function stopPoll(): void {
  if (pollTimer !== null) {
    window.clearInterval(pollTimer);
    pollTimer = null;
  }
}

export const localModels = {
  get models(): LocalModelInfo[] {
    return models;
  },
  get binaryPresent(): boolean {
    return binaryPresent;
  },
  get externalServer(): boolean {
    return externalServer;
  },
  get loading(): boolean {
    return loading;
  },
  get error(): string {
    return error;
  },
  get busy(): string | null {
    return busy;
  },
  fmtSize,
  refresh,
  download,
  cancel,
  remove,
  use,
  stopPoll,
};
