<script lang="ts">
  import MatrixOrb from "../../components/ui/matrix-orb.svelte";
  import SelectMenu from "../../shared/SelectMenu.svelte";
  import ProviderStart from "./ProviderStart.svelte";
  import { chatStore as chat } from "./chat.store.svelte";
  import { formatReply } from "./reply.format";
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

  // Per-message actions (replay voice, copy plain text). `copiedKey`
  // shows brief feedback on the message just copied.
  let copiedKey = $state<string | null>(null);
  let copiedTimer: number | null = null;

  /** Plain-text copy: drop house markers (asterisks, list dashes). */
  function plainText(raw: string | undefined): string {
    if (!raw) return "";
    return raw
      .replace(/\*/g, "")
      .replace(/^- /gm, "")
      .replace(/\n{3,}/g, "\n\n")
      .trim();
  }

  async function copyText(key: string, raw: string | undefined): Promise<void> {
    const text = plainText(raw);
    if (!text) return;
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      // Non-secure contexts (plain http/file): legacy fallback.
      const area = document.createElement("textarea");
      area.value = text;
      area.style.position = "fixed";
      area.style.opacity = "0";
      document.body.appendChild(area);
      area.select();
      document.execCommand("copy");
      area.remove();
    }
    copiedKey = key;
    if (copiedTimer !== null) window.clearTimeout(copiedTimer);
    copiedTimer = window.setTimeout(() => {
      copiedKey = null;
      copiedTimer = null;
    }, 1500);
  }

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

  const voiceState = () =>
    voice.capturing
      ? t("voice.stateCapturing")
      : voice.listening
        ? t("voice.stateListening")
        : voice.phase === "starting"
          ? t("voice.stateStarting")
          : t("voice.stateIdle");

  const voiceMode = () =>
    voice.mode === "wake"
      ? t("voice.shortWake", { word: voice.wakeWord })
      : voice.mode === "conversation"
        ? t("voice.shortConvo")
        : t("voice.shortManual");

  const engineOn = () => providers.activeAvailable();
</script>

