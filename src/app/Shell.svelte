<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { auth } from "../domains/auth/auth.store.svelte";
  import { providerStore as providers } from "../domains/assistant/providers.store.svelte";
  import { sessionStore as sessions } from "../domains/assistant/sessions.store.svelte";
  import { navigate, route, syncAuthRoute } from "./router.svelte";
  import TopBar from "./TopBar.svelte";
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

  /* Resizable panes (lg+): side widths persist across restarts; the center
   * always takes the remainder (flex-1, never hardcoded). Below lg the
   * panes stack and dividers hide.
   */
  const PANES_KEY = "smartpc.panes";
  const MIN_A = 200;
  const MIN_C = 220;
  const DEFAULT_A = 280;
  const DEFAULT_C = 300;

  function loadPanes(): { a: number; c: number } {
    try {
      const raw = window.localStorage.getItem(PANES_KEY);
      if (raw) {
        const p = JSON.parse(raw) as { a?: unknown; c?: unknown };
        const a = typeof p.a === "number" && p.a >= MIN_A ? p.a : DEFAULT_A;
        const c = typeof p.c === "number" && p.c >= MIN_C ? p.c : DEFAULT_C;
        return { a, c };
      }
    } catch {
      /* private mode */
    }
    return { a: DEFAULT_A, c: DEFAULT_C };
  }

  let paneA = $state(loadPanes().a);
  let paneC = $state(loadPanes().c);

  function savePanes(): void {
    try {
      window.localStorage.setItem(
        PANES_KEY,
        JSON.stringify({ a: paneA, c: paneC }),
      );
    } catch {
      /* private mode */
    }
  }

  let dragSide = $state<"a" | "c" | null>(null);

  function dividerDown(side: "a" | "c", e: PointerEvent): void {
    dragSide = side;
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
  }

  function dividerMove(e: PointerEvent, container: HTMLElement | null): void {
    if (!dragSide || !container) return;
    const rect = container.getBoundingClientRect();
    if (dragSide === "a") {
      paneA = Math.min(
        Math.max(MIN_A, e.clientX - rect.left),
        rect.width - 320 - MIN_C,
      );
    } else {
      paneC = Math.min(
        Math.max(MIN_C, rect.right - e.clientX),
        rect.width - 320 - MIN_A,
      );
    }
  }

  function dividerUp(): void {
    if (dragSide) {
      dragSide = null;
      savePanes();
    }
  }

  let panesEl: HTMLElement | null = $state(null);
</script>

<div class="dot-bg min-h-dvh lg:h-dvh lg:overflow-hidden">
  <div
    class="mx-auto flex min-h-dvh max-w-7xl flex-col px-4 md:px-6 lg:h-full lg:min-h-0"
  >
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
      <div
        bind:this={panesEl}
        class="flex flex-1 flex-col gap-4 pb-4 md:pb-8 lg:min-h-0 lg:flex-row lg:gap-0 lg:pb-8"
        role="group"
        aria-label="Panels"
        onpointermove={(e) => dividerMove(e, panesEl)}
        onpointerup={dividerUp}
        onpointercancel={dividerUp}
      >
        <div
          class="pane-a flex min-h-0 flex-col lg:shrink-0"
          style:flex-basis={`${paneA}px`}
        >
          <EventsFeed />
        </div>
        <div
          class="divider hidden w-3 shrink-0 cursor-col-resize touch-none items-stretch justify-center lg:flex"
          role="separator"
          aria-orientation="vertical"
          aria-label="Resize panels"
          onpointerdown={(e) => dividerDown("a", e)}
        >
          <span class="w-1 rounded-full"></span>
        </div>
        <div class="pane-b flex min-h-0 min-w-0 flex-1 flex-col">
          <CenterPanel />
        </div>
        <div
          class="divider hidden w-3 shrink-0 cursor-col-resize touch-none items-stretch justify-center xl:flex"
          role="separator"
          aria-orientation="vertical"
          aria-label="Resize panels"
          onpointerdown={(e) => dividerDown("c", e)}
        >
          <span class="w-1 rounded-full"></span>
        </div>
        <div
          class="pane-c hidden min-h-0 flex-col lg:shrink-0 xl:flex"
          style:flex-basis={`${paneC}px`}
        >
          <SessionsPane />
        </div>
      </div>
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
