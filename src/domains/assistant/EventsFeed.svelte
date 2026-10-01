<script lang="ts">
  import { chatStore as chat } from "./chat.store.svelte";
  import { providerStore as providers } from "./providers.store.svelte";
  import { contextPct, fmtK } from "../../lib/format";
  import { t } from "../../lib/i18n.svelte";

  type Filter = "all" | "running" | "done" | "failed";
  let filter = $state<Filter>("all");

  const counts = () => ({
    all: chat.events.length,
    running: chat.events.filter((e) => e.status === "running").length,
    done: chat.events.filter((e) => e.status === "done").length,
    failed: chat.events.filter((e) => e.status === "failed").length,
  });

  const tabs = (): { key: Filter; label: string; count: number }[] => {
    const c = counts();
    return [
      { key: "all", label: t("events.all"), count: c.all },
      { key: "running", label: t("events.running"), count: c.running },
      { key: "done", label: t("events.done"), count: c.done },
      { key: "failed", label: t("events.failed"), count: c.failed },
    ];
  };

  const visible = () =>
    filter === "all"
      ? chat.events
      : chat.events.filter((e) => e.status === filter);

  function color(status: string): string {
    if (status === "running") return "var(--peach)";
    if (status === "failed") return "var(--danger)";
    return "var(--success)";
  }

  // Single localized status text: the pill below is the only place it
  // appears, so it never repeats (no DONE + "Completada" duplication).
  function statusText(status: string): string {
    if (status === "running") return t("events.running");
    if (status === "failed") return t("events.failed");
    return t("events.done");
  }

  function ctxPct(): number {
    return contextPct(chat.contextUsed, providers.contextWindow);
  }

  function ctxColor(): string {
    const p = ctxPct();
    if (p >= 90) return "var(--danger)";
    if (p >= 70) return "var(--warn)";
    return "var(--accent)";
  }
</script>

<aside class="card-xl flex h-full min-h-[240px] min-h-0 flex-col">
  <div class="panel-header">
    <h2 class="panel-title">{t("events.title")}</h2>
    <span
      class="mono flex items-center gap-1.5 text-[11px]"
      style="color: var(--fg-faint);"
    >
      <span class="dot" style="background: var(--accent);"></span>
      {counts().all}
    </span>
  </div>
  <!-- Filter sub-tabs: client-side view over the real event list. -->
  <div
    class="mono flex items-stretch overflow-x-auto text-[10px]"
    style="background: var(--crust); border-bottom: 1px solid var(--border);"
    role="tablist"
    aria-label={t("events.title")}
  >
    {#each tabs() as tab (tab.key)}
      <button
        type="button"
        role="tab"
        aria-selected={filter === tab.key}
        class="flex-1 whitespace-nowrap px-1 py-1.5 text-center uppercase tracking-wider transition-colors"
        style={filter === tab.key
          ? "background: var(--bg); color: var(--fg); font-weight: 700;"
          : "color: var(--fg-muted);"}
        onclick={() => (filter = tab.key)}
      >
        {tab.label} ({tab.count})
      </button>
    {/each}
  </div>
  <div class="panel-body min-h-0 flex-1">
    <div
      class="shrink-0 border px-2 py-1.5"
      style="border-color: var(--border); background: var(--crust);"
    >
      <div class="flex items-center justify-between gap-2">
        <p class="hud-label">{t("context.label")}</p>
        <p class="mono text-[11px]">
          {fmtK(chat.contextUsed)} / {providers.contextWindow == null
            ? "—"
            : fmtK(providers.contextWindow)}
        </p>
      </div>
      {#if providers.contextWindow != null}
        <div
          class="mt-1.5 h-1 overflow-hidden"
          style="background: var(--border);"
        >
          <div
            class="h-full transition-all"
            style="width: {ctxPct()}%; background: {ctxColor()};"
          ></div>
        </div>
      {/if}
    </div>
    {#if visible().length === 0}
      <p class="faint px-1 text-xs leading-relaxed">{t("events.empty")}</p>
    {/if}
    <ul class="flex min-h-0 flex-1 flex-col gap-1.5 overflow-y-auto">
      {#each visible() as ev (ev.id)}
        <li
          class="border p-2 transition"
          style="border-color: var(--border); background: var(--surface);"
        >
          <div class="flex items-center justify-between gap-2">
            <span
              class="mono inline-flex items-center gap-1.5 px-1.5 py-0.5 text-[11px] font-bold uppercase"
              style="background: var(--crust); color: {color(ev.status)};"
            >
              <span
                class="dot {ev.status === 'running' ? 'animate-pulse' : ''}"
                style="background: {color(ev.status)};"
              ></span>
              {statusText(ev.status)}
            </span>
            {#if ev.continuous}
              <span
                class="mono animate-pulse text-[10px] uppercase"
                style="color: var(--peach);"
              >
                {t("events.active")}
              </span>
            {/if}
          </div>
          <p
            class="mono mt-1 text-[12px] font-medium leading-tight"
            style="color: var(--fg);"
          >
            {ev.title}
          </p>
        </li>
      {/each}
    </ul>
  </div>
</aside>
