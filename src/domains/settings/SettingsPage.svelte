<script lang="ts">
  import BounceSidebar from "../../components/ui/bounce-sidebar.svelte";
  import { auth } from "../auth/auth.store.svelte";
  import { cloud } from "../auth/cloud.store.svelte";
  import { navigate } from "../../app/router.svelte";
  import {
    t,
    getLang,
    setLang,
    type Lang,
    type I18nKey,
  } from "../../lib/i18n.svelte";
  import { getTheme, setTheme, type Theme } from "../../lib/theme.svelte";
  import { providerStore as providers } from "../assistant/providers.store.svelte";
  import ProviderStart from "../assistant/ProviderStart.svelte";
  import { voice, DEFAULT_WAKE_WORD } from "../voice/voice.store.svelte";
  import {
    DEFAULT_HOTKEY,
    displayHotkey,
    formatHotkey,
    suspendHotkey,
    type VoiceSensitivity,
  } from "../voice/voice.store.svelte";
  import {
    voiceApi,
    type VoiceMode,
    type VoiceStatus,
  } from "../voice/voice.api";
  import SelectMenu from "../../shared/SelectMenu.svelte";
  import { timeOf } from "../../lib/format";
  let {
    onBack,
    initialSection = 0,
  }: { onBack: () => void; initialSection?: number } = $props();

  // Gestures were cut here on purpose (P2 #13): the toggle switched state
  // with no detection pipeline behind it. It returns with the pipeline.
  const sections = ["general", "ai", "voice", "account"] as const;
  const clamp = (n: number): number =>
    Math.min(sections.length - 1, Math.max(0, n));
  // Local state synced FROM the route (Back button, deep links); writes go
  // through selectSection which replaces the hash.
  let section = $state(0);

  // Two-way binding with the #/settings[/<n>] hash (replace, not push: one
  // history entry per settings visit, not per section click).
  $effect(() => {
    const fromRoute = clamp(initialSection);
    if (fromRoute !== section) section = fromRoute;
  });

  function selectSection(i: number): void {
    section = clamp(i);
    navigate(section === 0 ? "settings" : `settings/${section}`, {
      replace: true,
    });
  }

  const items = (): string[] =>
    sections.map((s) => t(`settings.${s}` as I18nKey));

  // Voice status is live (mic hotplug, session state): refresh on every
  // entry. Debug output lives in the terminal now (sidecar stderr), not here.
  // Cloud status changes from the outside (other devices sync), so it is
  // read fresh on every Account visit.
  $effect(() => {
    if (section === 2) {
      void loadVoiceStatus();
    }
    if (section === 3) {
      void cloud.load();
    }
  });

  // Voice status (mic + model readiness). Refreshed on every entry —
  // see the section effect above.
  let voiceStatus = $state<VoiceStatus | null>(null);

  async function loadVoiceStatus(): Promise<void> {
    try {
      voiceStatus = await voiceApi.status();
    } catch {
      voiceStatus = null;
    }
  }

  /**
   * Device options, deduped by value: ALSA reports one entry per
   * subdevice/direction, so raw names repeat ("sof-hda-dsp, " ×7) and would
   * crash the keyed each block (each_key_duplicate → empty list). The server
   * tries every same-name match on selection, so collapsing loses nothing.
   */
  function inputOptions(): { value: string; label: string }[] {
    const names = voiceStatus?.inputs ?? [];
    const out: { value: string; label: string }[] = [
      { value: "", label: t("voice.systemDefault") },
    ];
    for (const d of names) {
      if (out.some((o) => o.value === d)) continue;
      out.push({ value: d, label: d });
    }
    return out;
  }

  async function logout(): Promise<void> {
    await auth.logout();
    navigate("login");
  }

  // Push-to-talk capture: while true the next keydown becomes the hotkey
  // (Escape cancels). The global toggle is suspended meanwhile so the new
  // combo doesn't fire the mic mid-capture.
  let capturingHotkey = $state(false);

  function beginHotkeyCapture(): void {
    capturingHotkey = true;
    suspendHotkey(true);
  }

  function captureHotkey(e: KeyboardEvent): void {
    if (!capturingHotkey) return;
    e.preventDefault();
    e.stopPropagation();
    if (e.key === "Escape") {
      capturingHotkey = false;
      suspendHotkey(false);
      return;
    }
    const combo = formatHotkey(e);
    if (combo) {
      voice.setHotkey(combo);
      capturingHotkey = false;
      suspendHotkey(false);
    }
  }

  function sttOptions(): { value: string; label: string; hint?: string }[] {
    return [
      { value: "", label: t("voice.serverDefault") },
      { value: "tiny", label: "tiny", hint: "~75 MB" },
      { value: "tiny.en", label: "tiny.en", hint: "~75 MB" },
      { value: "base", label: "base", hint: "~142 MB" },
      { value: "base.en", label: "base.en", hint: "~142 MB" },
      { value: "small", label: "small", hint: "~466 MB" },
    ];
  }

  /** What will actually transcribe: the pick, or the server default. */
  function effectiveStt(): { name: string; ready: boolean | null } {
    if (voice.sttModel) {
      return {
        name: voice.sttModel,
        ready: voiceStatus?.models_ready?.[voice.sttModel] ?? null,
      };
    }
    return {
      name: voiceStatus?.model ?? "…",
      ready: voiceStatus?.model_ready ?? null,
    };
  }

  /** Installed-model rows (sizes mirror stt/model.rs specs). */
  function sttCatalog(): { name: string; sizeMb: number; ready: boolean }[] {
    const sizes: Record<string, number> = {
      tiny: 75,
      "tiny.en": 75,
      base: 142,
      "base.en": 142,
      small: 466,
    };
    return Object.keys(sizes).map((name) => ({
      name,
      sizeMb: sizes[name],
      ready: voiceStatus?.models_ready?.[name] ?? false,
    }));
  }

  let modelsError = $state("");

  async function deleteSttModel(name: string, sizeMb: number): Promise<void> {
    modelsError = "";
    const msg = t("voice.deleteAsk", { name, size: String(sizeMb) });
    if (!window.confirm(msg)) return;
    try {
      await voiceApi.deleteSttModel(name);
      await loadVoiceStatus();
    } catch (err) {
      modelsError = err instanceof Error ? err.message : "Error";
    }
  }

  async function remove(): Promise<void> {
    if (!window.confirm(t("auth.deleteAsk"))) return;
    await auth.deleteAccount();
    navigate("register");
  }
