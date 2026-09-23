<script lang="ts">
  import MatrixOrb from "../../components/ui/matrix-orb.svelte";
  import SelectMenu from "../../shared/SelectMenu.svelte";
  import ProviderStart from "./ProviderStart.svelte";
  import { chatStore as chat } from "./chat.store.svelte";
  import { providerStore as providers } from "./providers.store.svelte";
  import { sessionStore as sessions } from "./sessions.store.svelte";
  import { voice } from "../voice/voice.store.svelte";
  import { displayHotkey } from "../voice/voice.store.svelte";
  import { t } from "../../lib/i18n.svelte";

  const orbLabels = () => ({
    idle: t("orb.idle"),
    listening: t("orb.listening"),
    thinking: t("orb.thinking"),
  });

  let scrollEl: HTMLDivElement | null = null;

  // Follow the conversation while the user stays near the bottom;
  // never yank them away when they scrolled up to read history.
  $effect(() => {
    void chat.messages.length;
    const el = scrollEl;
    if (!el) return;
    if (el.scrollHeight - el.scrollTop - el.clientHeight < 160) {
      el.scrollTo({ top: el.scrollHeight });
    }
  });

  function submit(e: SubmitEvent) {
    e.preventDefault();
    // Barge-in: newly typed input wins over in-flight speech.
    void voice.stopSpeaking();
    void chat.send(chat.draft);
  }

  const combo = () => sessions.sessionCombo;

  /** Mic tooltip: what a press (or the hotkey) does in the current mode. */
  const micTitle = () =>
    voice.mode === "wake"
      ? `${t("voice.modeWake")} («${voice.wakeWord}») · ${displayHotkey(voice.hotkey)}`
      : voice.mode === "conversation"
        ? `${t("voice.modeConvo")} · ${displayHotkey(voice.hotkey)}`
        : `${t("voice.modeManual")} · ${displayHotkey(voice.hotkey)}`;
</script>

