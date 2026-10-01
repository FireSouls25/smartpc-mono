<script lang="ts">
  import Logo from "../shared/Logo.svelte";
  import { getLang, setLang, t } from "../lib/i18n.svelte";
  import { getTheme, setTheme, type Theme } from "../lib/theme.svelte";
  import { auth } from "../domains/auth/auth.store.svelte";

  let { onSettings }: { onSettings: () => void } = $props();
  let open: "lang" | "theme" | null = $state(null);

  function toggle(which: "lang" | "theme"): void {
    open = open === which ? null : which;
  }

  const themes: Theme[] = ["auto", "light", "dark"];
  const themeLabel = (v: Theme): string =>
    v === "auto"
      ? t("common.auto")
      : v === "light"
        ? t("common.light")
        : t("common.dark");

  const initial = () =>
    (auth.user?.email ?? "").trim().slice(0, 1).toUpperCase() || "?";
</script>

<svelte:window
  onclick={(e) => {
    const el = e.target as HTMLElement | null;
    if (el?.closest?.("[data-menu]")) return;
    open = null;
  }}
  onkeydown={(e) => {
    if (e.key === "Escape") open = null;
  }}
/>

<header
  class="flex shrink-0 items-stretch justify-between gap-3"
  style="background: var(--surface); border-bottom: 1px solid var(--border); min-height: 3.5rem;"
>
  <div class="flex items-center gap-2.5 px-3">
    <Logo />
    <div>
      <p class="font-bold leading-tight">Smart PC</p>
      <p class="faint mono text-[10px] uppercase leading-tight tracking-widest">
        {t("brand.tagline")}
      </p>
    </div>
    <span
      class="mono ml-2 hidden px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-widest sm:inline-block"
      style="background: var(--surface-2); border: 1px solid var(--border); color: var(--accent);"
    >
      Desktop
    </span>
  </div>

  <div class="flex items-center gap-2 px-3">
    <div class="relative" data-menu>
      <button
        type="button"
        class="topdrop-trigger"
        aria-haspopup="listbox"
        aria-expanded={open === "lang"}
        aria-label="Language"
        onclick={() => toggle("lang")}
      >
        <svg
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="1.8"
          stroke-linecap="round"
          stroke-linejoin="round"
          aria-hidden="true"
        >
          <circle cx="12" cy="12" r="10" />
          <path
            d="M2 12h20M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"
          />
        </svg>
        <span>{getLang().toUpperCase()}</span>
        <svg
          class="chev"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2"
          stroke-linecap="round"
          stroke-linejoin="round"
          aria-hidden="true"
        >
          <path d="M6 9l6 6 6-6" />
        </svg>
      </button>
      {#if open === "lang"}
        <div
          class="dropdown dropdown-down"
          role="listbox"
          aria-label="Language"
        >
          {#each [["es", "Español"], ["en", "English"]] as [v, label] (v)}
            <button
              type="button"
              role="option"
              aria-selected={getLang() === v}
              aria-checked={getLang() === v}
              onclick={() => {
                setLang(v === "es" ? "es" : "en");
                open = null;
              }}
            >
              <span
                class="topdrop-marker"
                data-on={getLang() === v}
                aria-hidden="true"
              ></span>
              <span class="flex-1 text-left">{label}</span>
            </button>
          {/each}
        </div>
      {/if}
    </div>

    <div class="relative" data-menu>
      <button
        type="button"
        class="topdrop-trigger"
        aria-haspopup="listbox"
        aria-expanded={open === "theme"}
        aria-label={t("common.theme")}
        onclick={() => toggle("theme")}
      >
        {#if getTheme() === "light"}
          <svg
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="1.8"
            stroke-linecap="round"
            stroke-linejoin="round"
            aria-hidden="true"
          >
            <circle cx="12" cy="12" r="4" />
            <path
              d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"
            />
          </svg>
        {:else if getTheme() === "dark"}
          <svg
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="1.8"
            stroke-linecap="round"
            stroke-linejoin="round"
            aria-hidden="true"
          >
            <path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z" />
          </svg>
        {:else}
          <svg
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="1.8"
            stroke-linecap="round"
            stroke-linejoin="round"
            aria-hidden="true"
          >
            <rect x="2" y="4" width="20" height="13" rx="0" />
            <path d="M8 21h8M12 17v4" />
          </svg>
        {/if}
        <span>{themeLabel(getTheme()).toUpperCase()}</span>
        <svg
          class="chev"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2"
          stroke-linecap="round"
          stroke-linejoin="round"
          aria-hidden="true"
        >
          <path d="M6 9l6 6 6-6" />
        </svg>
      </button>
      {#if open === "theme"}
        <div
          class="dropdown dropdown-down"
          role="listbox"
          aria-label={t("common.theme")}
        >
          {#each themes as v (v)}
            <button
              type="button"
              role="option"
              aria-selected={getTheme() === v}
              aria-checked={getTheme() === v}
              onclick={() => {
                setTheme(v);
                open = null;
              }}
            >
              <span
                class="topdrop-marker"
                data-on={getTheme() === v}
                aria-hidden="true"
              ></span>
              {#if v === "light"}
                <svg
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="1.8"
                  stroke-linecap="round"
                  stroke-linejoin="round"
                  aria-hidden="true"
                >
                  <circle cx="12" cy="12" r="4" />
                  <path
                    d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"
                  />
                </svg>
              {:else if v === "dark"}
                <svg
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="1.8"
                  stroke-linecap="round"
                  stroke-linejoin="round"
                  aria-hidden="true"
                >
                  <path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z" />
                </svg>
              {:else}
                <svg
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="1.8"
                  stroke-linecap="round"
                  stroke-linejoin="round"
                  aria-hidden="true"
                >
                  <rect x="2" y="4" width="20" height="13" rx="0" />
                  <path d="M8 21h8M12 17v4" />
                </svg>
              {/if}
              <span class="flex-1 text-left">{themeLabel(v)}</span>
            </button>
          {/each}
        </div>
      {/if}
    </div>

    <button
      class="btn btn-ghost"
      style="padding: 0.375rem 0.75rem;"
      onclick={onSettings}
      aria-label={t("topbar.settings")}
    >
      {t("topbar.settings").toUpperCase()}
    </button>

    {#if auth.user}
      <span
        class="mono grid h-8 w-8 place-items-center text-sm font-bold"
        style="background: var(--accent); color: var(--on-accent);"
        title={auth.user.email}
        aria-label={auth.user.email}
      >
        {initial()}
      </span>
    {/if}
  </div>
</header>
