import { describe, expect, test } from "vitest";
import { defaultModelFor } from "./providers.store.svelte";
import type { ProviderInfo } from "./assistant.api";

const entry = (over: Partial<ProviderInfo>): ProviderInfo => ({
  id: "ollama",
  name: "ollama",
  available: true,
  models: ["m1", "m2"],
  default_model: "m1",
  needs_key: false,
  key_paste: false,
  context_window: 8192,
  startable: true,
  installed: true,
  ...over,
});

describe("defaultModelFor", () => {
  test("prefers default_model, falls back to first model, then empty", () => {
    expect(defaultModelFor([entry({})], "ollama")).toBe("m1");
    expect(
      defaultModelFor([entry({ default_model: "", models: ["z9"] })], "ollama"),
    ).toBe("z9");
    expect(
      defaultModelFor([entry({ default_model: "", models: [] })], "ollama"),
    ).toBe("");
    expect(defaultModelFor([], "ollama")).toBe("");
    expect(defaultModelFor([entry({})], "missing")).toBe("");
  });
});
