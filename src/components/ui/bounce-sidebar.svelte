<script lang="ts">
  /*
   * "Bounce Sidebar" — Svelte port of the Rare UI component
   * (rareui.com/components/bouncesidebar, free personal + commercial).
   * A nav list with a dot that hops between items along an arc (Web
   * Animations API, reduced-motion aware). No React runtime: this file
   * replaced components/ui/bounce-sidebar.tsx + shared/ReactIsland.svelte.
   */
  import { onMount } from "svelte";
  import { cn } from "../../lib/cn";

  export type BounceSidebarItem =
    | string
    | { label: string; href?: string }
    | { label: string; heading: true };

  let {
    items,
    value,
    defaultValue = 0,
    onChange,
    dotColor = "#8839ef",
    class: className = "",
    ...rest
  }: {
    items: BounceSidebarItem[];
    value?: number;
    defaultValue?: number;
    onChange?: (index: number) => void;
    dotColor?: string;
    class?: string;
    [key: string]: unknown;
  } = $props();

  // Uncontrolled fallback: read live (not snapshotted) so a late
  // defaultValue still applies; explicit selections win via `internal`.
  let internal = $state<number | undefined>(undefined);
  const active = () => value ?? internal ?? defaultValue;

  let listEl: HTMLUListElement | null = null;
  let dotEl: HTMLSpanElement | null = null;
  let prevY: number | null = null;
  let ready = $state(false);
  // Declarative (not imperative): Svelte rewrites the whole `style`
  // attribute when any interpolated part changes, which would wipe an
  // el.style.transform write. WAAPI owns the flight; dotY owns rest.
  let dotY = $state<number | null>(null);

  const labelOf = (item: BounceSidebarItem): string =>
    typeof item === "string" ? item : item.label;
  const isHeading = (item: BounceSidebarItem): boolean =>
    typeof item !== "string" && "heading" in item;
  const hrefOf = (item: BounceSidebarItem): string | undefined =>
    typeof item === "string" || "heading" in item ? undefined : item.href;

  function itemAt(i: number): HTMLLIElement | null {
    return listEl?.querySelector(`li[data-index="${i}"]`) ?? null;
  }

  function reducedMotion(): boolean {
    return (
      typeof window.matchMedia === "function" &&
      window.matchMedia("(prefers-reduced-motion: reduce)").matches
    );
  }

  let currentAnim: Animation | null = null;

  function place(): void {
    const el = itemAt(active());
    const dot = dotEl;
    if (!el || !dot) return;
    const size = 6;
    const toY = el.offsetTop + el.offsetHeight / 2 - size / 2;
    const fromY = prevY;
    prevY = toY;
    ready = true;
    currentAnim?.cancel();
    currentAnim = null;
    if (fromY === null || toY === fromY || reducedMotion()) {
      dotY = toY;
      return;
    }
    // Arc hop: sideways bulge peaking halfway, committed on finish.
    const bulge =
      Math.min(24, Math.max(6, Math.abs(toY - fromY) * 0.25)) *
      (toY > fromY ? -1 : 1);
    const anim = dot.animate(
      [
        { transform: `translate(0px, ${fromY}px)` },
        {
          transform: `translate(${bulge}px, ${(fromY + toY) / 2}px)`,
          offset: 0.5,
        },
        { transform: `translate(0px, ${toY}px)` },
      ],
      { duration: 250, easing: "cubic-bezier(0.2, 0.8, 0.3, 1)" },
    );
    anim.onfinish = () => {
      dotY = toY;
      if (currentAnim === anim) currentAnim = null;
    };
    currentAnim = anim;
  }

  function select(index: number): void {
    if (value === undefined) internal = index;
    onChange?.(index);
  }

  // Layout reads force sync reflow, so place() runs inline — no rAF race
  // with screenshots. Web fonts can shift rows after mount: re-seat once.
  onMount(() => {
    document.fonts?.ready.then(() => place()).catch(() => {});
  });

  $effect(() => {
    // Tracked: active item + item list (re-renders on language switch
    // re-seat the dot instead of stranding it between rows).
    void active();
    void items;
    place();
  });
</script>

<ul
  bind:this={listEl}
  data-slot="bounce-sidebar"
  class={cn("relative flex flex-col gap-1 pl-6", className)}
  {...rest}
>
  <span
    bind:this={dotEl}
    aria-hidden="true"
    class="absolute left-2 top-0 transition-opacity duration-150"
    style="width: 6px; height: 6px; background-color: {dotColor}; opacity: {ready
      ? 1
      : 0}; transform: {dotY === null ? 'none' : `translate(0px, ${dotY}px)`};"
  ></span>

  {#each items as item, index (index)}
    {@const label = labelOf(item)}
    {#if isHeading(item)}
      <li
        data-index={index}
        role="presentation"
        data-slot="bounce-sidebar-heading"
        style="color: {dotColor};"
        class="px-1 pb-1 pt-7 text-[11px] font-semibold uppercase tracking-[0.14em] first:pt-0"
      >
        {label}
      </li>
    {:else}
      {@const href = hrefOf(item)}
      {@const isActive = index === active()}
      {@const itemClass = cn(
        "flex w-full cursor-pointer items-center p-1 text-left text-sm transition-colors duration-200",
        isActive ? "text-foreground" : "text-foreground/50",
      )}
      <li data-index={index}>
        {#if href}
          <a
            {href}
            data-slot="bounce-sidebar-item"
            data-active={isActive}
            onclick={() => select(index)}
            class={itemClass}
          >
            {label}
          </a>
        {:else}
          <button
            type="button"
            data-slot="bounce-sidebar-item"
            data-active={isActive}
            onclick={() => select(index)}
            class={itemClass}
          >
            {label}
          </button>
        {/if}
      </li>
    {/if}
  {/each}
</ul>
