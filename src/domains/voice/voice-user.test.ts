import { beforeEach, describe, expect, test, vi } from "vitest";

// Per-account TTS voice: two local accounts on one device keep their own
// picks (cloud prefs carry them across machines). localStorage is stubbed;
// the store must never touch the real one.
function stubStorage(initial: Record<string, string> = {}) {
  const map = new Map(Object.entries(initial));
  vi.stubGlobal("window", {
    location: { hash: "" },
    addEventListener: () => {},
    localStorage: {
      getItem: (k: string) => (map.has(k) ? map.get(k)! : null),
      setItem: (k: string, v: string) => void map.set(k, v),
      removeItem: (k: string) => void map.delete(k),
    },
  });
  return map;
}

describe("per-user tts voice", () => {
  beforeEach(() => {
    vi.resetModules();
    stubStorage();
  });

  test("switching accounts switches the remembered voice", async () => {
    const { voice, setVoiceUser } = await import("./voice.store.svelte");
    // Fresh device: nothing chosen.
    expect(voice.ttsVoice).toBe("");
    // A picks Alan (under the current UI language).
    setVoiceUser("user-a");
    voice.setTtsVoice("upstream-piper-en-alan");
    expect(voice.ttsVoice).toBe("upstream-piper-en-alan");
    // B starts blank, picks Lessac; A's pick is untouched.
    setVoiceUser("user-b");
    expect(voice.ttsVoice).toBe("");
    voice.setTtsVoice("upstream-piper-en-lessac");
    setVoiceUser("user-a");
    expect(voice.ttsVoice).toBe("upstream-piper-en-alan");
    setVoiceUser("user-b");
    expect(voice.ttsVoice).toBe("upstream-piper-en-lessac");
  });

  test("legacy device-global pick is adopted once, then namespaced", async () => {
    stubStorage({ "smartpc.voice.ttsVoice.es": "legacy-voice" });
    const { voice, setVoiceUser } = await import("./voice.store.svelte");
    setVoiceUser("user-a");
    expect(voice.ttsVoice).toBe("legacy-voice");
    // A new pick no longer touches the legacy key.
    voice.setTtsVoice("new-voice");
    const { ttsVoiceKeyFor } = await import("./voice.store.svelte");
    expect(window.localStorage.getItem("smartpc.voice.ttsVoice.es")).toBe(
      "legacy-voice",
    );
    expect(window.localStorage.getItem(ttsVoiceKeyFor("user-a", "es"))).toBe(
      "new-voice",
    );
  });
});
