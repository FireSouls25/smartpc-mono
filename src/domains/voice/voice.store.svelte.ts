// Local voice control: mic button, wake/manual sessions, transcript routing.
// Capture + VAD + whisper all live in the sidecar; this store owns the poll
// loop and hands final transcripts to the chat store (they render as normal
// user messages and run through the agent like typed text).
import { voiceApi, type VoiceEvent, type VoiceMode } from "./voice.api";
import { ApiError } from "../../lib/api";
import {
  chatStore as chat,
  onAssistantReply,
} from "../assistant/chat.store.svelte";
import { getLang, t } from "../../lib/i18n.svelte";

export type { VoiceMode };
export type VoicePhase =
  "idle" | "starting" | "listening" | "capturing" | "error";
export type VoiceSensitivity = "low" | "medium" | "high";

const MODE_KEY = "smartpc.voice.mode";
const WAKE_KEY = "smartpc.voice.wake";
const DEVICE_KEY = "smartpc.voice.device";
const SPEAK_KEY = "smartpc.voice.speak";
const SENS_KEY = "smartpc.voice.sensitivity";
const STT_MODEL_KEY = "smartpc.voice.sttModel";
const HOTKEY_KEY = "smartpc.voice.hotkey";
export const DEFAULT_WAKE_WORD = "hey";
/** Mic toggle shortcut. Shown on the mic button and editable in settings. */
export const DEFAULT_HOTKEY = "Control+m";
/** VAD energy threshold per sensitivity; medium omits it (server default). */
export const SENSITIVITY_THRESHOLD: Record<VoiceSensitivity, number | null> = {
  low: 0.035,
  medium: null,
  high: 0.01,
};

function loadMode(): VoiceMode {
  try {
    const v = window.localStorage.getItem(MODE_KEY);
    return v === "wake" || v === "conversation" ? v : "manual";
  } catch {
    return "manual";
  }
}

function loadWake(): string {
  try {
    const w = (window.localStorage.getItem(WAKE_KEY) || "").trim();
    return w ? w.slice(0, 32) : DEFAULT_WAKE_WORD;
  } catch {
    return DEFAULT_WAKE_WORD;
  }
}

function loadDevice(): string | null {
  try {
    const d = (window.localStorage.getItem(DEVICE_KEY) || "").trim();
    return d ? d.slice(0, 128) : null;
  } catch {
    return null;
  }
}

function loadSpeak(): boolean {
  try {
    return window.localStorage.getItem(SPEAK_KEY) === "1";
  } catch {
    return false;
  }
}

function loadSensitivity(): VoiceSensitivity {
  try {
    const v = window.localStorage.getItem(SENS_KEY);
    return v === "low" || v === "high" ? v : "medium";
  } catch {
    return "medium";
  }
}

function loadSttModel(): string {
  try {
    const m = (window.localStorage.getItem(STT_MODEL_KEY) || "").trim();
    return ["tiny", "tiny.en", "base", "base.en", "small"].includes(m) ? m : "";
  } catch {
    return "";
  }
}

function loadHotkey(): string {
  try {
    const h = (window.localStorage.getItem(HOTKEY_KEY) || "").trim();
    return h ? h.slice(0, 64) : DEFAULT_HOTKEY;
  } catch {
    return DEFAULT_HOTKEY;
  }
}

function persist(key: string, value: string): void {
  try {
    window.localStorage.setItem(key, value);
  } catch {
    /* private mode */
  }
}

const sleep = (ms: number): Promise<void> =>
  new Promise((r) => setTimeout(r, ms));

function setNotice(text: string): void {
  notice = text;
  if (noticeTimer !== null) window.clearTimeout(noticeTimer);
  if (text) {
    noticeTimer = window.setTimeout(() => {
      notice = "";
      noticeTimer = null;
    }, 4000);
  }
}

let phase = $state<VoicePhase>("idle");
let mode = $state<VoiceMode>(loadMode());
let wakeWord = $state<string>(loadWake());
let device = $state<string | null>(loadDevice());
let error = $state("");
// TTS output: auto-read replies aloud when enabled. `speaking` is
// optimistic (the server gives no completion events by design); the timer
// mirrors the server watchdog so the UI recovers even if audio overruns.
let speakEnabled = $state<boolean>(loadSpeak());
let sensitivity = $state<VoiceSensitivity>(loadSensitivity());
let sttModel = $state<string>(loadSttModel());
let hotkey = $state<string>(loadHotkey());
let speaking = $state(false);
let speakTimer: number | null = null;
let lastSpokenId: string | null = null;
// Transient acknowledgment ("heard the wake word, talk now"), cleared after
// a few seconds or on the next state change.
let notice = $state("");
let noticeTimer: number | null = null;
// Generation counter: stale async work (a listen that resolves after stop,
// an old poll loop) stands down instead of fighting the current state.
let run = 0;
let cursor = 0;