<div class="flex h-full min-h-0 flex-1 flex-col gap-4">
  <div
    class="card-xl flex flex-col items-center"
    style="padding-top: 1rem; padding-bottom: 1rem;"
  >
    <MatrixOrb
      state={chat.orb}
      size={180}
      color="#f04e00"
      labels={orbLabels()}
    />
  </div>

  <div class="card-xl flex min-h-0 flex-1 flex-col gap-3">
    <div
      bind:this={scrollEl}
      class="flex min-h-0 flex-1 flex-col gap-2 overflow-y-auto"
      aria-live="polite"
    >
      {#each chat.messages as m, i (m.id ?? `local-${i}`)}
        {#if m.role === "user"}
          <div class="msg-in flex justify-end">
            <p class="bubble-user">{m.text}</p>
          </div>
        {:else}
          <div class="msg-in flex justify-start">
            <div class="flex max-w-[85%] flex-col gap-1">
              <p class="bubble-assistant" style="max-width: 100%;">
                {m.textKey ? t(m.textKey) : m.text}
              </p>
              {#if m.steps?.length}
                <div class="flex flex-wrap gap-1 pl-1">
                  {#each m.steps as st, j (j)}
                    <span
                      class="chip"
                      style="font-size: 10px; padding: 2px 8px;"
                      title={st.ok ? "tool ok" : "tool failed"}
                    >
                      <span
                        class="dot"
                        style="background: {st.ok
                          ? 'var(--success)'
                          : 'var(--danger)'}; height: 6px; width: 6px;"
                      ></span>
                      {st.tool}
                    </span>
                  {/each}
                </div>
              {/if}
            </div>
          </div>
        {/if}
      {/each}
    </div>

    <form
      onsubmit={submit}
      class="flex items-center gap-2 rounded-2xl border p-2"
      style="border-color: var(--border-strong); background: var(--bg);"
    >
      <button
        type="button"
        class="icon-btn shrink-0"
        style={voice.capturing
          ? "border-color: var(--danger); color: var(--danger);"
          : voice.listening
            ? "border-color: var(--accent); color: var(--accent);"
            : ""}
        onclick={() => voice.toggle()}
        aria-label={voice.mode === "wake"
          ? `${t("orb.listening")} (${voice.wakeWord})`
          : t("orb.listening")}
        aria-pressed={voice.listening}
        title={micTitle()}
        disabled={voice.phase === "starting"}
      >
        <svg
          viewBox="0 0 24 24"
          class="h-5 w-5"
          fill="none"
          stroke="currentColor"
          stroke-width="1.8"
          stroke-linecap="round"
          stroke-linejoin="round"
        >
          <rect x="9" y="3" width="6" height="11" rx="3" />
          <path d="M5 11a7 7 0 0 0 14 0M12 18v3" />
        </svg>
      </button>
      <input
        class="min-w-0 flex-1 bg-transparent text-sm outline-none"
        style="color: var(--fg);"
        placeholder={t("chat.placeholder")}
        value={chat.draft}
        oninput={(e) => chat.setDraft(e.currentTarget.value)}
      />
      <button
        class="btn shrink-0 {chat.orb === 'thinking' ? '' : 'btn-primary'}"
        type={chat.orb === "thinking" ? "button" : "submit"}
        style={chat.orb === "thinking"
          ? "background: var(--danger); color: #fff;"
          : ""}
        onclick={() => {
          if (chat.orb === "thinking") void chat.cancel();
        }}
        aria-label={chat.orb === "thinking" ? t("chat.stop") : t("chat.send")}
      >
        {#if chat.orb === "thinking"}
          <svg
            viewBox="0 0 24 24"
            class="h-4 w-4"
            fill="currentColor"
            aria-hidden="true"
          >
            <rect x="6" y="6" width="12" height="12" rx="2" />
          </svg>
          {t("chat.stop")}
        {:else}
          <svg
            viewBox="0 0 24 24"
            class="h-4 w-4"
            fill="none"
            stroke="currentColor"
            stroke-width="1.8"
            stroke-linecap="round"
            stroke-linejoin="round"
          >
            <path d="M22 2 11 13M22 2l-7 20-4-9-9-4z" />
          </svg>
          {t("chat.send")}
        {/if}
      </button>
    </form>
    {#if voice.error}
      <p class="error-box">{voice.error}</p>
    {/if}
    {#if voice.phase === "starting"}
      <p class="faint text-xs">{t("voice.starting")}</p>
    {/if}
    {#if voice.notice}
      <p class="text-xs font-semibold" style="color: var(--accent);">
        {voice.notice}
      </p>
    {/if}
    {#if voice.speaking}
      <button
        type="button"
        class="faint text-xs underline"
        onclick={() => void voice.stopSpeaking()}
      >
        {t("voice.speaking")} ✕
      </button>
    {/if}
    {#if voice.listening && !voice.capturing && voice.mode === "wake"}
      <p class="faint text-xs">{t("voice.sayHey", { word: voice.wakeWord })}</p>
    {/if}

    <div class="flex flex-wrap items-center gap-2">
      <SelectMenu
        label={t("chat.provider")}
        value={providers.activeProvider}
        options={providers.providers.map((p) => ({
          value: p.id,
          label: p.id,
          disabled: !p.available,
          hint: p.available ? undefined : t("providers.offline"),
        }))}
        onChange={(v) => void providers.selectProvider(v)}
      />
      <SelectMenu
        label={t("chat.model")}
        value={providers.activeModel}
        options={providers.activeModels().map((m) => ({ value: m, label: m }))}
        onChange={(v) => void providers.selectModel(v)}
      />
      {#if combo() && (combo()!.provider !== providers.activeProvider || (combo()!.model ?? "") !== providers.activeModel)}
        <button
          class="chip"
          style="border-color: var(--accent); color: var(--fg);"
          onclick={() => void chat.adoptSessionCombo()}
          title={`${combo()!.provider}${combo()!.model ? ` · ${combo()!.model}` : ""}`}
        >
          {t("sessions.useModel")}
        </button>
      {/if}
      <button
        class="chip"
        onclick={() => void providers.loadProviders()}
        aria-label={t("providers.refresh")}
      >
        ↻ {t("providers.refresh")}
      </button>
    </div>
    {#if providers.selectError}
      <p class="error-box">{providers.selectError}</p>
    {/if}
    {#if !providers.activeAvailable() && !providers.providersLoading}
      <p class="chip" style="border-color: var(--warn); color: var(--warn);">
        {t("providers.needServer")}
      </p>
      <ProviderStart providerId={providers.activeProvider} />
    {/if}
  </div>
</div>
