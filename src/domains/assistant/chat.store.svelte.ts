// Open conversation: messages, events, draft, voice, context meter, and the
// session lifecycle (new/open/delete/adopt) orchestrating the session
// directory + provider selection. Depends on sessions + providers stores;
// they never import back (acyclic by construction).
import { aiApi } from "./assistant.api";
import { ApiError } from "../../lib/api";
import { getLang, t, type I18nKey } from "../../lib/i18n.svelte";
import { sessionStore } from "./sessions.store.svelte";
import { providerStore } from "./providers.store.svelte";
import { SvelteSet } from "svelte/reactivity";

export type OrbState = "idle" | "listening" | "thinking";

export interface ChatMsg {
  /** Server message id when persisted; undefined for the local greeting. */
  id?: string;
  role: "user" | "assistant";
  text?: string;
  textKey?: I18nKey;
  steps?: { tool: string; ok: boolean }[];
}

export interface AppEvent {
  id: string;
  title: string;
  status: "running" | "done" | "failed";
  continuous: boolean;
}

const greeting = (): ChatMsg[] => [
  { role: "assistant", textKey: "chat.hello" },
];

let orb = $state<OrbState>("idle");
let messages = $state<ChatMsg[]>(greeting());
let events = $state<AppEvent[]>([]);
let contextUsed = $state(0);
let draft = $state("");

type ReplyListener = (id: string | undefined, text: string | undefined) => void;
const replyListeners = new SvelteSet<ReplyListener>();

/**
 * Subscribe to fresh assistant replies (SEND path only — history loads
 * never fire). Lets the voice store auto-speak without importing it here
 * (which would cycle the module graph).
 */
export function onAssistantReply(cb: ReplyListener): () => void {
  replyListeners.add(cb);
  return () => {
    replyListeners.delete(cb);
  };
}

function notifyReply(): void {
  if (replyListeners.size === 0) return;
  const last = messages[messages.length - 1];
  if (!last || last.role !== "assistant" || last.textKey) return;
  for (const cb of replyListeners) {
    try {
      cb(last.id, last.text);
    } catch {
      /* listener-local failure */
    }
  }
}

/** Voice sessions drive the orb while capturing (see voice store). */
function setOrb(next: OrbState): void {
  orb = next;
}

export function toEvent(a: {
  id: string;
  title: string;
  status: string;
}): AppEvent {
  const status =
    a.status === "done" ? "done" : a.status === "failed" ? "failed" : "running";
  return {
    id: a.id,
    title: a.title,
    status,
    continuous: status === "running",
  };
}

function setDraft(v: string): void {
  draft = v;
}

function newChat(): void {
  sessionStore.clearActive();
  messages = greeting();
  events = [];
}

async function openSession(id: string): Promise<void> {
  const d = await aiApi.sessionDetail(id);
  sessionStore.setActive(id, {
    provider: d.session.provider,
    model: d.session.model,
  });
  // Machine turns (role "tool") stay server-side; the chat shows people only.
  messages = d.messages
    .filter((m) => m.role !== "tool")
    .map((m) => ({
      id: m.id,
      role: m.role === "assistant" ? "assistant" : "user",
      text: m.content,
    }));
  events = d.actions.map(toEvent);
  // View-only: the global selection is untouched. Follow-ups run under the
  // active combo until the user explicitly adopts this session's (see
  // adoptSessionCombo, surfaced in CenterPanel).
}

/** Adopt the viewed session's combo as the global selection. */
async function adoptSessionCombo(): Promise<void> {
  const combo = sessionStore.sessionCombo;
  if (!combo) return;
  await providerStore.selectProvider(combo.provider, combo.model ?? undefined);
  sessionStore.clearCombo();
}

async function deleteSession(id: string): Promise<void> {
  await aiApi.deleteSession(id);
  sessionStore.removeFromList(id);
  if (sessionStore.activeSessionId === id) newChat();
}

/**
 * Real send: the turn is persisted server-side (session + history).
 * Plain chat never creates actions — those come only from command execution.
 * Returns whether the text was dispatched (false = dropped: empty or the
 * agent is still thinking; callers keep the text instead of losing it).
 */
let sendController: AbortController | null = null;

async function send(text: string): Promise<boolean> {
  const clean = text.trim();
  if (!clean || orb === "thinking") return false;
  messages = [...messages, { role: "user", text: clean }];
  draft = "";
  orb = "thinking";
  const controller = new AbortController();
  sendController = controller;
  try {
    const res = await aiApi.run(clean, sessionStore.activeSessionId, {
      lang: getLang(),
      signal: controller.signal,
    });
    sessionStore.setActive(res.session_id, null);
    contextUsed = res.context.used_tokens;
    if (res.context.window != null)
      providerStore.setContextWindow(res.context.window);
    const d = await aiApi.sessionDetail(res.session_id);
    const steps = (res.steps ?? []).map((s) => ({ tool: s.tool, ok: s.ok }));
    // Machine turns (role "tool") never render as chat bubbles: without
    // this filter they show up as raw-JSON user messages.
    messages = d.messages
      .filter((m) => m.role !== "tool")
      .map((m, i, arr) => ({
        id: m.id,
        role: m.role === "assistant" ? "assistant" : "user",
        text: m.content,
        ...(i === arr.length - 1 && steps.length > 0 ? { steps } : {}),
      }));
    events = d.actions.map(toEvent);
    await sessionStore.refreshSessions();
    // The turn above ran under the active combo, so the viewed session now
    // continues under it too — the adopt affordance has served its purpose.
    sessionStore.clearCombo();
    notifyReply();
  } catch (err) {
    if (
      err instanceof DOMException &&
      err.name === "AbortError" &&
      controller.signal.aborted
    ) {
      // User hit Detener: no error bubble, the user message stays for retry.
    } else {
      const msg =
        err instanceof ApiError && err.code === "timeout"
          ? t("chat.timeout")
          : err instanceof Error
            ? err.message
            : "Error";
      messages = [...messages, { role: "assistant", text: `Error: ${msg}` }];
    }
  } finally {
    if (sendController === controller) sendController = null;
    orb = "idle";
  }
  return true;
}

/** Detener: abort the HTTP wait AND flag the server turn for unwind. */
async function cancel(): Promise<void> {
  sendController?.abort();
  try {
    await aiApi.cancel();
  } catch {
    /* best-effort: the local abort already freed the UI */
  }
}

export const chatStore = {
  get orb(): OrbState {
    return orb;
  },
  get messages(): ChatMsg[] {
    return messages;
  },
  get events(): AppEvent[] {
    return events;
  },
  get contextUsed(): number {
    return contextUsed;
  },
  get draft(): string {
    return draft;
  },
  setDraft,
  setOrb,
  newChat,
  openSession,
  adoptSessionCombo,
  deleteSession,
  send,
  cancel,
};