function mapListenError(err: unknown): string {
  const code = err instanceof ApiError ? err.code : undefined;
  if (code === "no_microphone" || code === "unsupported_audio") {
    return t("voice.unsupported");
  }
  if (code === "timeout") return t("voice.downloading");
  return t("voice.error") + (code ? ` (${code})` : "");
}

async function start(requested?: VoiceMode): Promise<void> {
  if (phase === "starting" || phase === "listening" || phase === "capturing") {
    return;
  }
  const my = ++run;
  if (requested) setMode(requested);
  phase = "starting";
  error = "";
  setNotice("");
  let epoch: number;
  // Sensitivity rides per call (medium = server default, omitted);
  // the STT model too (first use downloads it, later listens are instant).
  const sensThreshold = SENSITIVITY_THRESHOLD[sensitivity];
  try {
    const res = await voiceApi.listen({
      mode,
      wake_word: wakeWord,
      lang: getLang(),
      ...(device ? { device } : {}),
      ...(sttModel ? { model: sttModel } : {}),
      ...(sensThreshold != null ? { threshold: sensThreshold } : {}),
    });
    epoch = res.epoch;
  } catch (err) {
    if (my !== run) return;
    phase = "error";
    error = mapListenError(err);
    return;
  }
  if (my !== run) {
    // Stopped while the (possibly downloading) listen was in flight:
    // release the orphan session server-side.
    try {
      await voiceApi.stop();
    } catch {
      /* already gone */
    }
    return;
  }
  cursor = 0;
  phase = "listening";
  chat.setOrb("listening");
  void pollLoop(my, epoch);
}

async function pollLoop(my: number, epoch: number): Promise<void> {
  while (my === run) {
    let batch;
    try {
      batch = await voiceApi.poll(cursor);
    } catch {
      if (my !== run) return;
      // Transient (sidecar restart, blip): the server holds session state,
      // so back off and resume polling with the same cursor.
      await sleep(1000);
      continue;
    }
    if (my !== run) return;
    cursor = batch.next;
    for (const ev of batch.events) {
      if (my !== run) return;
      // The queue is global across sessions: drop anything that isn't ours
      // (a previous session's Transcript/End replayed here used to kill the
      // new poll loop and orphan a live session → permanent 409s).
      if (ev.epoch !== epoch) continue;
      await handle(ev);
    }
  }
}

async function handle(ev: VoiceEvent): Promise<void> {
  switch (ev.type) {
    case "started":
      // Session confirmed live; we're already in listening.
      break;
    case "capturing":
      phase = ev.active ? "capturing" : "listening";
      chat.setOrb("listening");
      break;
    case "wake":
      // The session heard the wake word and is recording the command:
      // say so out loud in the UI, or users talk into the void.
      phase = "capturing";
      setNotice(t("voice.hello"));
      break;
    case "transcript":
      setNotice("");
      await onTranscript(ev.text);
      break;
    case "error": {
      // Server errors are terminal (the session ends server-side too), so
      // force local cleanup first: this guarantees the next press starts
      // fresh instead of 409ing on a stuck record.
      const msg = t("voice.error") + (ev.message ? `: ${ev.message}` : "");
      await stop();
      phase = "error";
      error = msg;
      break;
    }
    case "end":
      setNotice("");
      phase = "idle";
      run++;
      if (chat.orb === "listening") chat.setOrb("idle");
      break;
  }
}

async function onTranscript(text: string): Promise<void> {
  const clean = text.trim();
  if (!clean) return;
  if (conversationActive) {
    await onConversationTranscript(clean);
    return;
  }
  const dispatched = await chat.send(clean);
  if (!dispatched) {
    // Agent busy: keep the words in the composer instead of losing them.
    chat.setDraft(clean);
    error = t("voice.busy");
  }
}

/* Continuous conversation (half-duplex ChatGPT-voice-style loop):
 * listen → send → reply → speak → listen again. No echo cancellation in
 * this stack, so turns are strictly sequential and the speaker's own
 * reply is echo-guarded (dropped, loop resumes) instead of answered.
 * Barge-in is the toggle/hotkey: it cuts speech AND ends the loop. */

const CONVO_MAX_TURNS = 30;
const CONVO_NEXT_PAUSE_MS = 800;

