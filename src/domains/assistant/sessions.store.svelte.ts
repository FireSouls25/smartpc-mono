// Session directory: the list, which session is open, and the viewed
// session's combo. Imports only the API client — conversation content and
// selection live in chat/providers stores, which drive this one.
import { aiApi, type SessionSummary } from "./assistant.api";

let list = $state<SessionSummary[]>([]);
let activeSessionId = $state<string | null>(null);
// Combo the viewed session was created with. Browsing history never mutates
// the global selection; the UI offers an explicit adopt (see chat store).
let sessionCombo = $state<{ provider: string; model: string | null } | null>(
  null,
);

async function refreshSessions(): Promise<void> {
  try {
    list = (await aiApi.sessions()).sessions;
  } catch {
    /* sessions stay stale; the empty-state copy already covers first run */
  }
}

function clearList(): void {
  list = [];
}

function removeFromList(id: string): void {
  list = list.filter((s) => s.id !== id);
}

function setActive(
  id: string | null,
  combo: { provider: string; model: string | null } | null,
): void {
  activeSessionId = id;
  sessionCombo = combo;
}

function clearActive(): void {
  activeSessionId = null;
  sessionCombo = null;
}

function clearCombo(): void {
  sessionCombo = null;
}

export const sessionStore = {
  get list(): SessionSummary[] {
    return list;
  },
  get activeSessionId(): string | null {
    return activeSessionId;
  },
  get sessionCombo(): { provider: string; model: string | null } | null {
    return sessionCombo;
  },
  refreshSessions,
  clearList,
  removeFromList,
  setActive,
  clearActive,
  clearCombo,
};
