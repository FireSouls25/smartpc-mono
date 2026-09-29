// Pane geometry for the main three-pane row. Layout owns geometry, panes
// own content: side widths + visibility persist (versioned key), the center
// always takes the remainder and never drops below its minimum.
//
// State lives here (module `$state`, like the voice store) so components
// stay thin event forwarders.
export interface PanesState {
  left: number;
  right: number;
  showLeft: boolean;
  showRight: boolean;
}

export const PANES_KEY = "smartpc.panes.v2";
export const MIN_LEFT = 200;
export const MIN_RIGHT = 220;
export const MIN_CENTER = 360;
export const DEFAULT_LEFT = 280;
export const DEFAULT_RIGHT = 300;

const defaults = (): PanesState => ({
  left: DEFAULT_LEFT,
  right: DEFAULT_RIGHT,
  showLeft: true,
  showRight: true,
});

function clampNum(v: unknown, min: number, max: number, fb: number): number {
  if (typeof v !== "number" || Number.isNaN(v)) return fb;
  if (v < min) return min;
  if (v > max) return max;
  return Math.round(v);
}

/** Fit widths into a measured row; the center keeps MIN_CENTER. */
function clampToRow(s: PanesState, rowW: number): PanesState {
  const left = clampNum(
    s.left,
    MIN_LEFT,
    Math.max(MIN_LEFT, rowW - MIN_CENTER - MIN_RIGHT),
    DEFAULT_LEFT,
  );
  const right = clampNum(
    s.right,
    MIN_RIGHT,
    Math.max(MIN_RIGHT, rowW - MIN_CENTER - left),
    DEFAULT_RIGHT,
  );
  return { left, right, showLeft: !!s.showLeft, showRight: !!s.showRight };
}

function load(): PanesState {
  try {
    const raw = window.localStorage.getItem(PANES_KEY);
    if (raw) {
      const p = JSON.parse(raw) as Partial<PanesState>;
      const w = window.innerWidth || 1280;
      return clampToRow(
        {
          left: p.left ?? DEFAULT_LEFT,
          right: p.right ?? DEFAULT_RIGHT,
          showLeft: p.showLeft ?? true,
          showRight: p.showRight ?? true,
        },
        w,
      );
    }
  } catch {
    /* private mode */
  }
  return defaults();
}

let state = $state<PanesState>(defaults());
let dragSide = $state<"left" | "right" | null>(null);
// Last throttled save during a drag (pointerup can be lost to window blur,
// alerts, or touch interruptions — the last move already persisted then).
let lastMoveSave = 0;
const MOVE_SAVE_MS = 150;

function save(): void {
  try {
    window.localStorage.setItem(PANES_KEY, JSON.stringify(state));
  } catch {
    /* private mode */
  }
}

/** Self-heal + track container size. Call from onMount with the row element;
 * returns the resize-listener cleanup. In-memory only: mount never saves,
 * so a transient narrow first layout (CSS/fonts still streaming) can't
 * cement shrunken widths into storage — the next user drag/toggle persists.
 * Below lg the row stacks and side widths don't apply: skip entirely. */
function mount(rowEl: HTMLElement | null): () => void {
  const refit = (persist: boolean): void => {
    if (!rowEl) return;
    const w = rowEl.getBoundingClientRect().width;
    if (w < 1024) return;
    const next = clampToRow(state, w);
    if (
      next.left !== state.left ||
      next.right !== state.right ||
      next.showLeft !== state.showLeft ||
      next.showRight !== state.showRight
    ) {
      state = next;
      if (persist) save();
    }
  };
  if (rowEl) {
    // Double rAF: measure after first paint, not against pre-CSS layout.
    requestAnimationFrame(() =>
      requestAnimationFrame(() => {
        if (!rowEl) return;
        const w = rowEl.getBoundingClientRect().width;
        if (w >= 1024) {
          state = clampToRow(load(), w);
        } else {
          state = load();
        }
      }),
    );
  } else {
    state = load();
  }
  const onResize = (): void => refit(true);
  window.addEventListener("resize", onResize);
  return () => window.removeEventListener("resize", onResize);
}

function dividerDown(side: "left" | "right", e: PointerEvent): void {
  dragSide = side;
  (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
}

function dividerMove(e: PointerEvent, rowEl: HTMLElement | null): void {
  if (!dragSide || !rowEl) return;
  const rect = rowEl.getBoundingClientRect();
  if (dragSide === "left") {
    // Right edge of the left pane follows the pointer.
    state.left = clampNum(
      e.clientX - rect.left,
      MIN_LEFT,
      Math.max(MIN_LEFT, rect.width - MIN_CENTER - state.right),
      state.left,
    );
  } else {
    // Left edge of the right pane follows the pointer.
    state.right = clampNum(
      rect.right - e.clientX,
      MIN_RIGHT,
      Math.max(MIN_RIGHT, rect.width - MIN_CENTER - state.left),
      state.right,
    );
  }
  const now = Date.now();
  if (now - lastMoveSave >= MOVE_SAVE_MS) {
    lastMoveSave = now;
    save();
  }
}

function dividerUp(): void {
  if (dragSide) {
    dragSide = null;
    save();
  }
}

/** Double-click a divider: back to defaults. */
function dividerReset(side: "left" | "right"): void {
  if (side === "left") state.left = DEFAULT_LEFT;
  else state.right = DEFAULT_RIGHT;
  save();
}

function toggleLeft(): void {
  state.showLeft = !state.showLeft;
  save();
}

function toggleRight(): void {
  state.showRight = !state.showRight;
  save();
}

export const panes = {
  get left(): number {
    return state.left;
  },
  get right(): number {
    return state.right;
  },
  get showLeft(): boolean {
    return state.showLeft;
  },
  get showRight(): boolean {
    return state.showRight;
  },
  get dragging(): boolean {
    return dragSide !== null;
  },
  mount,
  dividerDown,
  dividerMove,
  dividerUp,
  dividerReset,
  toggleLeft,
  toggleRight,
};
