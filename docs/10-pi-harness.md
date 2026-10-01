# 10 — Pi harness: the reasoning engine (embedded RPC)

Status: **pi is the only engine** (native hand-rolled loop removed
2026-09-23): `harness/agent.rs` deleted, `chat`/`run` both run
`pi_chat_turn`, the provider catalog comes from pi, vendors expose only
identity + catalog + credentials. Proven live: providers from pi catalog,
plain + tool turns through `pi --mode rpc` + ollama/gemma4, actions persisted,
`test:e2e:pi` green through the real UI.

Cancellation: the renderer aborts its HTTP wait and flags the turn via
`POST /v1/ai/cancel`; the turn loop aborts pi and unwinds (new
`PiError::Cancelled`), freeing the per-user turn lock.

## Validated spikes (do not re-prove)

- RPC roundtrip: `pi --mode rpc` speaks strict JSONL (`\n` only — never Node
  `readline`); `get_available_models`/`get_state`/`prompt`/`agent_settled`
  all behave per docs. (`/tmp/opencode/pi-spike/`, EPIPE at the end was our
  `head -c` closing the pipe, not a pi bug.)
- Custom tools via `-e` extension + HTTP callback into our process works end
  to end: gemma4:e2b called `test_ping`, bridge answered, result flowed back,
  reply `DONE`, clean `agent_settled`.
- **Tool-space hygiene is mandatory**: the user's global packages
  (pi-web-access, pi-btw, …) auto-load in RPC and flood the model with web
  tools. Our spawn must use
  `--no-extensions --no-skills --no-builtin-tools -t <our-6-tools>`
  (explicit `-e` still loads). This _is_ our sandbox boundary with pi.
- pi 0.85.1 installed; node 26 available; user's pi already authenticates
  `opencode` (matches our default provider).

## pi-listen verdict (important)

pi-listen registers **zero tools** — transcripts flow only into the TUI
editor (`setEditorText`), and hold-to-talk is a TUI keybinding. In RPC mode
there is no TUI: STT-via-pi-listen would mean driving `/voice dictate`
through prompts and scraping `set_editor_text` side-channel events. Fragile,
unobservable, no wake word, no silence-finalize. **Recommendation: keep our
whisper STT** (proven 1.5 s, integrated VAD/wake/device picker); adopt
pi-listen **only for TTS** (`/voice-speak <text>` by prompt after settle —
validate headless playback + no history pollution first), Phase 2.

## Architecture

```text
renderer ──HTTP──► sidecar ──stdio JSONL──► pi --mode rpc
                       │      ▲  ▲               │
                       │      │  │               ├── our 6 tools (TS ext)
                       │      │  │                    │ HTTP callback
                       │      │  │                    ▼
                       │      │  └────────► extension_ui_request (auto-policy)
                       │      └───────── prompt/steer/abort/switch/set_model…
                       └── SQLite (sessions, messages, actions), auth, policy
```

- **Process model**: one pi child per sidecar (spawn cost is seconds:
  extension load + catalog fetch), multiplexed across users/turns with an
  async mutex + `switch_session` per chat turn. Single-user desktop makes
  serialization acceptable; document it.
- **Tool bridge**: vendored TS extension (`src/pi-bridge/`, loaded via `-e`)
  registering our exact 6 tools (schemas converted from `tools::catalog`,
  contract-tested for parity). `execute()` POSTs to a sidecar-internal
  endpoint (same axum server, sidecar-token gate, `{pi_session, name, args}`
  → resolves our user+session → `exec::execute` with `Policy`). No Node-side
  OS access, ever.
- **Actions**: `tool_execution_start` → Action row `running`;
  `tool_execution_end` → done/failed. Only `records_action` tools create
  rows (same rule as today). Steps for `RunResponse` from the same events.
- **Permissions**: extension `confirm`/`select` dialogs arrive as
  `extension_ui_request` → sidecar auto-answers per policy (v1: same as
  today — everything runs except `type_text` without `HARNESS_ALLOW_RISKY`;
  UI confirmation hook later).