let conversationActive = $state(false);
let convoTurns = 0;
let convoTimer: number | null = null;
let lastSpokenNorm = "";

function normEcho(s: string): string {
  return s
    .toLowerCase()
    .replace(/[^a-z0-9áéíóúñü ]/gi, " ")
    .replace(/\s+/g, " ")
    .trim();
}

/** The mic heard our own TTS (speaker bleed): short overlaps don't count. */
function isEchoSpoken(text: string): boolean {
  const n = normEcho(text);
  if (n.length < 12 || !lastSpokenNorm) return false;
  return lastSpokenNorm.includes(n) || n.includes(lastSpokenNorm);
}

function clearConvoTimer(): void {
  if (convoTimer !== null) {
    window.clearTimeout(convoTimer);
    convoTimer = null;
  }
}

function stopConversation(): void {
  conversationActive = false;
  clearConvoTimer();
}

function scheduleNextTurn(): void {
  if (!conversationActive) return;
  clearConvoTimer();
  convoTimer = window.setTimeout(() => {
    convoTimer = null;
    if (!conversationActive) return;
    if (convoTurns >= CONVO_MAX_TURNS) {
      error = t("voice.convoLimit");
      void stopAll();
      return;
    }
    void start();
  }, CONVO_NEXT_PAUSE_MS);
}

function startConversation(): void {
  if (
    phase === "starting" ||
    phase === "listening" ||
    phase === "capturing" ||
    conversationActive
  ) {
    return;
  }
  setMode("conversation");
  conversationActive = true;
  convoTurns = 0;
  error = "";
  void start();
}

async function onConversationTranscript(clean: string): Promise<void> {
  convoTurns++;
  if (isEchoSpoken(clean)) {
    // Own reply through the speakers: ignore, keep listening.
    scheduleNextTurn();
    return;
  }
  const dispatched = await chat.send(clean);
  if (!dispatched) {
    // Agent busy mid-loop: park the words, end the loop (no pile-up).
    chat.setDraft(clean);
    error = t("voice.busy");
    void stopAll();
    return;
  }
  // The reply arrives via onAssistantReply → forced speak → next turn is
  // scheduled when speech ends. Nothing to do here but wait.
}

async function stop(): Promise<void> {
  stopConversation();
  run++;
  setNotice("");
  const wasActive =
    phase === "starting" || phase === "listening" || phase === "capturing";
  phase = "idle";
  if (wasActive) {
    try {
      await voiceApi.stop();
    } catch {
      /* already gone */
    }
  }
  if (chat.orb === "listening") chat.setOrb("idle");
}

function toggle(): void {
  if (
    phase === "starting" ||
    phase === "listening" ||
    phase === "capturing" ||
    conversationActive ||
    speaking
  ) {
    // Barge-in: cuts speech AND ends the loop (conversation included).
    void stopAll();
  } else if (mode === "conversation") {
    startConversation();
  } else {
    void start();
  }
}

/** Full stop: loop flag, speech, session. */
async function stopAll(): Promise<void> {
  stopConversation();
  await stopSpeaking();
  await stop();
}

function setMode(m: VoiceMode): void {
  mode = m;
  persist(MODE_KEY, m);
}

function setSensitivity(s: VoiceSensitivity): void {
  sensitivity = s;
  persist(SENS_KEY, s);
}

function setSttModel(m: string): void {
  const clean = m.trim().slice(0, 32);
  sttModel = ["tiny", "tiny.en", "base", "base.en", "small"].includes(clean)
    ? clean
    : "";
  persist(STT_MODEL_KEY, sttModel);
}

function setHotkey(combo: string): void {
  const clean = combo.trim().slice(0, 64) || DEFAULT_HOTKEY;
  hotkey = clean;
  persist(HOTKEY_KEY, clean);
}

/** "Control+Shift+K" from a keydown; bare modifiers never form a combo. */
export function formatHotkey(e: KeyboardEvent): string {
  const parts: string[] = [];
  if (e.ctrlKey) parts.push("Control");
  if (e.metaKey) parts.push("Meta");
  if (e.altKey) parts.push("Alt");
  if (e.shiftKey) parts.push("Shift");
  if (!["Control", "Shift", "Alt", "Meta"].includes(e.key)) {
    parts.push(e.key.length === 1 ? e.key.toLowerCase() : e.key);
  }
  return parts.join("+");
}

/** Short display form ("Control+m" → "Ctrl+M"). */
export function displayHotkey(combo: string): string {
  return combo
    .split("+")
    .map((p) =>
      p === "Control"
        ? "Ctrl"
        : p === "Meta"
          ? "Meta"
          : p.length === 1
            ? p.toUpperCase()
            : p,
    )
    .join("+");
}