</script>

<svelte:window onkeydown={captureHotkey} />

<div class="flex flex-col gap-4">
  <div class="flex items-center gap-3">
    <button class="icon-btn" onclick={onBack} aria-label={t("settings.back")}>
      <svg
        viewBox="0 0 24 24"
        class="h-5 w-5"
        fill="none"
        stroke="currentColor"
        stroke-width="1.8"
        stroke-linecap="round"
        stroke-linejoin="round"
      >
        <path d="M19 12H5M12 19l-7-7 7-7" />
      </svg>
      <span class="tip">{t("settings.back")}</span>
    </button>
    <h1 class="text-xl font-bold">{t("settings.title")}</h1>
  </div>

  <div class="card-xl flex flex-col gap-6 md:flex-row">
    <div class="shrink-0 md:w-52">
      <BounceSidebar
        items={items()}
        value={section}
        onChange={selectSection}
        dotColor="#8839ef"
      />
    </div>

    <div class="min-w-0 flex-1">
      {#if section === 0}
        <div class="flex flex-col gap-4">
          <div class="group-card">
            <h3 class="group-title">{t("common.theme")}</h3>
            <div class="flex flex-wrap gap-2">
              {#each ["auto", "light", "dark"] as Theme[] as v (v)}
                <button
                  class="chip"
                  style={getTheme() === v
                    ? "border-color: var(--accent); color: var(--fg);"
                    : ""}
                  onclick={() => setTheme(v)}
                  aria-pressed={getTheme() === v}
                >
                  {v === "auto"
                    ? t("common.auto")
                    : v === "light"
                      ? t("common.light")
                      : t("common.dark")}
                </button>
              {/each}
            </div>
          </div>
          <div class="group-card">
            <h3 class="group-title">Language / Idioma</h3>
            <div class="flex flex-wrap gap-2">
              {#each ["es", "en"] as Lang[] as v (v)}
                <button
                  class="chip"
                  style={getLang() === v
                    ? "border-color: var(--accent); color: var(--fg);"
                    : ""}
                  onclick={() => setLang(v)}
                  aria-pressed={getLang() === v}
                >
                  {v === "es" ? "Español" : "English"}
                </button>
              {/each}
            </div>
          </div>
        </div>
      {:else if section === 1}
        <div class="flex flex-col gap-4">
          <p class="muted text-sm">{t("settings.providerNote")}</p>
          {#if providers.providersError}
            <p class="error-box">{providers.providersError}</p>
          {/if}
          <div class="group-card">
            <h3 class="group-title">{t("chat.provider")}</h3>
            <SelectMenu
              label={t("chat.provider")}
              value={providers.activeProvider}
              options={providers.providers.map((p) => ({
                value: p.id,
                label: p.id,
                disabled: !p.available,
                hint: p.available ? undefined : t("providers.offline"),
              }))}
              align="down"
              onChange={(v) => void providers.selectProvider(v)}
            />
          </div>
          <div class="group-card">
            <h3 class="group-title">{t("chat.model")}</h3>
            <SelectMenu
              label={t("chat.model")}
              value={providers.activeModel}
              options={providers
                .activeModels()
                .map((m) => ({ value: m, label: m }))}
              align="down"
              onChange={(v) => void providers.selectModel(v)}
            />
          </div>
          {#if providers.selectError}
            <p class="error-box">{providers.selectError}</p>
          {/if}
          <ProviderStart providerId={providers.activeProvider} />
          {#each providers.providers.filter((p) => p.needs_key) as p (p.id)}
            <div class="flex items-center gap-2">
              <span class="chip">
                <span
                  class="dot"
                  style="background: {providers.keyStatus[p.id]
                    ? 'var(--success)'
                    : 'var(--warn)'};"
                ></span>
                {p.id}
              </span>
              <button
                class="btn btn-ghost"
                style="padding: 0.375rem 0.75rem; font-size: 0.75rem;"
                onclick={() => providers.openKeyModal(p.id)}
              >
                {providers.keyStatus[p.id] ? t("aikey.change") : t("aikey.add")}
              </button>
            </div>
          {/each}
        </div>
      {:else if section === 2}
        <div class="flex flex-col gap-4">
          <p class="muted text-sm">{t("voice.localNote")}</p>
          <div class="group-card">
            <h3 class="group-title">{t("voice.input")}</h3>
            <SelectMenu
              label={t("voice.input")}
              value={voice.device ?? ""}
              options={inputOptions()}
              align="down"
              onChange={(v) => {
                voice.setDevice(v || null);
                void loadVoiceStatus();
              }}
            />
            {#if voiceStatus?.device}
              <p class="faint mt-1 text-xs">{voiceStatus.device}</p>
            {/if}
            <div class="flex flex-wrap gap-2">
              <span class="chip">
                <span
                  class="dot"
                  style="background: {voiceStatus?.mic
                    ? 'var(--success)'
                    : 'var(--warn)'};"
                ></span>
                {t("voice.mic")}: {voiceStatus
                  ? voiceStatus.mic
                    ? t("voice.ready")
                    : t("voice.notReady")
                  : "…"}
              </span>
              <span class="chip">
                <span
                  class="dot"
                  style="background: {effectiveStt().ready === false
                    ? 'var(--warn)'
                    : effectiveStt().ready
                      ? 'var(--success)'
                      : 'var(--border-strong)'};"
                ></span>
                {t("voice.model")}: {effectiveStt().name}
              </span>
              {#if voiceStatus?.mixer}
                <span class="chip">
                  <span
                    class="dot"
                    style="background: {voiceStatus.mixer.muted ||
                    voiceStatus.mixer.volume < 0.5
                      ? 'var(--warn)'
                      : 'var(--success)'};"
                  ></span>
                  {t("voice.osGain")}: {voiceStatus.mixer.muted
                    ? t("voice.muted")
                    : `${Math.round(voiceStatus.mixer.volume * 100)} %`}
                </span>
              {/if}
            </div>
          </div>
          <div class="group-card">
            <h3 class="group-title">{t("voice.sttModel")}</h3>
            <SelectMenu
              label={t("voice.sttModel")}
              value={voice.sttModel}
              options={sttOptions()}
              align="down"
              onChange={(v) => {
                voice.setSttModel(v);
                void loadVoiceStatus();
              }}
            />
            <p class="faint mt-1 text-xs">{t("voice.sttHint")}</p>
          </div>
          <div class="group-card">
            <h3 class="group-title">{t("voice.models")}</h3>
            <div class="flex flex-col gap-2">
              {#each sttCatalog() as m (m.name)}
                <div class="flex items-center gap-2">
                  <span class="chip">
                    <span
                      class="dot"
                      style="background: {m.ready
                        ? 'var(--success)'
                        : 'var(--border-strong)'};"
                    ></span>
                    {m.name} · ~{m.sizeMb} MB
                  </span>
                  <span class="faint text-xs">
                    {m.ready ? t("voice.downloaded") : t("voice.notDownloaded")}
                  </span>
                  {#if m.ready}
                    <button
                      class="btn btn-ghost"
                      style="padding: 0.375rem 0.75rem; font-size: 0.75rem;"
                      onclick={() => void deleteSttModel(m.name, m.sizeMb)}
                      aria-label={`${t("voice.deleteModel")}: ${m.name}`}
                    >
                      {t("voice.deleteModel")}
                    </button>
                  {/if}
                </div>
              {/each}
            </div>
            {#if modelsError}
              <p class="error-box mt-2">{modelsError}</p>
            {/if}
          </div>
          <div class="group-card">
            <h3 class="group-title">{t("voice.sensitivity")}</h3>
            <div class="flex flex-wrap gap-2">
              {#each ["low", "medium", "high"] as VoiceSensitivity[] as v (v)}
                <button
                  class="chip"
                  style={voice.sensitivity === v
                    ? "border-color: var(--accent); color: var(--fg);"
                    : ""}
                  onclick={() => voice.setSensitivity(v)}
                  aria-pressed={voice.sensitivity === v}
                >
                  {v === "low"
                    ? t("voice.sensLow")
                    : v === "medium"
                      ? t("voice.sensMedium")
                      : t("voice.sensHigh")}
                </button>
              {/each}
            </div>
            <p class="faint mt-1 text-xs">{t("voice.sensHint")}</p>
          </div>
          <div class="group-card">
            <h3 class="group-title">{t("voice.hotkey")}</h3>
            <div class="flex flex-wrap items-center gap-2">
              <button
                class="chip"
                style={capturingHotkey
                  ? "border-color: var(--accent); color: var(--fg);"
                  : ""}
                onclick={beginHotkeyCapture}
              >
                {capturingHotkey
                  ? t("voice.pressKeys")
                  : displayHotkey(voice.hotkey)}
              </button>
              {#if voice.hotkey !== DEFAULT_HOTKEY}
                <button
                  class="btn btn-ghost"
                  style="padding: 0.375rem 0.75rem; font-size: 0.75rem;"
                  onclick={() => voice.setHotkey(DEFAULT_HOTKEY)}
                >
                  {t("voice.hotkeyReset")}
                </button>
              {/if}
            </div>
            <p class="faint mt-1 text-xs">{t("voice.hotkeyHint")}</p>
          </div>
          <div class="group-card">
            <h3 class="group-title">{t("voice.mode")}</h3>
            <div class="flex flex-wrap gap-2">
              {#each ["manual", "wake", "conversation"] as VoiceMode[] as v (v)}
                <button
                  class="chip"
                  style={voice.mode === v
                    ? "border-color: var(--accent); color: var(--fg);"
                    : ""}
                  onclick={() => voice.setMode(v)}
                  aria-pressed={voice.mode === v}
                >
                  {v === "manual"
                    ? t("voice.modeManual")
                    : v === "wake"
                      ? t("voice.modeWake")
                      : t("voice.modeConvo")}
                </button>
              {/each}
            </div>
          </div>
          <div class="group-card">
            <label class="group-title" for="voice-wake"
              >{t("voice.wakeWord")}</label
            >
            <input
              id="voice-wake"
              class="field"
              style="max-width: 16rem;"
              type="text"
              maxlength={32}
              autocomplete="off"
              placeholder={DEFAULT_WAKE_WORD}
              value={voice.wakeWord}
              oninput={(e) => voice.setWakeWord(e.currentTarget.value)}
            />
            <p class="faint mt-1 text-xs">{t("voice.wakeHint")}</p>
          </div>
          <div class="group-card">
            <div class="flex items-center justify-between gap-4">
              <div>
                <p class="text-sm font-bold">{t("voice.speak")}</p>
                <p class="muted mt-0.5 text-xs">{t("voice.speakHint")}</p>
              </div>
              <button
                class="switch"
                role="switch"
                aria-checked={voice.speakEnabled}
                aria-label={t("voice.speak")}
                onclick={() => voice.setSpeakEnabled(!voice.speakEnabled)}
              ></button>
            </div>
          </div>
        </div>
      {:else}
        <div class="flex flex-col gap-4">
          <div class="group-card">
            <h3 class="group-title">{t("auth.email")}</h3>
            <p class="text-sm font-semibold">{auth.user?.email}</p>
            <p class="faint mt-0.5 text-xs">{t("settings.accountNote")}</p>
          </div>
          {#if auth.logoutWarning}
            <p class="warn-box">{t("auth.logoutUnsynced")}</p>
          {/if}
          {#if cloud.status}
            <div class="group-card">
              <div class="flex flex-wrap items-start justify-between gap-4">
                <div>
                  <h3 class="group-title">{t("cloud.title")}</h3>
                  <p class="mt-0.5 text-sm font-semibold">
                    {cloud.status.enabled ? t("cloud.on") : t("cloud.off")}
                  </p>
                  <p class="faint mt-0.5 text-xs">
                    {cloud.status.enabled
                      ? t("cloud.onHint")
                      : t("cloud.offHint")}
                  </p>
                  {#if cloud.status.enabled && cloud.status.url}
                    <p class="faint mt-0.5 text-xs">{cloud.status.url}</p>
                  {/if}
                </div>
                {#if cloud.status.enabled}
                  <button
                    class="btn btn-ghost"
                    disabled={cloud.syncing}
                    onclick={() => void cloud.syncNow()}
                  >
                    {cloud.syncing ? t("cloud.syncing") : t("cloud.syncNow")}
                  </button>
                {/if}
              </div>
              {#if cloud.status.enabled}
                <p class="faint mt-3 text-xs">
                  {#if cloud.lastError}
                    {t("cloud.error", { detail: cloud.lastError })}
                  {:else if cloud.status.last_pull_at || cloud.status.last_push_at}
                    {t("cloud.lastAt", {
                      when: timeOf(
                        cloud.status.last_pull_at ??
                          cloud.status.last_push_at ??
                          "",
                      ),
                    })}
                    · {t("cloud.rows", {
                      pulled: String(cloud.status.pulled),
                      pushed: String(cloud.status.pushed),
                    })}
                  {:else}
                    {t("cloud.never")}
                  {/if}
                </p>
              {/if}
            </div>
          {/if}
          <div class="group-card">
            <div class="flex flex-wrap gap-2">
              <button class="btn btn-ghost" onclick={logout}>
                {t("auth.logout")}
              </button>
              <button class="btn btn-danger" onclick={remove}>
                {t("auth.delete")}
              </button>
            </div>
          </div>
        </div>
      {/if}
    </div>
  </div>
</div>
