# 12 — Expansion design (tools, voice, grounding, safety)

Status: DESIGN APPROVED WITH FIXES (2026-10-07). 4 track designs + 2 verifier
passes complete (contract consistency: 2 FAILs fixed below; safety: BLOCK
lifted by adopting the 5 P0 minimal fixes). This file is the merged authority;
it supersedes the per-track artifacts on every conflict.

Locked operator decisions (2026-10-07): any-https for `open_url` (AMENDED by
safety P0-a — see §2, needs operator confirm); screenshots from the beginning;
Piper-only TTS; Laya discarded (deterministic risk matrix); risky gating
graduates to a Settings toggle; all §G ideas (G1 confirmation, G2 screenshot,
G3 dry-run, G4 budgets, G5 audit) accepted.

## Merge order (verifier B2/B3 — binding)

1. **T1 grounding** — additive foundation (`context.rs`, new `screen.rs`,
   `platform.rs`, `prompt.rs`).
2. **T2 open + policy** — `open_url` + `Policy::allows/requires_confirmation` +
   supervisor deny-by-default channel.
3. **T4 control + budgets** — rebases T1+T2 arms onto the budgeted
   `execute()` signature, unifies the Policy API, issues the single
   PROTOCOL 3 → 4.
4. **T3 voice** — disjoint seam, lands any time (suggest last); folds its
   surfaces into the step-3 PROTOCOL notes.

No `Risk` variants are added anywhere (joint T2/T4 agreement). Test-count
assertions use contains-key per tool name, never totals.

## 1. Grounding (T1)

### Extended SystemContext (additive; existing fields untouched)

`MemoryDetail{total_mb, available_mb, used_pct}`, `CpuDetail{logical,
physical?, brand, usage_pct}`, `DisplaySummary{count, primary}`,
plus `hostname`, `uptime_s`, `load_avg[3]`, `mem`, `cpu`, `displays`,
`screen_runnable`. Display data from new `harness/screen.rs`, NOT sysinfo.

`render()` gains exactly two lines (prompt stays small per the gemma-class
constraint in `prompt.rs`):

```
host: {hostname or "-"} · up {uptime_h}h · load {l1}/{l5}/{l15}
mem: {used_pct}% of {total_mb} MB · cpu: {brand} x{logical} ({usage_pct}%) · displays: {count} ({primary})
```

ADOPTED safety P2: `hostname` stays **tool-JSON-only** (prompt carries only
mem/cpu/display lines) — T1 open Q3 resolved.

TTL cache: `gather_cached()` (5 s, targeted refreshes only — never
`refresh_all`/`new_all` per turn); `gather_fresh()` kept for tests and the
tool's `refresh:true` passthrough. `turn_context()` uses the cached path.

### get_display_info (ReadOnly, records_action:false)

`{refresh?: bool}` → `{displays:[{id,name,width,height,x,y,scale,
refresh_hz,primary}], focused_display}`. Geometry in physical pixels; empty
array + note when headless. Backend: xrandr/wlr-randr/kscreen-doctor →
EnumDisplayMonitors → CoreGraphics; any failure → empty, never error.

### capture_screen (High, records_action:true, "Captura de pantalla")

`{display?, region?, max_width? (default 960, 320–1280), format:"png"}` →
`{display, region, size, bytes, format, note}` — metadata only, bytes NEVER
in tool results/logs/exports. Pipeline: OS capture
(X11/xcb-image, Wayland portal, DXGI, CGWindowListCreateImage) → downscale
(no upscale) → PNG squeeze to ≤200 KB (else `ok:false`) → frame validated
then discarded (no viewer yet: no model image path, no UI preview).

AS-BUILT (cleanup pass): the retained shot store (`store_shot`/`take_shot`,
cap-5 LRU, 60 s TTL) was removed — nothing ever consumed it, so it was pure
dead weight plus two `dead_code` warnings. The result carries no `shot_id`;
the grounding gate (success/failure per turn) is unaffected. If a viewer
ever lands, re-add a capped store at that point.

ADOPTED safety P0-b fixes: (1) lock check fails **closed** (`unknown` →
`ShotError::Locked`); (2) title deny-list extended (case-insensitive):
`password, credential, keychain, contraseña` + `login, signin, otp, 2fa,
passkey, pin, bank`; (3) description + denial copy state disclosure:
"Captures pixels visible on screen (may include secrets) and sends them to
the model. Requires risky consent."; (4) T1 open Q1 resolved: `get_shot`
REJECTED for v1 (bytes never enter tool results).
ADOPTED safety P1 (partially superseded): audit rows carry size-only (no
`region`); the TTL + destructive-take half fell away with the store removal
above — nothing is retained, so there is nothing to expire.
T1 open Q2 resolved: keep 960 px / 200 KB defaults.

