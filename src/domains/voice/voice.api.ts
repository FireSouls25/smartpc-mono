import { api } from "../../lib/api";

export type VoiceMode = "manual" | "wake" | "conversation";

export interface VoiceStatus {
  listening: boolean;
  capturing: boolean;
  mode: VoiceMode | null;
  model: string;
  wake_word: string | null;
  mic: boolean;
  device: string | null;
  inputs: string[];
  model_ready: boolean;
  /** Per-model whisper download state (see stt/model.rs ALL_MODELS). */
  models_ready: Record<string, boolean>;
  /** OS capture gain, when the platform exposes it (Linux only for now). */
  mixer: { volume: number; muted: boolean } | null;
}

export type VoiceEvent =
  | { seq: number; epoch: number; type: "started" }
  | { seq: number; epoch: number; type: "capturing"; active: boolean }
  | { seq: number; epoch: number; type: "wake"; word: string }
  | { seq: number; epoch: number; type: "transcript"; text: string }
  | { seq: number; epoch: number; type: "error"; code: string; message: string }
  | { seq: number; epoch: number; type: "end" }
  // A2 owned playback: Rodio Sink completion is real. Global event (not a
  // listen session) — the poll loop applies it regardless of epoch.
  | {
      seq: number;
      epoch: number;
      type: "speaking";
      active: boolean;
      chunk: number;
      chunks: number;
      model: string;
    };

export interface TtsModel {
  id: string;
  lang: string;
  label: string;
  /** "male" | "female" — the dropdown groups by this. */
  gender: string;
  size_mb: number;
  quality: string;
  /** A2 per-voice download URL (absent on older sidecars — optional). */
  url?: string;
}

export interface TtsModelsResponse {
  models: TtsModel[];
  default_for_lang: Record<string, string>;
  active: string | null;
  /** Per-id readiness (mirrors the STT `models_ready` shape). */
  ready: Record<string, boolean>;
}

export const voiceApi = {
  status: () => api<VoiceStatus>("/v1/voice/status"),

  /** Blocks on first-use model download — generous budget. */
  listen: (opts: {
    mode: VoiceMode;
    wake_word: string;
    lang: string;
    device?: string;
    model?: string;
    threshold?: number;
  }) =>
    api<{ ok: boolean; epoch: number }>("/v1/voice/listen", {
      method: "POST",
      body: opts,
      timeoutMs: 180000,
    }),

  stop: () =>
    api<{ ok: boolean }>("/v1/voice/stop", {
      method: "POST",
      timeoutMs: 10000,
    }),

  /** Long-poll: the server holds up to ~25 s for news. */
  poll: (cursor: number) =>
    api<{ events: VoiceEvent[]; next: number }>(
      `/v1/voice/events?cursor=${cursor}`,
      { timeoutMs: 35000 },
    ),

  /**
   * Speak text aloud (queued server-side on the Rodio Sink; completion
   * arrives as `speaking{active,chunk,chunks}` poll events). Per-chunk
   * call: the store splits long replies and posts each chunk, which the
   * server synthesizes and queues in order. `voice` is a catalog id from
   * `ttsModels` (unknown → 400 `invalid_voice`); omit for the default.
   * `estimated_ms` stays as the client fallback budget (engines without
   * speaking events). First-use model download rides this call (180 s
   * budget on the explicit download/test-play callers).
   */
  speak: (
    text: string,
    lang: string,
    voice?: string,
    opts?: { timeoutMs?: number },
  ) =>
    api<{ ok: boolean; estimated_ms: number; model: string }>(
      "/v1/voice/speak",
      {
        method: "POST",
        body: voice ? { text, lang, voice } : { text, lang },
        timeoutMs: opts?.timeoutMs ?? 30000,
      },
    ),

  /** TTS voice catalog (always 200; empty models = engine missing). */
  ttsModels: () => api<TtsModelsResponse>("/v1/voice/tts-models"),

  /** Uninstall a downloaded TTS voice model (idempotent). */
  deleteTtsModel: (id: string) =>
    api<{ ok: boolean; removed: boolean }>(
      `/v1/voice/tts-models/${encodeURIComponent(id)}`,
      { method: "DELETE", timeoutMs: 30000 },
    ),

  stopSpeaking: () =>
    api<{ ok: boolean }>("/v1/voice/speak-stop", {
      method: "POST",
      timeoutMs: 10000,
    }),

  /** Uninstall a downloaded whisper model (idempotent). */
  deleteSttModel: (name: string) =>
    api<{ ok: boolean; removed: boolean }>(
      `/v1/voice/models/${encodeURIComponent(name)}`,
      { method: "DELETE", timeoutMs: 30000 },
    ),
};
