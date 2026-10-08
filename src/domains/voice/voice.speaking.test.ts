import { beforeEach, describe, expect, test, vi } from "vitest";
import type { VoiceEvent } from "./voice.api";

type SpeakingEvent = Extract<VoiceEvent, { type: "speaking" }>;

function speaking(
  seq: number,
  active: boolean,
  chunk: number,
  chunks: number,
): SpeakingEvent {
  return {
    seq,
    epoch: 0,
    type: "speaking",
    active,
    chunk,
    chunks,
    model: "test-voice",
  };
}

async function freshVoice() {
  vi.resetModules();
  return import("./voice.store.svelte");
}

describe("A2 speaking events", () => {
  beforeEach(() => {
    vi.resetModules();
  });

  test("active event raises speaking + advances the chunk highlight", async () => {
    const { voice, voiceTest } = await freshVoice();
    expect(voice.speaking).toBe(false);
    voiceTest.applySpeakingEvent(speaking(1, true, 1, 3));
    expect(voice.speaking).toBe(true);
    expect(voice.speakChunkIndex).toBe(1);
    expect(voice.speakChunkCount).toBe(3);
    voiceTest.applySpeakingEvent(speaking(2, true, 2, 3));
    expect(voice.speaking).toBe(true);
    expect(voice.speakChunkIndex).toBe(2);
    expect(voice.speakChunkCount).toBe(3);
  });

  test("inactive event clears speaking + progress", async () => {
    const { voice, voiceTest } = await freshVoice();
    voiceTest.applySpeakingEvent(speaking(1, true, 2, 3));
    expect(voice.speaking).toBe(true);
    voiceTest.applySpeakingEvent(speaking(2, false, 3, 3));
    expect(voice.speaking).toBe(false);
    expect(voice.speakChunkIndex).toBe(0);
    expect(voice.speakChunkCount).toBe(0);
  });

  test("stale (replayed) events are dropped", async () => {
    const { voice, voiceTest } = await freshVoice();
    voiceTest.applySpeakingEvent(speaking(5, true, 2, 3));
    expect(voice.speakChunkIndex).toBe(2);
    // Older seq replayed (queue replay, second poll loop): ignored.
    voiceTest.applySpeakingEvent(speaking(3, true, 1, 3));
    expect(voice.speakChunkIndex).toBe(2);
    expect(voice.speaking).toBe(true);
    // Same seq twice: second application is a no-op.
    voiceTest.applySpeakingEvent(speaking(5, false, 0, 0));
    expect(voice.speaking).toBe(true);
    expect(voice.speakChunkIndex).toBe(2);
  });

  test("zero chunk counts keep the pump queue length", async () => {
    const { voice, voiceTest } = await freshVoice();
    voiceTest.applySpeakingEvent(speaking(1, true, 0, 0));
    expect(voice.speaking).toBe(true);
    // Unknown counts never collapse the highlight to 0/0 mid-speech.
    expect(voice.speakChunkIndex).toBe(0);
    expect(voice.speakChunkCount).toBe(0);
  });
});
