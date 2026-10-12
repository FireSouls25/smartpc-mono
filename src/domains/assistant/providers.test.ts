import { describe, expect, test } from "vitest";
import { providerStore } from "./providers.store.svelte";

// Fresh accounts have nothing preselected: sends (typed or voice) ask
// first via requireSelection instead of running under a silent default.
describe("requireSelection", () => {
  test("false with nothing chosen, true once both are set", () => {
    expect(providerStore.activeProvider).toBe("");
    expect(providerStore.activeModel).toBe("");
    expect(providerStore.requireSelection()).toBe(false);
    expect(providerStore.selectError).toBeTruthy();
  });
});
