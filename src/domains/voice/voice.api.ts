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
  | { seq: number; epoch: number; type: "end" };

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

  /** Speak text aloud (fire-and-forget server-side with a watchdog). */
  speak: (text: string, lang: string) =>
    api<{ ok: boolean; estimated_ms: number }>("/v1/voice/speak", {
      method: "POST",
      body: { text, lang },
      timeoutMs: 30000,
    }),

  stopSpeaking: () =>
    api<{ ok: boolean }>("/v1/voice/speak-stop", {
      method: "POST",
      timeoutMs: 10000,
    }),
};