New deps: `screenshots 0.2` + `image 0.25` (no model/ONNX). `pi-bridge`
untouched (schemas flow from Rust).

## 2. open_url + deterministic risk matrix + confirmation hook (T2)

### open_url — Medium, records_action:true (AMENDED, was Low)

Schema: `{url (≤2048 chars, required), browser? (plain-name rules)}`,
`required:["url"]`, `additionalProperties:false`. `title_for`: "Abrir enlace
{host+path ≤60 chars}" (query stripped in chat-visible text).

Per-OS launch reuses `spawn_verified` + 600 ms fast-fail + detached reaper;
URL always a single argv element, never through a shell. Default: `open` /
`cmd /C start` / `xdg-open`; with `browser`: `open -a` / `start <browser>` /
`Command::new(browser).arg(url)`. Shared `validate_plain_name()` across
open_app/close_app/open_url-browser.

Validation order: trim → ≤2048 → absolute URI with authority (no bare words)
→ scheme exactly `http/https` (scheme-specific refusals; **contract test:
`javascript:alert(1)` → `ok:false` containing `refused javascript:`**) →
reject controls/whitespace/backslashes → `browser` plain-name check.

ADOPTED safety P0-a fixes (⚠️ AMENDS locked decision 1 — operator confirm
requested): (1) Risk Low → **Medium**, routed through `requires_confirmation`
(deny-by-default until the renderer surface lands); (2) `browser` restricted
to `None` (system default) or fixed allowlist (`firefox, google-chrome,
chromium, microsoft-edge, safari, librewolf`, per-OS subset), else
`err("browser not allowlisted")`; (3) refuse non-global hosts — loopback,
RFC1918, link-local, `*.local`, cloud-metadata IP, `user:pass@` userinfo →
`err("refused local/private URL")`; (4) redaction contract: scheme+host+path
ONLY in logs/exports/chat summaries (query dropped everywhere, not
128-truncated); T4 G5 verbatim passthrough explicitly excludes
`open_url.query`. ADOPTED P1: `http:` URLs always confirm + render "not
secure" prefix in `title_for`/dialog.

T2 open Q1 resolved: strict reject-and-tell (no scheme auto-prepend).
T2 open Q2/Q3 resolved by the P0-a redaction rule (strip, don't truncate;
renderer shows stripped form).

### Deterministic risk matrix (Laya discarded — static table, no scores)

Evaluated in `execute()` before dispatch; arg rules can only refuse, never
escalate. ReadOnly/Low: immediate. Medium: immediate in v1 BUT routed through
`confirm_if_needed(risk)` (T2 §5 gate, default off) so a policy flip needs no
new plumbing. High: iff `policy.allows(risk)` (single predicate fed by env
var AND the T4 toggle). T4 authoritative on `type_text`: ≤200/call,
≤1000/turn (verifier B1 — T2 §4 row amended accordingly).

### Confirmation hook G1 (T2 builds, T4 + renderer consume)

`ChildHandle` gains `PendingConfirm{id, tool, args_summary, risk}` +
confirm channel. `execute()` gates Medium/High via `requires_confirmation`
(120 s timeout → deny `err("not confirmed by user")`). `answer_ui` routes
pending-confirmation dialogs to the future UI endpoint and keeps deny-by-
default (`confirmed:false`, selects/inputs `cancelled:true`) until the
renderer surface lands. ADOPTED verifier B5: T4's four new Medium arms call
the T2 gate (no bypass in v1).

## 3. Voice (T3)

### A1 chunk queue (ships now, frontend-owned; server stays single-utterance)

`splitSpeak(text, maxLen=400)` in `voice.store.svelte.ts`: sentence split
(`/[^.!?…\n]+[.!?…]+["»”’]?\s*|\n+/g`, es `¿? ¡!` aware), markdown stripped
(code fences skipped, links→text), word-wrap fallback, cap 5 chunks (~2000
chars, preserves `MAX_CHARS` budget). Pure + unit-tested.

Pump: detached `void` task (never blocks the turn), sequential
`POST /v1/voice/speak` per chunk (server barge-in semantics preserved —
each chunk a fresh child), per-chunk authoritative `estimated_ms`
(+2 s grace, 250 s cap), shared `estimateSpeakMs` helper so client/server
can't drift. `speaking` true while queue non-empty. UI: `● i/n` progress +
sentence fade-in + cancel (clears queue). Failures: bad chunk skipped,
transport failure aborts queue (conversation → `speakFailed` + `stopAll`;
manual → silent + diagnostic). Empty split → no POSTs.

