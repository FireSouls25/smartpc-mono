<script lang="ts">
  /*
   * Themed dropdown menu. Opens upward by default (chat blocks sit low on
   * the screen); set align="down" for top-anchored contexts.
   */
  import { t } from "../lib/i18n.svelte";

  export interface MenuOption {
    value: string;
    label: string;
    disabled?: boolean;
    hint?: string;
  }

  let {
    options,
    value,
    onChange,
    label,
    align = "up",
  }: {
    options: MenuOption[];
    value: string;
    onChange: (v: string) => void;
    label: string;
    align?: "up" | "down";
  } = $props();

  let open = $state(false);
  let query = $state("");
  let searchEl: HTMLInputElement | null = $state(null);
  const current = () => options.find((o) => o.value === value);

  // Focus the filter whenever the menu opens (replaces autofocus).
  $effect(() => {
    if (open) searchEl?.focus();
  });

  const filtered = () => {
    const q = query.trim().toLowerCase();
    if (!q) return options;
    return options.filter((o) => o.label.toLowerCase().includes(q));
  };

  function toggle(): void {
    // Fresh filter on every open; stale queries hide the current value.
    if (!open) query = "";
    open = !open;
  }

  function pick(v: string, disabled?: boolean): void {
    if (disabled) return;
    open = false;
    query = "";
    onChange(v);
  }

  function onSearchKey(e: KeyboardEvent): void {
    if (e.key !== "Enter") return;
    const first = filtered().find((o) => !o.disabled);
    if (first) {
      e.preventDefault();
      pick(first.value, false);
    }
  }
</script>

<svelte:window
  onclick={(e) => {
    const el = e.target as HTMLElement | null;
    if (el?.closest?.("[data-selectmenu]")) return;
    open = false;
  }}
  onkeydown={(e) => {
    if (e.key === "Escape") open = false;
  }}
/>

<div class="relative" data-selectmenu>
  <button
    type="button"
    class="chip"
    style={open ? "border-color: var(--accent);" : ""}
    onclick={() => toggle()}
    aria-haspopup="listbox"
    aria-expanded={open}
    aria-label={label}
  >
    <span class="faint">{label}</span>
    <span class="font-semibold" style="color: var(--fg);">
      {current()?.label ?? value}
    </span>
    <svg
      viewBox="0 0 24 24"
      class="h-3.5 w-3.5 transition-transform {open ? 'rotate-180' : ''}"
      fill="none"
      stroke="currentColor"
      stroke-width="2"
      stroke-linecap="round"
      stroke-linejoin="round"
    >
      <path d="M6 9l6 6 6-6" />
    </svg>
  </button>

  {#if open}
    <div
      role="listbox"
      aria-label={label}
      class="dropdown msg-in {align === 'up' ? 'dropdown-up' : 'dropdown-down'}"
    >
      <div class="dropdown-search">
        <svg
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="1.8"
          stroke-linecap="round"
          stroke-linejoin="round"
          aria-hidden="true"
        >
          <circle cx="11" cy="11" r="7" />
          <path d="M21 21l-4.3-4.3" />
        </svg>
        <input
          type="text"
          placeholder={t("common.search")}
          aria-label={t("common.search")}
          bind:value={query}
          bind:this={searchEl}
          onkeydown={onSearchKey}
        />
      </div>
      {#each filtered() as o (o.value)}
        <button
          type="button"
          role="option"
          aria-selected={o.value === value}
          disabled={o.disabled}
          onclick={() => pick(o.value, o.disabled)}
        >
          <span
            class="dot"
            style="background: {o.value === value
              ? 'var(--accent)'
              : 'var(--border-strong)'};"
          ></span>
          <span class="flex-1 text-left">{o.label}</span>
          {#if o.hint}
            <span class="faint text-[11px]">{o.hint}</span>
          {/if}
        </button>
      {:else}
        <p class="faint px-2.5 py-2 text-xs">{t("common.noResults")}</p>
      {/each}
    </div>
  {/if}
</div>
