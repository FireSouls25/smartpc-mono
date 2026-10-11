# 14 — Unified chat + seamless rotation + memory layer

Status: SPEC (operator approved 2026-10-11 with answers: divider chip yes;
thresholds per §4; main screen = chat + actions, history moves to Settings).
Wave brief below — implement, then review.

## Decisions (operator)

1. Rotation surfaces as a one-line dismissible divider chip in the chat
   ("Context refreshed — full history in Settings → History"), cleared on
   the next send. Silent would confuse; loud would break seamlessness.
2. Thresholds (§4): rotate at `used/window ≥ 0.75`; handoff = last 3 turns
   verbatim capped at 3000 chars + current wake notes. Constants with env
   overrides (`SMARTPC_ROTATE_AT`, `SMARTPC_HANDOFF_TURNS`,
   `SMARTPC_HANDOFF_CHARS`). Rationale: an agentic turn is MULTIPLE LLM
   calls (tool loop), so 0.75 — not 0.8 — leaves room for one big final
   turn; the handoff fits any window ≥ ~2k tokens, i.e. every supported
   model from tiny local to user-paid giants (relative fraction adapts
   automatically). Never rotate a session with < 2 turns; a first-turn
   overflow surfaces honestly (nothing to hand off yet).
3. Main screen = CenterPanel (chat) + EventsFeed (actions). SessionsPane
   leaves Shell; history lives in Settings (reusing list/detail).

## 0. Core split (do not redesign)

pi owns LLM context; SQLite sessions are display/audit logs. Rotation =
new log row (+ fresh pi mapping) mid-stream; the renderer holds the
conversation and does nothing. The unified chat is a VIEW; sessions are
STORAGE. Sessions stay immutable (audit, cloud sync, SessionsPane-equivalent
all need exactness) — the tree is cache, never source of truth.

## 1. UI (W1)

- Boot always shows the greeting (verify current behavior; make it
  unconditional, never auto-open last session). `newChat()` IS clear-chat
  (already non-deleting); label it honestly in UI copy if touched.
- Remove SessionsPane from Shell; add Settings → History section reusing
  list/detail components (title, date, count, model; read-only view;
  delete). No behavior change to rows.
- `run` response gains additive `rotated: bool`. chat.store sets
  `justRotated=true` on true, clears on next send; CenterPanel renders the
  divider chip when true. Additive field — no PROTOCOL bump.
- i18n keys for the chip + History section (es contract, en parity).

## 2. Rotation engine (W2, sidecar)

- Migration: `chat_sessions.parent_session_id TEXT NULL` (+ index).
  Detail JSON gains `parent_session_id` (additive, no bump). Cloud sync
  carries the column with the row (last-write-wins per row, as today).
- After each successful turn, evaluate `used/window ≥ ROTATE_AT`
  (from `get_session_stats`, already read). If over AND session has ≥ 2
  turns → set pending-rotation for the NEXT send (never mid-turn).
- Next send with pending flag: freeze row → build handoff (§3) → new row
  with parent link → fresh pi session mapping → turn proceeds; response
  carries `rotated: true`.
- Reactive path: provider context-overflow error OR pi `compaction_end`
  event → set pending flag; if a turn DIES to overflow, auto-retry ONCE
  under a fresh session (cap 1, then honest error).
- Handoff (deterministic, zero-LLM): last HANDOFF_TURNS turns verbatim
  capped at HANDOFF_CHARS chars + current wake notes (§3). Rides the
  existing ephemeral prepend channel (like `turn_context` — never stored).
  Abstractive summary: NOT in v1 (small-model safety; revisit only with
  provider-capability gating).
- Diagnostics line per rotation (old → new id, reason: threshold/overflow/
  compaction). No renderer surface beyond the chip.

## 3. Memory layer (W3, sidecar + catalog)

- New `memory_notes` table: `id, user_id, text (≤280), topic, source
  (user|extracted), created_at, updated_at`. Append-only; NO delete
  endpoint in v1 (mirrors OptMem log + our audit posture).
- `wake`: top notes (recency × topic overlap with the incoming message,
  capped tokens) prepended with `turn_context` every turn. Replaces the
  TURN_REMINDER hack over time (keep the reminder until wake proves out —
  do NOT remove it in this wave).
- Catalog tools (schemas flow, no new routes, no bump):
  `memory_note {text, topic?}` (Medium — persistent prompt content is
  injection surface, confirmation-gated like other Medium tools),
  `memory_recall {query}` (ReadOnly, keyword search over notes),
  `memory_zoom {topic}` (ReadOnly, notes under a topic).
- Inline merge (no daemon, no background): when notes under one topic
  exceed 12, the turn appends a merge step producing ONE line (verbatim
  concat preferred; abstractive only if provider not known-small —
  gate on the active provider id, default to safe).
- Rotation writes a boundary note (`topic … continued in session …`)
  so zoom walks the chain. Notes sync: DEFERRED to phase 2 (local-only
  in v1 — state explicitly in code + docs; do not half-ship cloud).

## 4. Constants

`ROTATE_AT = 0.75` (`SMARTPC_ROTATE_AT`), `HANDOFF_TURNS = 3`
(`SMARTPC_HANDOFF_TURNS`), `HANDOFF_CHARS = 3000`
(`SMARTPC_HANDOFF_CHARS`), `MERGE_TOPIC_CAP = 12`. Minimum 2 turns per
session before rotation eligible.

## 5. Tests (normative)

- Unit (Rust): threshold math (0.75 incl. boundary), handoff builder
  (3-turn/3000-char caps, wake fold-in), join/chunk reuse patterns.
- Contract: detail shows `parent_session_id`; run shows `rotated`;
  memory tools in catalog with exact schemas + risk levels; list/detail
  unchanged otherwise. PROTOCOL pin UNCHANGED (assert it).
- Headless E2E: force tiny window via env → two turns → two linked rows,
  chat array intact; chip appears; Settings History lists both; boot
  shows greeting, never auto-opens.
- Memory: recall precision on fixtures, zoom walk across a rotation
  boundary note, gated-write denial without toggle (Medium).
- i18n parity enforced as usual.

## 6. Out of scope (explicit)

Removing TURN_REMINDER, notes cloud sync, abstractive handoff summaries,
memory TUI/browser, embedding/FTS5 retrieval, touching STT/TTS/policy.