### Voice catalog + selection + test-play (ships now)

`GET /v1/voice/tts-models` → `{models:[{id, lang, label, gender,
size_mb, quality}], default_for_lang, active}` (`gender` additive, no bump);
`ready` mirrors STT `models_ready`; always 200 (empty = engine missing).
Voice ids are pi-listen model ids with optional `#<sid>` for multi-voice
models (sid rides `ttsLocalVoiceId`); the sid half is validated by whole-id
catalog match, so crafted suffixes fail closed. Catalog: en 2 masculine +
2 feminine (Kitten Nano sids 0/2 + 1/3, shared 25 MB model, M1 default); es
is Davefx alone — the two Kokoro es sids (Álex/Dora, kokoro-int8-multi-lang-
v1_0) were REMOVED after a live failure proved that model emits NaN samples
(pi-listen refuses it outright; neither v1_1 nor fp32 v1_0 ships es voices).
A 2nd es voice of either gender needs an upstream model first. `POST /v1/voice/speak`
gains optional `voice` (unknown → 400 `invalid_voice`); response gains
`model` (composite voice id, additive). `write_tts_config` takes model + sid;
hardcoded table stays as fallback default.

Pref: localStorage `smartpc.voice.ttsVoice.<lang>` (preference, not secret —
consistent with §4 storage rationale); sidecar stays stateless per-call.
Settings "Voz de lectura" group: lang-first dropdown + ready badges +
test-play (canned ~60-char phrase, indeterminate `downloading` progress, 180 s
budget) + `effectiveTts()` line. i18n: `voice.ttsVoice/ttsVoiceHint/testPlay/
testing/downloadingTts/testPhrase/chunkProgress` + `speakHint` rewritten to
Piper (es contract, en parity). Dropdown: current-lang-first (T3 Q3 resolved).
A2 speech-events channel: multiplex `channel:"tts"` on the existing poll
(T3 Q2 resolved).

AS-BUILT (voice-panel pass): catalog trimmed to es + en (operator decision —
the UI supports only those two; `default_for_lang` likewise); `gender` per
entry; `DELETE /v1/voice/tts-models/{id}` implemented after all (uninstall
with stop-first + idempotency, so TTS matches the STT panel: install state,
download-via-test-play with indeterminate progress, per-row remove); TTS
`ready` is now real per-model install state
(`<tts-home>/.pi/models/tts/<model>/tokens.txt`); PROTOCOL 5 covers the
new route + the `ready` semantics change.

### A2 playback takeover (proposal, NOT built)

Rust-owned synth+play (`rodio` + `piper-rs` or `piper` CLI, espeak-ng noted
for Linux) with event-driven `speaking{active,chunk,chunks,model}` /
`chunk{started|done|failed}` / `stopped{barge|done}`. A1 shapes are
forward-compatible (catalog ids become synth ids; events only added).

### Barge-in/watchdog race (ships with A1)

Server: generation re-check between spawn and store (superseded → kill +
`Err("superseded")`). Client: `speakGen` guards the pump (stale results
dropped); in-flight POST resolving post-stop triggers one more idempotent
`stopSpeaking`.

## 4. Control + budgets + audit + risky toggle (T4)

### Mouse (Medium; click High) + key_combo + type_text budget

- `mouse_move{x,y}` Medium, records_action:**true** (verifier change);
  `mouse_click{button?=left,count?=1-2}` **High** (verifier change, gated);
  `mouse_scroll{delta -10..10}` Medium, records_action:**true**.
- ADOPTED safety P0-c: `TurnBudget.grounded` — mouse tools refused unless
  `get_display_info` + `capture_screen(ok)` both succeeded **this turn**
  (`err("need fresh screenshot first…")`); out-of-range → refuse
  (fail-closed, no clamp-into-range).
- `key_combo{combo: copy|paste|cut|undo|redo|save|select_all|find}` Medium,
  records_action:true; macOS Meta-for-Control; denial mirrors `press_key`
  wording; description documents the `select_all→cut/type` wipe composition
  (ADOPTED P1). `redo = Ctrl+Shift+Z` (T4 Q1 resolved).
- `type_text`: `maxLength` **200**/call, 1000/turn, 5 calls/turn (B1).
  Description rewritten honestly (ADOPTED P1 — no unenforceable NEVER claim).
- enigo 0.6 suffices; no new deps. `press_key` media keys exempt from
  cooldown (T4 Q2 resolved). Audit export: per-session v1 (T4 Q3 resolved).

### Budgets G4 (exec top, before risk gate)

