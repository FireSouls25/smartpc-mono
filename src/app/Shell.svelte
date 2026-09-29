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
</script>

<div class="dot-bg min-h-dvh lg:h-dvh lg:overflow-hidden">
  <div class="flex min-h-dvh w-full flex-col px-4 md:px-6 lg:h-full lg:min-h-0">
    {#if !protoOk}
      <div
        class="mb-3 rounded-2xl border p-3 text-center text-sm font-semibold"
        style="border-color: var(--danger); color: var(--danger); background: color-mix(in srgb, var(--danger) 8%, transparent);"
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
      <div class="min-h-0 flex-1 pb-8 lg:overflow-y-auto">
        <SettingsPage
          initialSection={route.settingsSection}
          onBack={() => navigate("")}
        />
      </div>
    {/if}
    <ApiKeyModal />
  </div>
</div>
