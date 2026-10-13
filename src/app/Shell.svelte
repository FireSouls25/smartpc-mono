<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { auth } from "../domains/auth/auth.store.svelte";
  import { providerStore as providers } from "../domains/assistant/providers.store.svelte";
  import { sessionStore as sessions } from "../domains/assistant/sessions.store.svelte";
  import { navigate, route, syncAuthRoute } from "./router.svelte";
  import TopBar from "./TopBar.svelte";
  import EventsFeed from "../domains/assistant/EventsFeed.svelte";
  import CenterPanel from "../domains/assistant/CenterPanel.svelte";
  import SettingsPage from "../domains/settings/SettingsPage.svelte";
  import ApiKeyModal from "../shared/ApiKeyModal.svelte";
  import { installVoiceHotkey } from "../domains/voice/voice.store.svelte";
  import { fetchHealth, SIDECAR_PROTOCOL } from "../lib/api";
  import { t } from "../lib/i18n.svelte";
  import { getTheme } from "../lib/theme.svelte";
  import { getLang } from "../lib/i18n.svelte";
  import { voice, setVoiceUser } from "../domains/voice/voice.store.svelte";
  import { pullApplyPrefs, pushPrefsIfChanged } from "../lib/prefs";

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
    // First load rides the token effect below (it fires after the async
    // session restore too — this mount runs before restore completes).
  });

  onDestroy(() => providers.stopProviderWatch());

  // Cross-machine prefs: adopt the cloud set once per login (register,
  // login, session resume), then mirror local edits back up. Echo-safe:
  // pull snapshots after applying, push skips unchanged signatures.
  let authedOnce = false;
  $effect(() => {
    if (auth.token && !authedOnce) {
      authedOnce = true;
      // New sign-in on a mounted shell (login/register/switch): reload
      // everything user-scoped — otherwise the previous account's (or
      // empty) provider, sessions and voice leak into this one.
      void providers
        .loadProviders()
        .then(() => sessions.refreshSessions())
        .then(() => providers.startProviderWatch());
      void pullApplyPrefs();
    } else if (!auth.token) {
      authedOnce = false;
      providers.stopProviderWatch();
      // Signed out: voice prefs fall back to the device key.
      setVoiceUser(null);
    }
  });
  $effect(() => {
    void getTheme();
    void getLang();
    void voice.mode;
    void voice.wakeWord;
    void voice.ttsVoice;
    if (auth.token) void pushPrefsIfChanged();
  });

  // Global push-to-talk: the mic toggle shortcut works from anywhere.
  onMount(() => installVoiceHotkey());
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
      <!-- §14.1: main screen = chat + actions. History lives in
        Settings → History (SessionsPane reused there, not here). -->
      <div class="resizable-row" role="group" aria-label="Panels">
        <section
          class="pane pane-side"
          style="--pw: 320px;"
          data-testid="pane-a"
          aria-label="Activity"
        >
          <EventsFeed />
        </section>
        <section
          class="pane pane-center"
          data-testid="pane-b"
          aria-label="Chat"
        >
          <CenterPanel />
        </section>
      </div>
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
</div>