`harness/budget.rs::TurnBudget{tool_calls, typed_chars, last_input_ms,
dry_run, grounded}`: `MAX_TOOL_CALLS_PER_TURN=20`,
`MAX_TYPED_CHARS_PER_TURN=1000`, `MAX_TYPE_TEXT_CALLS=5`,
`INPUT_COOLDOWN_MS=400` (synthetic-input tools only; ReadOnly/`open_url`
exempt), `MAX_MOUSE_MOVES_PER_TURN=10`. ADOPTED safety P0-f: session caps
`MAX_MUTATING_PER_SESSION=100` / `MAX_TYPED_CHARS_PER_SESSION=5000`
(run aborts via settle, not a string); 3 consecutive denials/turn → abort;
ReadOnly capped separately (`MAX_READONLY_PER_TURN=30`). Budget denials are
failed `TraceStep`s + `diagnostics::push`. ADOPTED T3-loop P1: conversation
mode never auto-advances after a budget/policy-aborted turn.

### Dry-run G3, audit G5

`POST /v1/ai/run {preview?}` threads through turn → `TurnBudget.dry_run`;
Medium/High return `{ok:true,"preview: <title>"}` with no side effect, no
Action rows (`action_id:None`); ReadOnly/Low execute. EventsFeed prefixes
`Vista previa: ` / `Preview: ` via `title_for`.
`GET /v1/support/audit-export?session_id=` → NDJSON download from existing
stores; `type_text` → `[redacted N chars]`; screenshot bytes excluded by
allowlist; `open_url.query` excluded (§2).

### Risky toggle D5 (graduates `HARNESS_ALLOW_RISKY`)

SQLite `prefs(user_id,key,value)`, `allow_risky_input="1"/"0"`; renderer
mirror non-authoritative. `Policy::for_user = env OR toggle`, resolved per
tool call (cached per turn). Endpoints `GET/PUT /v1/prefs/risky-input`
(`{allowed, source: toggle|env|none}`). Settings → General "Control del PC"
group + switch + `settings.riskyTitle/Label/Hint/EnvNote` (+`auditExport`,
`preview.*`, es+en).
ADOPTED safety P0-e: enabling requires a **trusted renderer gesture**
(`isTrusted`, synthetic enigo input fails closed); revoke one-click always
honored + clears cached Policy; env-set → toggle disabled with "managed by
administrator" copy (never stores an Off that behaves as On); session expiry
noted (`expires_at` follow-up).

## 5. Single PROTOCOL 3 → 4 (verifier B4 — union)

One bump covering: `speak.{voice,model}`, `GET /v1/voice/tts-models`,
`GET/PUT /v1/prefs/risky-input`, `GET /v1/support/audit-export`,
`RunBody.preview`, `type_text.maxLength` 200. Shell `SIDECAR_PROTOCOL`
check + contract test advance together. T1/T2 surfaces ride free (no shape
change).

PROTOCOL 5 (voice-panel pass): `DELETE /v1/voice/tts-models/{id}` +
per-model TTS `ready` semantics + `gender` on catalog entries.

## 6. Build order (binding, from §Merge order)

T1 → T2 → T4 (issues the bump) → T3 last. Contract tests land in merge order
(additive); T3 voice tests appended after the Rust tracks.

## 7. Test summary (per-track plans are normative)

Rust unit (mapping fns, validation, budget truth tables, TTL, deny-list,
generation re-check, voice gender counts + sid resolution) · contract (`sidecar.contract.test.ts`:
new keys/shapes, `javascript:` refusal, `invalid_voice`, prefs round-trip,
audit redaction, preview creates no Actions) · headless-safe E2E (no
display/mic; real-speech gated behind `SMARTPC_TTS_LIVE=1`; launch-success
browser/display passes manual-only). `javascript:alert(1)` → `ok:false`
containing `refused javascript:` is the T2 gate test.

## 8. Residual risks / deferred

- Confirmation renderer surface (dialog UI + `GET /internal/ui/
  confirmations` push) is designed (T2 §5) but unscheduled — until then
  Medium/High fail closed when gated.
- A2 rodio takeover, TTS DELETE endpoint, whole-history audit export,
  per-tool toggle granularity, toggle `expires_at` — deferred, recorded.
- `http:` URLs always confirm even post-P0 (P1 adopted); punycode display
  hardening stays an open renderer concern (T2 Q3).

*Track artifacts: `design/t1-grounding.md`, `t2-open-policy.md`,
`t3-voice.md`, `t4-control.md`, `verify-contracts.md`, `verify-safety.md`
(run 5eb76436). Verifier B1–B5 all resolved above; safety BLOCK lifted by the
adopted P0 fixes. Deviation from locked decision 1 (open_url Medium+gated,
private-host refusal, query-stripping) is flagged for operator confirm in the
delivery report.*
