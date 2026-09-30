<script lang="ts">
  import Logo from "../shared/Logo.svelte";
  import { getLang, setLang, t } from "../lib/i18n.svelte";
  import { getTheme, setTheme, type Theme } from "../lib/theme.svelte";
  import { auth } from "../domains/auth/auth.store.svelte";

  let { onSettings }: { onSettings: () => void } = $props();

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
    <div class="seg" data-menu>
      <span>LANG</span>
      <button
        type="button"
        aria-pressed={getLang() === "es"}
        onclick={() => setLang("es")}
      >
        ES
      </button>
      <button
        type="button"
        aria-pressed={getLang() === "en"}
        onclick={() => setLang("en")}
      >
        EN
      </button>
    </div>

    <div class="seg relative" data-menu>
      <span>{t("common.theme")}</span>
      {#each themes as v (v)}
        <button
          type="button"
          aria-pressed={getTheme() === v}
          onclick={() => setTheme(v)}
        >
          {themeLabel(v).toUpperCase()}
        </button>
      {/each}
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
