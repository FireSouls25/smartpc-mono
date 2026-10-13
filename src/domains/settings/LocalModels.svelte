<script lang="ts">
  /*
   * Local models view: curated GGUFs served by our own llama-server.
   * Download (resumable, hashed) → use (boots the server + selects).
   * Nothing here touches a user-owned Ollama/llama-server: ours binds an
   * OS-picked loopback port, and external mode (LLAMACPP_URL set) only
   * allows downloads, never start/use.
   */
  import { onMount, onDestroy } from "svelte";
  import { localModels as llm } from "../assistant/localmodels.store.svelte";
  import { providerStore as providers } from "../assistant/providers.store.svelte";
  import { t } from "../../lib/i18n.svelte";

  onMount(() => void llm.refresh());
  onDestroy(() => llm.stopPoll());

  const pct = (done: number, total: number) =>
    total > 0 ? Math.min(100, Math.round((done / total) * 100)) : 0;
  const activeId = () =>
    providers.activeProvider === "llama.cpp" ? providers.activeModel : "";
</script>

<div class="flex flex-col gap-4">
  <p class="muted text-sm">{t("localmodels.note")}</p>
  {#if llm.externalServer}
    <p class="warn-box">{t("localmodels.external")}</p>
  {/if}
  {#if llm.error}
    <p class="error-box">{llm.error}</p>
  {/if}
  {#if llm.loading && llm.models.length === 0}
    <p class="faint mono text-[11px]">…</p>
  {/if}
  {#each llm.models as m (m.id)}
    <div class="group-card">
      <div class="flex flex-wrap items-start justify-between gap-4">
        <div>
          <h3 class="group-title">
            {m.label}
            <span class="faint mono text-[11px]"> · {m.params}</span>
          </h3>
          <p class="faint mono mt-0.5 text-[11px]">
            {llm.fmtSize(m.size)}
            {#if m.state === "ready"}
              · {t("localmodels.ready")}
              {#if m.serving}· {t("localmodels.serving")}{/if}
              {#if activeId() === m.id}· {t("localmodels.active")}{/if}
            {:else if m.state === "downloading"}
              · {pct(m.downloaded_bytes, m.size)}% ({llm.fmtSize(
                m.downloaded_bytes,
              )})
            {:else}
              · {t("localmodels.notDownloaded")}
            {/if}
          </p>
          {#if m.state === "downloading"}
            <div
              class="mt-2 h-1.5 w-full"
              style="background: var(--surface-2);"
              role="progressbar"
              aria-valuenow={pct(m.downloaded_bytes, m.size)}
              aria-valuemin={0}
              aria-valuemax={100}
            >
              <div
                class="h-full"
                style="width: {pct(
                  m.downloaded_bytes,
                  m.size,
                )}%; background: var(--accent);"
              ></div>
            </div>
          {/if}
          {#if m.last_error}
            <p class="error-box mt-1">{m.last_error}</p>
          {/if}
        </div>
        <div class="flex flex-wrap items-center gap-2">
          {#if m.state === "not_downloaded"}
            <button
              class="btn btn-ghost"
              style="padding: 0.375rem 0.75rem; font-size: 0.75rem;"
              disabled={llm.busy !== null}
              onclick={() => void llm.download(m.id)}
            >
              {t("localmodels.download")}
            </button>
          {:else if m.state === "downloading"}
            <button
              class="btn btn-ghost"
              style="padding: 0.375rem 0.75rem; font-size: 0.75rem;"
              onclick={() => void llm.cancel(m.id)}
            >
              {t("localmodels.cancel")}
            </button>
          {:else if activeId() !== m.id}
            <button
              class="btn btn-ghost"
              style="padding: 0.375rem 0.75rem; font-size: 0.75rem;"
              disabled={llm.busy !== null || llm.externalServer}
              onclick={() => void llm.use(m.id)}
            >
              {llm.busy === m.id
                ? t("localmodels.starting")
                : t("localmodels.use")}
            </button>
          {/if}
          {#if m.state !== "not_downloaded"}
            <button
              class="btn btn-ghost"
              style="padding: 0.375rem 0.75rem; font-size: 0.75rem;"
              disabled={llm.busy !== null}
              onclick={() => void llm.remove(m.id)}
              title={t("localmodels.deleteHint")}
            >
              {t("localmodels.delete")}
            </button>
          {/if}
        </div>
      </div>
    </div>
  {/each}
</div>
