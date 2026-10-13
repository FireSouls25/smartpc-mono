// Cross-machine renderer preferences: curated keys mirrored through the
// sidecar (`PUT /v1/prefs` → cloud mirror on sync) and adopted on login
// (`GET /v1/prefs` → applied here). Hardware-local keys (mic, sensitivity,
// hotkey, STT model) and secrets never leave the device.
import { api } from "./api";
import { auth } from "../domains/auth/auth.store.svelte";
import { getTheme, setTheme, type Theme } from "./theme.svelte";
import { getLang, setLang, type Lang } from "./i18n.svelte";
import {
  voice,
  ttsVoiceKeyFor,
  setVoiceUser,
} from "../domains/voice/voice.store.svelte";
import type { VoiceMode } from "../domains/voice/voice.store.svelte";

const THEMES: Theme[] = ["light", "dark", "auto"];
const MODES: VoiceMode[] = ["manual", "conversation", "wake"];

function snapshot(): Record<string, string> {
  // TTS voices are per account (voice store namespaces by user, with a
  // device fallback pre-login): read the same keys the store uses.
  const uid = auth.user?.id ?? null;
  let ttsEs = "";
  let ttsEn = "";
  try {
    ttsEs = window.localStorage.getItem(ttsVoiceKeyFor(uid, "es")) ?? "";
    ttsEn = window.localStorage.getItem(ttsVoiceKeyFor(uid, "en")) ?? "";
  } catch {
    /* private mode */
  }
  return {
    "ui.theme": getTheme(),
    "ui.lang": getLang(),
    "voice.tts.es": ttsEs,
    "voice.tts.en": ttsEn,
    "voice.wake": voice.wakeWord,
    "voice.mode": voice.mode,
  };
}

let lastSynced: string | null = null;
// Push stays quiet until one pull succeeds: otherwise the login-time
// push would overwrite the cloud set before it is adopted.
let pulledOnce = false;
const sig = (s: Record<string, string>): string => JSON.stringify(s);

/** Push when logged in and anything changed since the last push/apply. */
export async function pushPrefsIfChanged(): Promise<void> {
  if (!auth.token || !pulledOnce) return;
  const snap = snapshot();
  if (lastSynced !== null && sig(snap) === lastSynced) return;
  try {
    await api<{ ok: boolean }>("/v1/prefs", {
      method: "PUT",
      body: { prefs: snap },
      token: auth.token ?? undefined,
    });
    lastSynced = sig(snap);
  } catch {
    /* next change retries; local state is always authoritative here */
  }
}

/** Adopt the cloud prefs after auth (login/register/session resume). */
export async function pullApplyPrefs(): Promise<void> {
  // Voice picks are namespaced per account: switch first so both the
  // apply below and every later read hit this account's keys.
  setVoiceUser(auth.user?.id ?? null);
  if (!auth.token) return;
  try {
    const res = await api<{ prefs: { key: string; value: string }[] }>(
      "/v1/prefs",
      { token: auth.token ?? undefined },
    );
    const map = new Map(res.prefs.map((p) => [p.key, p.value]));
    const cur = getLang();
    const wantLang = map.get("ui.lang");
    const finalLang: Lang =
      wantLang === "es" || wantLang === "en" ? wantLang : cur;
    const theme = map.get("ui.theme");
    if (theme && (THEMES as string[]).includes(theme)) setTheme(theme as Theme);
    setLang(finalLang);
    const mode = map.get("voice.mode");
    if (mode && (MODES as string[]).includes(mode))
      voice.setMode(mode as VoiceMode);
    const wake = map.get("voice.wake");
    if (wake && wake.trim()) voice.setWakeWord(wake);
    // Voices are per-language keys: apply under each language in turn,
    // then restore the (possibly just applied) language.
    const es = map.get("voice.tts.es");
    if (es !== undefined) {
      setLang("es");
      voice.setTtsVoice(es);
    }
    const en = map.get("voice.tts.en");
    if (en !== undefined) {
      setLang("en");
      voice.setTtsVoice(en);
    }
    setLang(finalLang);
    lastSynced = sig(snapshot());
    pulledOnce = true;
  } catch {
    /* offline or fresh account: local values stand */
  }
}
