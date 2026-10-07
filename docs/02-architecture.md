# 02 — Architecture

## Layers

```
┌─ Renderer (Svelte 5 runes, no Node) ─────────────────┐
│ app/ → domains/ → shared/ + components/ui/ → lib/    │
└──────────────────┬───────────────────────────────────┘
                   │ HTTP loopback (fetch) + window.smartpc bridge
┌─ Electron main (Node, privileged) ───────────────────┐
│ spawns sidecar, injects url+token via env → preload  │
└──────────────────┬───────────────────────────────────┘
                   │ spawn --port 0 --token <random> --db <userData>
┌─ Rust sidecar (smartpc-native) ──────────────────────┐
│ auth · ai providers · chat sessions · actions ·      │
│ harness (computer-use sandbox) · SQLite              │
└──────────────────────────────────────────────────────┘
```

Hard boundary: renderer ↔ main speak **only** through `window.smartpc`
(`electron/preload.cjs` allow-list: `versions`, `ping`, `sidecar`).
`nodeIntegration: false`, `sandbox: true`, `contextIsolation: true`
(`electron/main.cjs:66-80`).

## Renderer: domain ownership

Each business domain owns screens + API client + runes stores:

- `domains/auth/` — `auth.api.ts` (6 endpoints), `auth.store.svelte.ts`
  (`user`, in-memory access token, vault-first refresh tokens).
- `domains/assistant/` — `assistant.api.ts` (providers/selection/chat/run/
  sessions/actions/keys/start) plus three acyclic stores:
  `providers.store` (catalog, selection, keys, start, 3 s watch),
  `sessions.store` (directory: list, active pointer, viewed combo — imports
  only the API client), `chat.store` (conversation, context meter, lifecycle
  orchestrating the other two; `send` returns whether it dispatched, `setOrb`
  lets voice drive the orb).
  `run` → `sessionDetail` → re-render (tool turns filtered out) → `refreshSessions`.
- `domains/voice/` — `voice.api.ts` + `voice.store.svelte.ts` (prefs, poll
  loop with cursor + generation guard, transcript → `chat.send`, busy
  fallback to draft). Imports chat, never the reverse.
- `domains/settings/` — `SettingsPage.svelte` only; reads providers + voice
  stores, no store of its own.

Transversal code: `lib/api.ts` (base resolution: bridge → env → `127.0.0.1:18080`;
`X-Sidecar-Token` gate + `Authorization: Bearer` user token; 30 s default
abort budget with per-endpoint overrides), `lib/theme.*`, `lib/i18n.*`,
`shared/` primitives. `sessionsError` was dropped in the split — written but
never read by any component.

## Routing & boot

Hash routes (`app/router.svelte.ts`): `#/login | #/register | #/settings[/section] | #/`.
`App.svelte`: `initLang(); initTheme();` → `auth.restore()` → `syncAuthRoute`
(the single auth-routing truth; unknown hashes redirect home once).
Settings is a route with a replace-hash section binding (one history entry per
visit, Back button works, `#/settings/1` deep-links the AI section).
`App.svelte` renders `Shell` for home/settings regardless of auth; `Shell`
re-checks the guard on mount.

## UI components (all Svelte, no second runtime)

`matrix-orb.svelte` (hand-ported canvas orb, dependency-free, reduced-motion
aware) and `bounce-sidebar.svelte` (nav list with WAAPI arc dot, same
contract as the Rare UI original it replaced). Positioning state lives in
`$state` (the template rewrites `style` wholesale, so imperative
`el.style.transform` writes get wiped — learned the hard way). Prod bundle
has no React: `react`/`react-dom`/`motion`/`@vitejs/plugin-react` removed,
`dist/` 464K → 132K.

## Sidecar contract (renderer view)

> Expansion (approved design, see `docs/12-expansion-design.md`): the 6-tool
> catalog grows to ~13 (`get_display_info`, `capture_screen`, `open_url`,
> `mouse_move/click/scroll`, `key_combo`; `type_text` tightens to 200/call +
> 1000/turn). Risk stays Low/Medium/High with a single `policy.allows(risk)`
> predicate (env `HARNESS_ALLOW_RISKY` OR the Settings risky-toggle) plus a
> deny-by-default confirmation gate for Medium/High and per-turn + per-session
> budgets. Single PROTOCOL 3 → 4 covers the union.

- Gate: `X-Sidecar-Token` on every `/v1/*`; `/health` open (liveness).
- Auth: `POST /v1/auth/register|login|refresh|logout`, `GET /v1/auth/me`,
  `DELETE /v1/auth/account`; `{error:{code,message}}` envelope
  (`src/native/README.md`, `src/lib/api.ts:66-73`).
- AI: `GET /v1/ai/providers` (live detection), `POST /v1/ai/select`,
  `GET /v1/ai/selection`, `POST /v1/ai/chat` (plain, no actions),
  `POST /v1/ai/run` (agentic; mutating tool calls → Action rows),
  `GET|POST /v1/ai/keys`, `DELETE /v1/ai/keys/{provider}`.
- Chat: `GET /v1/chat/sessions`, `GET|DELETE /v1/chat/sessions/{id}`
  (detail = session + messages + actions).
- Actions: `GET /v1/actions?session_id=`, `POST /v1/actions`,
  `PATCH /v1/actions/{id}`. Plain chat never creates actions.
- Voice: `GET /v1/voice/status`, `POST /v1/voice/listen|stop`,
  `GET /v1/voice/events?cursor=` (25 s long-poll). Transcripts are not
  persisted here — the renderer feeds them to the agent itself.
- Support: `GET /v1/support/diagnostics` (stderr mirror for the UI).
- Cloud: `GET /v1/cloud/status`, `POST /v1/cloud/sync` (present only as a
  feature; without Supabase config `status.enabled` is `false` and `sync`
  fails with a client error).

## Data & identity

- Local SQLite (`smartpc.db` in Electron `userData`, `/tmp/*.db` in dev).
  JWT secret derived per launch from the sidecar token → access tokens die
  with the process (sidecar README).
- Refresh tokens: opaque, hashed, rotated, reuse kills the chain. At rest
  they live in `<userData>/smartpc-vault.json` with values encrypted by
  `safeStorage` (OS keychain), via `vault:*` IPC (`main.cjs`, allow-listed in
  `preload.cjs`). Plain web uses localStorage (documented risk); a vault
  failure falls back rather than locking out; pre-vault tokens migrate once.
- Supabase (optional, `SUPABASE_*` env): Supabase Auth owns the credentials
  and Postgres mirrors sessions/messages/actions. The renderer never sees a
  key — the sidecar proxies auth and calls PostgREST with the user's own
  access token, so RLS decides visibility. `refresh_tokens.source` records
  which issuer a chain belongs to, so a device can hold both kinds; cloud
  accounts carry a `!supabase` password marker and a `cloud_accounts` row.
  Details: `docs/08-sync-design.md`, runbook `docs/11-supabase-cloud-runbook.md`.
