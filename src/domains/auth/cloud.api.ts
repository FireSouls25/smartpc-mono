import { api } from "../../lib/api";

/** Mirrors the sidecar's cloud status (see src/native/src/cloud/mod.rs). */
export interface CloudStatus {
  enabled: boolean;
  /** Host only, e.g. "abc.supabase.co" — never the key. */
  url: string | null;
  last_push_at: string | null;
  last_pull_at: string | null;
  last_error: string | null;
  pushed: number;
  pulled: number;
}

export interface CloudSyncResult {
  ok: boolean;
  pulled: number;
  pushed: number;
}

export const cloudApi = {
  status: (token: string) => api<CloudStatus>("/v1/cloud/status", { token }),
  /** Full two-way reconcile: pull the cloud, then push what is local. */
  sync: (token: string) =>
    api<CloudSyncResult>("/v1/cloud/sync", { method: "POST", token }),
};
