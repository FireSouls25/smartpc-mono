<!-- Three-pane resizable row. Layout owns geometry, panes own content:
  widths + visibility live in panes.svelte.ts (persisted, versioned key),
  the center always takes the remainder and never drops below its minimum.
  This component is a thin event forwarder by design. -->
<script lang="ts">
  import { onMount } from "svelte";
  import type { Snippet } from "svelte";
  import { t } from "../../lib/i18n.svelte";
  import { panes } from "./panes.svelte";

  let {
    left,
    center,
    right,
  }: {
    left: Snippet;
    center: Snippet;
    // Always passed (SessionsPane); CSS hides it below xl.
    right: Snippet;
  } = $props();

  let rowEl: HTMLElement | null = null;

  onMount(() => panes.mount(rowEl));
</script>

<div
  bind:this={rowEl}
  class="resizable-row {panes.dragging ? 'resizing' : ''}"
  role="group"
  aria-label="Panels"
  onpointermove={(e) => panes.dividerMove(e, rowEl)}
  onpointerup={() => panes.dividerUp()}
  onpointercancel={() => panes.dividerUp()}
>
  {#if panes.showLeft}
    <section
      class="pane pane-side"
      style="--pw: {panes.left}px;"
      data-testid="pane-a"
      aria-label="Activity"
    >
      {@render left()}
      <button
        type="button"
        class="pane-collapse edge-right"
        onclick={() => panes.toggleLeft()}
        aria-label={t("common.hide")}
        title={t("common.hide")}
      >
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <path d="M15 6l-6 6 6 6" />
        </svg>
      </button>
    </section>
    <div
      class="divider"
      role="separator"
      aria-orientation="vertical"
      aria-label="Resize activity and chat panels"
      title="Drag to resize · double-click to reset"
      onpointerdown={(e) => panes.dividerDown("left", e)}
      ondblclick={() => panes.dividerReset("left")}
    >
      <span></span>
    </div>
  {:else}
    <button
      type="button"
      class="rail"
      data-testid="rail-a"
      onclick={() => panes.toggleLeft()}
      aria-label={t("common.show")}
      title={t("common.show")}
    >
      <svg viewBox="0 0 24 24" aria-hidden="true">
        <path d="M9 6l6 6-6 6" />
      </svg>
    </button>
  {/if}

  <section class="pane pane-center" data-testid="pane-b" aria-label="Chat">
    {@render center()}
  </section>

  {#if panes.showRight}
    <div
      class="divider divider-right"
      role="separator"
      aria-orientation="vertical"
      aria-label="Resize chat and sessions panels"
      title="Drag to resize · double-click to reset"
      onpointerdown={(e) => panes.dividerDown("right", e)}
      ondblclick={() => panes.dividerReset("right")}
    >
      <span></span>
    </div>
    <section
      class="pane pane-side pane-right"
      style="--pw: {panes.right}px;"
      data-testid="pane-c"
      aria-label="Sessions"
    >
      {@render right()}
      <button
        type="button"
        class="pane-collapse edge-left"
        onclick={() => panes.toggleRight()}
        aria-label={t("common.hide")}
        title={t("common.hide")}
      >
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <path d="M9 6l6 6-6 6" />
        </svg>
      </button>
    </section>
  {:else}
    <button
      type="button"
      class="rail rail-right"
      data-testid="rail-c"
      onclick={() => panes.toggleRight()}
      aria-label={t("common.show")}
      title={t("common.show")}
    >
      <svg viewBox="0 0 24 24" aria-hidden="true">
        <path d="M15 6l-6 6 6 6" />
      </svg>
    </button>
  {/if}
</div>
