<script lang="ts">
  import { chatStore as chat } from "./chat.store.svelte";
  import { sessionStore as sessions } from "./sessions.store.svelte";
  import { timeOf } from "../../lib/format";
  import { t } from "../../lib/i18n.svelte";

  let query = $state("");

  const visible = () => {
    const q = query.trim().toLowerCase();
    if (!q) return sessions.list;
    return sessions.list.filter(
      (s) =>
        s.title.toLowerCase().includes(q) ||
        (s.preview ?? "").toLowerCase().includes(q) ||
        s.provider.toLowerCase().includes(q),
    );
  };
</script>

<aside class="card-xl flex h-full min-h-[240px] min-h-0 flex-col">
  <div class="panel-header">
    <h2 class="panel-title">{t("sessions.title")}</h2>
    <span class="flex items-center gap-2">
      <span class="mono text-[11px]" style="color: var(--fg-faint);">
        {sessions.list.length}
      </span>
      <button
        class="mono px-1.5 py-0.5 text-[11px] font-bold"
        style="background: var(--surface-2); border: 1px solid var(--border); color: var(--fg);"
        onclick={() => chat.newChat()}
        aria-label={t("sessions.new")}
        title={t("sessions.new")}
      >
        +
      </button>
    </span>
  </div>
  <div
    class="border-b px-2 py-1.5"
    style="border-color: var(--border); background: var(--crust);"
  >
    <input
      class="field mono"
      style="padding: 0.375rem 0.5rem; font-size: 11px;"
      type="search"
      placeholder="Filter…"
      aria-label="Filter sessions"
      bind:value={query}
    />
  </div>

  <div class="panel-body min-h-0 flex-1">
    {#if visible().length === 0}
      <p class="faint px-1 text-xs leading-relaxed">{t("sessions.empty")}</p>
    {/if}

    <ul class="flex min-h-0 flex-1 flex-col gap-1.5 overflow-y-auto">
      {#each visible() as s (s.id)}
        {@const active = sessions.activeSessionId === s.id}
        <li
          class="flex items-stretch gap-1 border transition"
          style={active
            ? "border-color: var(--accent); background: var(--surface-2);"
            : "border-color: var(--border); background: var(--surface);"}
        >
          <button
            class="min-w-0 flex-1 p-2 text-left"
            onclick={() => void chat.openSession(s.id)}
          >
            {#if active}
              <p
                class="mono mb-1 flex items-center gap-1 text-[10px] font-bold uppercase tracking-widest"
                style="color: var(--accent);"
              >
                <span class="dot" style="background: var(--accent);"></span>
                ACTIVE SESSION
              </p>
            {/if}
            <p class="truncate text-[13px] font-semibold leading-snug">
              {s.title}
            </p>
            {#if s.preview}
              <p class="faint mono mt-0.5 truncate text-[11px]">{s.preview}</p>
            {/if}
            <p
              class="mono mt-1.5 flex flex-wrap gap-x-2 gap-y-0.5 border-t pt-1 text-[10px] uppercase tracking-wider"
              style="border-color: var(--border); color: var(--fg-muted);"
            >
              <span>{timeOf(s.updated_at)}</span>
              <span>MSGS: {s.message_count}</span>
              <span>{s.provider}{s.model ? ` / ${s.model}` : ""}</span>
            </p>
          </button>
          <button
            class="faint self-start p-2 transition hover:text-[var(--danger)]"
            title={t("sessions.delete")}
            aria-label={t("sessions.delete")}
            onclick={() => void chat.deleteSession(s.id)}
          >
            <svg
              viewBox="0 0 24 24"
              class="h-4 w-4"
              fill="none"
              stroke="currentColor"
              stroke-width="1.8"
              stroke-linecap="round"
              stroke-linejoin="round"
            >
              <path
                d="M4 7h16M9 7V5a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2M6 7l1 13h10l1-13"
              />
            </svg>
          </button>
        </li>
      {/each}
    </ul>
  </div>
</aside>
