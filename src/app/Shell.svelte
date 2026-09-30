<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { auth } from "../domains/auth/auth.store.svelte";
  import { providerStore as providers } from "../domains/assistant/providers.store.svelte";
  import { sessionStore as sessions } from "../domains/assistant/sessions.store.svelte";
  import { navigate, route, syncAuthRoute } from "./router.svelte";
  import TopBar from "./TopBar.svelte";
  import ResizableRow from "./shell/ResizableRow.svelte";
  import EventsFeed from "../domains/assistant/EventsFeed.svelte";
  import CenterPanel from "../domains/assistant/CenterPanel.svelte";
  import SessionsPane from "../domains/assistant/SessionsPane.svelte";
  import SettingsPage from "../domains/settings/SettingsPage.svelte";
  import ApiKeyModal from "../shared/ApiKeyModal.svelte";
  import { installVoiceHotkey } from "../domains/voice/voice.store.svelte";
  import { fetchHealth, SIDECAR_PROTOCOL } from "../lib/api";
  import { t } from "../lib/i18n.svelte";

  // Settings is a route (#/settings[/section]), not local state: deep-linkable,
  // browser back works, reload keeps the page.
  const view = () => (route.name === "settings" ? "settings" : "main");
  let protoOk = $state(true);

  onMount(() => {
    fetchHealth()
      .then((h) => {
        protoOk = h.protocol === SIDECAR_PROTOCOL;
      })
      .catch(() => {});
  });

  onMount(() => {
    syncAuthRoute(auth.user);
    if (auth.user) {
      void providers
        .loadProviders()
        .then(() => sessions.refreshSessions())
        .then(() => providers.startProviderWatch());
    }
  });

  onDestroy(() => providers.stopProviderWatch());

  // Global push-to-talk: the mic toggle shortcut works from anywhere.
  onMount(() => installVoiceHotkey());

  const engineLabel = () =>
    !protoOk
      ? "MISMATCH"
      : providers.providersLoading
        ? "PROBING"
        : providers.activeAvailable()
          ? "READY"
          : "NO ENGINE";
</script>

<div class="dot-bg flex min-h-dvh flex-col lg:h-dvh lg:overflow-hidden">
  <div class="flex min-h-dvh w-full flex-1 flex-col lg:h-full lg:min-h-0">
    {#if !protoOk}
      <div
        class="mono border-b p-2 text-center text-xs font-bold uppercase tracking-widest"
        style="border-color: var(--danger); color: var(--danger); background: color-mix(in srgb, var(--danger) 8%, var(--crust));"
      >
        {t("protocol.mismatch")}
      </div>
    {/if}
    <TopBar onSettings={() => navigate("settings")} />
    {#if view() === "main"}
      <ResizableRow>
        {#snippet left()}
          <EventsFeed />
        {/snippet}
        {#snippet center()}
          <CenterPanel />
        {/snippet}
        {#snippet right()}
          <SessionsPane />
        {/snippet}
      </ResizableRow>
    {:else}
      <div
        class="min-h-0 flex-1 lg:overflow-y-auto"
        style="background: var(--bg);"
      >
        <div class="mx-auto w-full max-w-4xl p-3">
          <SettingsPage
            initialSection={route.settingsSection}
            onBack={() => navigate("")}
          />
        </div>
      </div>
    {/if}
    <ApiKeyModal />
  </div>
  <!-- Persistent status line: only real state, never invented telemetry. -->
  <footer
    class="mono flex h-7 shrink-0 items-center justify-between gap-3 px-3 text-[11px]"
    style="background: var(--crust); border-top: 1px solid var(--border); color: var(--fg-muted);"
  >
    <span class="flex items-center gap-2">
      <span
        class="dot"
        style="background: {protoOk && providers.activeAvailable()
          ? 'var(--success)'
          : 'var(--warn)'};"
      ></span>
      <span class="hud-label" style="color: var(--fg);">
        {protoOk ? "SYSTEM ONLINE" : "PROTOCOL MISMATCH"}
      </span>
      <span class="faint hidden sm:inline">·</span>
      <span class="hidden sm:inline">ENGINE: {engineLabel()}</span>
    </span>
    <span class="flex items-center gap-2">
      <span class="hidden md:inline">
        {providers.activeProvider}{providers.activeModel
          ? ` / ${providers.activeModel}`
          : ""}
      </span>
      <span class="faint hidden md:inline">|</span>
      <span>
        {sessions.activeSessionId
          ? `SESSION ${sessions.activeSessionId.slice(0, 8).toUpperCase()}`
          : "NO SESSION"}
      </span>
    </span>
  </footer>
</div>
