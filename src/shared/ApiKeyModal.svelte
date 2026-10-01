<script lang="ts">
  import { providerStore as providers } from "../domains/assistant/providers.store.svelte";
  import { t } from "../lib/i18n.svelte";

  let key = $state("");

  const modal = () => providers.keyModal;

  function close() {
    key = "";
    providers.closeKeyModal();
  }

  async function save(e: SubmitEvent) {
    e.preventDefault();
    const m = modal();
    if (!m || providers.keyBusy) return;
    await providers.saveKey(m.provider, key);
    key = "";
  }

  function onkeydown(e: KeyboardEvent) {
    if (e.key === "Escape") close();
  }
</script>

<svelte:window {onkeydown} />

{#if modal()}
  {@const m = modal()!}
  <div
    class="fixed inset-0 z-50 grid place-items-center p-4"
    style="background: rgb(17 17 27 / 0.7);"
    onclick={(e) => {
      if (e.target === e.currentTarget) close();
    }}
    role="presentation"
  >
    <div
      class="card-xl msg-in w-full max-w-sm"
      style="border: 1px solid var(--accent);"
      role="dialog"
      aria-modal="true"
      aria-label={t("aikey.title")}
    >
      <div class="panel-header">
        <h2 class="panel-title">{t("aikey.title")}</h2>
      </div>
      <div class="panel-body">
        <p class="muted mb-4 text-sm">{t("aikey.desc")}</p>
        {#if providers.keyNotice}
          <p class="warn-box">{providers.keyNotice}</p>
          <div class="mt-3 flex justify-end gap-2">
            <button
              type="button"
              class="btn btn-primary"
              disabled={providers.keyBusy}
              onclick={close}
            >
              {t("common.done")}
            </button>
          </div>
        {:else}
          <form onsubmit={save} class="flex flex-col gap-3">
            <div>
              <label class="label" for="aikey-input">{m?.provider}</label>
              <input
                id="aikey-input"
                class="field"
                type="password"
                required
                minlength={8}
                autocomplete="new-password"
                placeholder={t("aikey.placeholder")}
                bind:value={key}
              />
            </div>
            {#if providers.keyError}
              <p class="error-box">{providers.keyError}</p>
            {/if}
            <div class="flex justify-end gap-2">
              {#if m?.hasKey}
                <button
                  type="button"
                  class="btn btn-danger mr-auto"
                  disabled={providers.keyBusy}
                  onclick={() => void providers.deleteKey(m.provider)}
                >
                  {t("aikey.remove")}
                </button>
              {/if}
              <button
                type="button"
                class="btn btn-ghost"
                disabled={providers.keyBusy}
                onclick={close}
              >
                {t("aikey.cancel")}
              </button>
              <button
                type="submit"
                class="btn btn-primary"
                disabled={providers.keyBusy}
              >
                {providers.keyBusy ? t("common.loading") : t("aikey.save")}
              </button>
            </div>
          </form>
        {/if}
      </div>
    </div>
  </div>
{/if}
