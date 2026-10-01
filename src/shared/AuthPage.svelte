<script lang="ts">
  import { auth } from "../domains/auth/auth.store.svelte";
  import { t } from "../lib/i18n.svelte";
  import { navigate, syncAuthRoute } from "../app/router.svelte";
  import Logo from "./Logo.svelte";
  import LangTheme from "./LangTheme.svelte";

  let { mode }: { mode: "login" | "register" } = $props();

  let email = $state("");
  let password = $state("");
  let error = $state("");
  let busy = $state(false);
  // Supabase signup with email confirmation: the account exists but cannot
  // sign in until the link is clicked. Show that instead of the form.
  let confirmEmail = $state("");

  const isLogin = () => mode === "login";

  $effect(() => {
    syncAuthRoute(auth.user);
  });

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    if (busy) return;
    busy = true;
    error = "";
    try {
      if (isLogin()) {
        await auth.login(email, password);
      } else {
        const res = await auth.register(email, password);
        if (res.needsConfirmation) {
          confirmEmail = email;
          return;
        }
      }
      navigate("");
    } catch (err) {
      error = err instanceof Error ? err.message : "Error";
    } finally {
      busy = false;
    }
  }

  function switchMode() {
    error = "";
    confirmEmail = "";
    navigate(isLogin() ? "register" : "login");
  }
</script>

<div class="dot-bg grid min-h-screen place-items-center p-4">
  <div class="absolute right-4 top-4"><LangTheme /></div>
  <div class="card-xl msg-in w-full max-w-md" style="padding: 0;">
    <div class="panel-header">
      <h2 class="panel-title">Smart PC</h2>
      <span class="mono text-[10px]" style="color: var(--fg-faint);">AUTH</span>
    </div>
    <div style="padding: 1.5rem;">
      <div class="mb-6 flex items-center gap-3">
        <Logo />
        <div>
          <p class="text-lg font-bold">Smart PC</p>
          <p class="faint text-xs">{t("brand.tagline")}</p>
        </div>
      </div>
      <h1 class="mb-1 text-2xl font-bold">
        {#if confirmEmail}
          {t("auth.confirmTitle")}
        {:else}
          {isLogin() ? t("auth.loginTitle") : t("auth.registerTitle")}
        {/if}
      </h1>
      <p class="muted mb-5 text-sm">
        {#if confirmEmail}
          {t("auth.confirmBody", { email: confirmEmail })}
        {:else}
          {isLogin() ? t("auth.toRegister") : t("auth.toLogin")}
        {/if}
      </p>
      {#if confirmEmail}
        <button
          class="btn btn-primary w-full"
          style="padding: 0.8rem 1.25rem;"
          onclick={() => {
            confirmEmail = "";
            navigate("login");
          }}
        >
          {t("auth.toLogin")} →
        </button>
      {:else}
        {#if isLogin() && auth.logoutWarning}
          <p class="warn-box mb-3">{t("auth.logoutUnsynced")}</p>
        {/if}
        <form onsubmit={submit} class="flex flex-col gap-3">
          <div>
            <label class="label" for="auth-email">{t("auth.email")}</label>
            <input
              id="auth-email"
              class="field"
              style="padding: 0.75rem 1rem;"
              type="email"
              required
              autocomplete="email"
              bind:value={email}
            />
          </div>
          <div>
            <label class="label" for="auth-password">{t("auth.password")}</label
            >
            <input
              id="auth-password"
              class="field"
              style="padding: 0.75rem 1rem;"
              type="password"
              required
              minlength={isLogin() ? undefined : 8}
              autocomplete={isLogin() ? "current-password" : "new-password"}
              bind:value={password}
            />
          </div>
          {#if error}<p class="error-box">{error}</p>{/if}
          <button
            class="btn btn-primary mt-1"
            style="padding: 0.8rem 1.25rem;"
            type="submit"
            disabled={busy}
          >
            {busy
              ? t("common.loading")
              : isLogin()
                ? t("auth.loginBtn")
                : t("auth.registerBtn")}
          </button>
        </form>
      {/if}
      <div class="mt-5 flex items-center gap-3">
        <span class="h-px flex-1" style="background: var(--border);"></span>
        <span class="faint text-xs">{t("auth.or")}</span>
        <span class="h-px flex-1" style="background: var(--border);"></span>
      </div>
      <button
        class="mx-auto mt-3 block text-sm font-semibold transition hover:brightness-110"
        style="color: var(--accent);"
        onclick={switchMode}
      >
        {isLogin() ? t("auth.registerBtn") : t("auth.loginBtn")} →
      </button>
    </div>
  </div>
</div>