- **System prompt + context**: keep `prompt::system_prompt` + per-turn
  `context::gather()` (fresh facts beat pi's coding prompt for our domain),
  passed via `--system-prompt` at spawn (replaces pi's coding prompt) and
  context JSON prepended to the user message exactly like today. Keep
  `TURN_REMINDER` until pi-side behavior proves it unneeded.
- **Sessions**: new `pi_session_file TEXT` column on chat sessions (SQLite
  migration). Turn flow per chat session S: mapping missing → `new_session`
  (+ `set_session_name`) → store file; present → `switch_session`. Then
  `set_model(provider, model)` from our stored selection, then `prompt`.
  Completion = `agent_settled` (NOT `agent_end`: retries/compaction follow).
  Reply = last assistant text; context meter from `get_session_stats`.
- **History**: our SQLite stays the UI source of truth (sessions list,
  messages incl. tool turns filtered as today). pi owns LLM context; we
  mirror display state from events. No backfill of old sessions into pi
  (fresh pi session per old chat on next turn — acceptable, documented).
- **Providers/models endpoint**: `get_available_models` (cached at boot +
  poll) grouped by provider → `ProviderInfo`; `available` keeps our current
  semantics (live probe for ollama/llama.cpp, key presence for keyed);
  `context_window` from pi (strictly better: real values for Zen/llama.cpp);
  selection persists in sidecar as today → `set_model` per turn. Our
  `registerProvider` extension row adds ollama/llama.cpp/Zen endpoints so pi
  works even without user pi config. Since the RPC only reports authenticated
  - local providers, the endpoint appends pi's built-in registry from a
    version-pinned repo snapshot (`src/native/assets/pi-models.json`,
    regen via `scripts/regen-pi-models.mjs`) so the UI lists, validates, and
    takes keys for the whole catalog — live data wins whenever present.
- **Keys**: sidecar injects keyring/file keys as `<PROVIDER>_API_KEY` env for
  the pi child, using pi's exact env table mirrored in `secrets.rs`
  (`PI_KEY_ENV`, from pi's `packages/ai/src/env-api-keys.ts`, pi 0.85.1 —
  generic derivation is wrong for google→GEMINI_API_KEY,
  azure-openai-responses, huggingface→HF_TOKEN, so it's table-first).
  Verified live: `ANTHROPIC_API_KEY=… pi auth check` reports ready, and a
  pasted key was observed in the spawned child's environment. OAuth-only ids
  (copilot, codex, bedrock) refuse paste with a `pi auth` pointer. Never
  log, never persist elsewhere (the provider index lists ids only).
- **Chat vs run**: pi has no "tool-less chat" per turn → **unify**: every
  turn may act; `steps` carries what happened (empty when pure chat). UI
  unchanged (it already renders steps). The "plain chat never creates
  actions" invariant is replaced by "read-only turns create none".
- **Abort/stop**: UI stop → RPC `abort`. Delete-session → drop mapping
  (+ optional pi session file cleanup, best-effort).
- **Distribution (v1)**: require `pi` + `node` on PATH at sidecar boot;
  clear `misconfigured` error otherwise (Electron dialog already exists for
  backend failures). Packaged fallback later: `npx -y <pinned pi>` (npm
  cache) — no code change, spawn-command switch.

## Rollout (behind `PI_HARNESS=1`, default off)

- Phase 1 (implemented): `src/native/src/pi/` —
  `supervisor.rs` (per-user spawn, strict-\n JSONL, id correlation, turn
  mutex, catalog cache, dialog auto-policy), `turn.rs` (SQLite session-file
  mapping, `set_model`, prompt→`agent_settled`, steps/actions/context
  mapping), `providers.rs` (pi catalog + loopback probes + pi-auth key
  check), `routes.rs` (bridge endpoints), `pi-bridge/smartpc.ts` (zero-dep
  extension: schemas + local providers from Rust, tools callback with
  pi-session attribution). `run`/`chat`/`providers`/`save_key`/`delete_key`
  branch on the flag; old harness stays default. `test:e2e:pi` proves a real
  UI→pi→ollama turn (incl. a live tool call mapped to steps).
- Live results 2026-09-22: plain turn exact reply + real token usage (1350);
  tool turn executed in Rust with output mapped; missing default model fails
  loudly at `set_model` (no silent fallback). First real-tool bug caught by
  the spike (bridge omitted `pi_session` → 409s) is fixed and covered.
- Phase 2 (TTS output): pi-listen engine via disposable pi children —
  implemented (speak/stop endpoints, auto-speak toggle, isolated pi home,
  per-language Piper/Kitten voices). Extension commands emit no turn events
  (spike-proven), so completion is watchdog-based, never awaited.
- Phase 3: flip default, remove old harness (`ai/provider.rs`,
  `harness/agent.rs` loop, `openai_compat` chat paths) only after a full
  green cycle. Keep `harness/exec.rs` + `tools.rs` + `context/` + `prompt/`
  permanently (the sandbox outlives the loop).

## What this buys (and doesn't)

Buys: maintained reasoning loop (retries, compaction, steering, abort),
pi's provider/model catalog + auth, thinking levels later, extension
ecosystem later, no more hand-rolled tool-call parsing. Doesn't change:
UI, sessions/actions storage, auth, keyring, policy sandbox, whisper STT,
Rust execution of every OS action.