let hotkeySuspended = false;

/** Settings capture mode sets this so the combo being recorded doesn't fire. */
export function suspendHotkey(v: boolean): void {
  hotkeySuspended = v;
}

let hotkeyInstalled = false;

/**
 * Global mic toggle shortcut (installed once from Shell). Fires from
 * anywhere — typing included — but never while recording a new combo.
 */
export function installVoiceHotkey(): () => void {
  if (hotkeyInstalled || typeof window === "undefined") return () => {};
  hotkeyInstalled = true;
  const onKey = (e: KeyboardEvent) => {
    if (hotkeySuspended || e.repeat) return;
    if (formatHotkey(e) !== hotkey) return;
    e.preventDefault();
    toggle();
  };
  window.addEventListener("keydown", onKey);
  return () => {
    window.removeEventListener("keydown", onKey);
    hotkeyInstalled = false;
  };
}

function setWakeWord(w: string): void {
  wakeWord = w.trim().slice(0, 32) || DEFAULT_WAKE_WORD;
  persist(WAKE_KEY, wakeWord);
}

function setDevice(d: string | null): void {
  device = d?.trim().slice(0, 128) || null;
  if (device) persist(DEVICE_KEY, device);
  else {
    try {
      window.localStorage.removeItem(DEVICE_KEY);
    } catch {
      /* private mode */
    }
  }
}

function setSpeakEnabled(on: boolean): void {
  speakEnabled = on;
  persist(SPEAK_KEY, on ? "1" : "0");
  ensureReplySub();
  if (!on) void stopSpeaking();
}

let replyUnsub: (() => void) | null = null;

/** Speak fresh replies (never history loads — see onAssistantReply). */
function ensureReplySub(): void {
  if (replyUnsub) return;
  replyUnsub = onAssistantReply((id, text) => {
    if (!id || !text?.trim()) return;
    if (id === lastSpokenId) return;
    lastSpokenId = id;
    // Conversation mode is TTS-first-class (always spoken); normal mode
    // stays mute unless the speak toggle opted in.
    if (!conversationActive && !speakEnabled) return;
    void speakText(text);
  });
}

ensureReplySub();

function clearSpeakTimer(): void {
  if (speakTimer !== null) {
    window.clearTimeout(speakTimer);
    speakTimer = null;
  }
}

/** Read text aloud (barge-in: cuts anything playing). No-op on failure. */
async function speakText(text: string): Promise<void> {
  const clean = text.trim().slice(0, 2000);
  if (!clean) return;
  clearSpeakTimer();
  lastSpokenNorm = normEcho(clean);
  try {
    const res = await voiceApi.speak(clean, getLang());
    speaking = true;
    speakTimer = window.setTimeout(
      () => {
        speaking = false;
        speakTimer = null;
        // Conversation turn-taking: speech "end" is the watchdog estimate
        // (the server emits no completion events by design).
        if (conversationActive) scheduleNextTurn();
      },
      Math.min(res.estimated_ms + 5000, 250000),
    );
  } catch {
    speaking = false;
    if (conversationActive) {
      // Conversation needs voice: degrade honestly instead of looping mute.
      error = t("voice.speakFailed");
      void stopAll();
    }
  }
}

async function stopSpeaking(): Promise<void> {
  clearSpeakTimer();
  speaking = false;
  try {
    await voiceApi.stopSpeaking();
  } catch {
    /* already quiet */
  }
}

export const voice = {
  get phase(): VoicePhase {
    return phase;
  },
  get mode(): VoiceMode {
    return mode;
  },
  get wakeWord(): string {
    return wakeWord;
  },
  get device(): string | null {
    return device;
  },
  get sensitivity(): VoiceSensitivity {
    return sensitivity;
  },
  get sttModel(): string {
    return sttModel;
  },
  get hotkey(): string {
    return hotkey;
  },
  get speakEnabled(): boolean {
    return speakEnabled;
  },
  get speaking(): boolean {
    return speaking;
  },
  get error(): string {
    return error;
  },
  get notice(): string {
    return notice;
  },
  get listening(): boolean {
    return phase === "listening" || phase === "capturing";
  },
  get capturing(): boolean {
    return phase === "capturing";
  },
  get conversationActive(): boolean {
    return conversationActive;
  },
  start,
  stop,
  toggle,
  setMode,
  setSensitivity,
  setSttModel,
  setHotkey,
  setWakeWord,
  setDevice,
  setSpeakEnabled,
  speakText,
  stopSpeaking,
};