<div class="card-xl flex h-full min-h-0 flex-1 flex-col">
  <div class="panel-header">
    <h2 class="panel-title">{t("agent.title")}</h2>
    <span
      class="mono flex items-center gap-1.5 text-[11px]"
      style="color: var(--fg-muted);"
    >
      <span
        class="dot {chat.orb === 'thinking' ? 'animate-pulse' : ''}"
        style="background: {engineOn() ? 'var(--accent)' : 'var(--warn)'};"
      ></span>
      {engineOn() ? t("agent.connected") : t("providers.offline")}
    </span>
  </div>

  <!-- Presence block: centered orb, narrow telemetry column. -->
  <div
    class="flex flex-col items-center gap-3 border-b px-3 py-3"
    style="border-color: var(--border); background: var(--surface);"
  >
    <div class="relative flex shrink-0 items-center justify-center">
      <MatrixOrb
        state={chat.orb}
        size={144}
        color="#cba6f7"
        labels={orbLabels()}
      />
    </div>
    <div class="flex w-full max-w-md flex-col gap-1.5">
      <div
        class="mono border p-2 text-[11px]"
        style="border-color: var(--border); background: var(--crust);"
      >
        <div
          class="flex items-center justify-between gap-2 font-semibold"
          style="color: var(--fg);"
        >
          <span class="truncate"
            >{providers.activeModel || providers.activeProvider}</span
          >
          <span
            class="px-1 uppercase tracking-widest"
            style="background: var(--surface-2); color: var(--accent);"
          >
            {providers.activeProvider}
          </span>
        </div>
        <div class="mt-0.5" style="color: var(--fg-muted);">
          {providers.activeAvailable()
            ? t("agent.ready")
            : t("providers.needServer")}
        </div>
      </div>
      <div
        class="mono flex flex-col gap-1 border p-2 text-[11px]"
        style="border-color: var(--border); background: var(--crust); color: var(--fg-muted);"
      >
        <div class="flex items-center justify-between">
          <span class="uppercase">{t("voice.state")}:</span>
          <span
            class="uppercase"
            style="color: {voice.listening
              ? 'var(--teal)'
              : 'var(--fg-faint)'};"
          >
            {voiceState()}
          </span>
        </div>
        <div class="flex items-center justify-between">
          <span class="uppercase">{t("voice.mode")}:</span>
          <span class="uppercase" style="color: var(--fg);">
            {voiceMode()}
          </span>
        </div>
      </div>
      <div class="flex items-center gap-1.5">
        <button
          type="button"
          class="btn btn-ghost flex-1"
          style="padding: 0.375rem 0.5rem;"
          onclick={() => voice.setSpeakEnabled(!voice.speakEnabled)}
          aria-pressed={voice.speakEnabled}
          title={t("voice.speakHint")}
        >
          {voice.speakEnabled
            ? t("voice.mute").toUpperCase()
            : t("voice.speak").toUpperCase()}
        </button>
        {#if voice.speaking}
          <button
            type="button"
            class="btn btn-ghost"
            style="padding: 0.375rem 0.5rem;"
            onclick={() => void voice.stopSpeaking()}
            title={t("chat.stop")}
          >
            {#if voice.speakChunkCount > 1}
              {t("voice.chunkProgress", {
                i: String(voice.speakChunkIndex),
                n: String(voice.speakChunkCount),
              })} ✕
            {:else}
              {t("chat.stop").toUpperCase()}
            {/if}
          </button>
        {/if}
      </div>
    </div>
  </div>

  <div class="panel-body min-h-0 flex-1">
    <div
      bind:this={scrollEl}
      class="flex min-h-0 flex-1 flex-col gap-4 overflow-y-auto"
      aria-live="polite"
    >
      {#each chat.messages as m, i (m.id ?? `local-${i}`)}
        {#if m.role === "user"}
          <div class="msg-in flex justify-end">
            <div class="flex max-w-[95%] flex-col items-end gap-1">
              <p class="bubble-user">{m.text}</p>
            </div>
          </div>
        {:else}
          <div class="msg-in flex justify-start">
            <div class="flex w-full flex-col gap-1">
              <div class="reply-text">
                <!-- eslint-disable-next-line svelte/no-at-html-tags -- formatReply escapes all HTML first -->
                {@html formatReply(m.textKey ? t(m.textKey) : m.text)}
              </div>
              {#if m.text}
                <div class="flex items-center gap-1 pt-1">
                  <button
                    type="button"
                    class="icon-btn"
                    style="padding: 0.25rem;"
                    title={t("chat.replay")}
                    aria-label={t("chat.replay")}
                    onclick={() => void voice.speakText(m.text ?? "")}
                  >
                    <svg
                      viewBox="0 0 24 24"
                      class="h-4 w-4"
                      fill="none"
                      stroke="currentColor"
                      stroke-width="1.8"
                      stroke-linecap="round"
                      stroke-linejoin="round"
                      aria-hidden="true"
                    >
                      <path d="M11 5 6 9H3v6h3l5 4z" />
                      <path d="M15.5 8.5a5 5 0 0 1 0 7" />
                      <path d="M18.5 5.5a9 9 0 0 1 0 13" />
                    </svg>
                  </button>
                  <button
                    type="button"
                    class="icon-btn"
                    style="padding: 0.25rem;"
                    title={copiedKey === (m.id ?? `local-${i}`)
                      ? t("chat.copied")
                      : t("chat.copy")}
                    aria-label={t("chat.copy")}
                    onclick={() => void copyText(m.id ?? `local-${i}`, m.text)}
                  >
                    {#if copiedKey === (m.id ?? `local-${i}`)}
                      <svg
                        viewBox="0 0 24 24"
                        class="h-4 w-4"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="2"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                        aria-hidden="true"
                      >
                        <path d="M20 6 9 17l-5-5" />
                      </svg>
                    {:else}
                      <svg
                        viewBox="0 0 24 24"
                        class="h-4 w-4"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.8"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                        aria-hidden="true"
                      >
                        <rect x="9" y="9" width="12" height="12" rx="0" />
                        <path d="M5 15H4V3h12v1" />
                      </svg>
                    {/if}
                  </button>
                </div>
              {/if}
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
                          : 'var(--danger)'};"
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
      class="border p-1.5"
      style="border-color: var(--border-strong); background: var(--crust);"
    >
      <div class="flex items-center gap-2">
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
            <rect x="9" y="3" width="6" height="11" rx="0" />
            <path d="M5 11a7 7 0 0 0 14 0M12 18v3" />
          </svg>
        </button>
        <span
          class="mono font-bold"
          style="color: var(--accent);"
          aria-hidden="true">&gt;</span
        >
        <input
          class="mono min-w-0 flex-1 bg-transparent text-[13px] outline-none"
          style="color: var(--fg);"
          placeholder={t("chat.placeholder")}
          value={chat.draft}
          oninput={(e) => chat.setDraft(e.currentTarget.value)}
        />
        <button
          class="btn shrink-0 {chat.orb === 'thinking' ? '' : 'btn-primary'}"
          type={chat.orb === "thinking" ? "button" : "submit"}
          style={chat.orb === "thinking"
            ? "background: var(--danger); border-color: var(--danger); color: var(--crust);"
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
              <rect x="6" y="6" width="12" height="12" />
            </svg>
            {t("chat.stop").toUpperCase()}
          {:else}
            {t("chat.send").toUpperCase()}
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
          {/if}
        </button>
      </div>
      {#if voice.listening && !voice.capturing && voice.mode === "wake"}
        <div
          class="mono flex flex-wrap items-center gap-x-3 gap-y-1 px-1 pt-1.5 text-[10px] uppercase tracking-widest"
          style="color: var(--fg-faint);"
        >
          <span>{t("voice.sayHey", { word: voice.wakeWord })}</span>
        </div>
      {/if}
    </form>
    {#if voice.error}
      <p class="error-box">{voice.error}</p>
    {/if}
    {#if voice.phase === "starting"}
      <p class="faint mono text-[11px]">{t("voice.starting")}</p>
    {/if}
    {#if voice.notice}
      <p class="mono text-[11px] font-semibold" style="color: var(--accent);">
        {voice.notice}
      </p>
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
        ↻ {t("providers.refresh").toUpperCase()}
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
