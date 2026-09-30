# 08 — Supabase cloud (accounts + history)

Supabase is wired in. Supabase Auth owns the accounts and Postgres mirrors
the chat history; the local SQLite database stays the source of truth for
reads, so the app keeps working with no network at all.

> This file used to describe an unimplemented design. The design notes that
> mattered are kept below under "Decisions"; everything else now describes
> what the code does.

## Shape of the integration

```
  renderer (Svelte)  ──HTTP──▶  sidecar (Rust)  ──HTTPS──▶  Supabase
   no supabase client            GoTrue  +  PostgREST      Auth + Postgres
   no keys ever
```

The renderer talks **only** to the sidecar, exactly as before. The sidecar
proxies sign-in and holds the tokens, so a compromised renderer cannot mint
credentials, and no key is ever shipped to the page. `src/supabase/` (the old
`@supabase/supabase-js` module) is still not needed and is still absent —
the bundle stays free of it.

| Concern               | Where it lives                                            |
| --------------------- | --------------------------------------------------------- |
| Sign-up / sign-in     | `src/native/src/cloud/auth.rs` → GoTrue `/auth/v1`        |
| Access token for RLS  | in-memory in `cloud/mod.rs`, refreshed on every sign-in   |
| Local account mirror  | `users` + `cloud_accounts` tables (`auth/store.rs`)       |
| History write-through | `cloud/sync.rs`, queued per user                          |
| Pull / repair         | `cloud/routes.rs` → `POST /v1/cloud/sync`                 |
| Cloud schema + RLS    | `supabase/migrations/20260929000100_cloud_history.sql`    |
| UI                    | Settings → Account (`domains/auth/cloud.store.svelte.ts`) |

## Configuration

Read from the environment by the sidecar (Electron forwards it; see
`electron/main.cjs`), or from `supabase.json` in the project root /
`userData`:

```bash
SUPABASE_URL=https://<ref>.supabase.co   # or SUPABASE_PROJECT_REF=<ref>
SUPABASE_ANON_KEY=sb_publishable_…      # publishable or legacy anon key
SUPABASE_SERVICE_ROLE_KEY=…             # optional, delete-account only
SMARTPC_CLOUD=0                         # force off (tests, offline)
```

Without them the feature is off and **nothing else changes**: local
accounts, local history, no outbound requests. `npm run verify` and the
whole test suite run with `SMARTPC_CLOUD=0` forced, so a developer with the
variables exported still gets hermetic tests.

Apply the schema once:

```bash
supabase db push                      # CLI, with the migration in git
# or paste the file into the SQL editor
```

## What syncs

| Data                    | Direction               | When                                   |
| ----------------------- | ----------------------- | -------------------------------------- |
| Account (email, id)     | Supabase → local mirror | sign-in / refresh                      |
| Chat sessions           | write-through + pull    | every create/touch; pull on sync/login |
| Chat messages           | write-through + pull    | every turn (user, tool, assistant)     |
| Actions (executed cmds) | write-through + pull    | created, and on status change          |
| Chosen provider + model | both ways               | on select; adopted on a new device     |

Not synced, by design: **API keys** (they live in the OS keyring and are
machine-specific), audio, and anything the agent reads off the machine.

## Decisions worth keeping

- **Write-through, local-first.** A turn must never wait on the network, so
  every cloud write is queued (one worker per user, ordered), fire-and-forget,
  and a failure is recorded in `last_error` instead of surfacing. Reads always
  hit SQLite. Offline is a normal state, not an error.
- **Ids are shared, not translated.** Cloud tables key on the sidecar's own
  hex ids (`text`), so an upsert is idempotent with no id-mapping table.
- **RLS, not application filtering.** Every PostgREST call carries the
  user's own access token, so `auth.uid() = user_id` decides visibility in
  the database. A bug in the sidecar can lose a write; it cannot leak a row.
- **Last write wins per row**, by `updated_at`. Messages are immutable, so a
  history that forked on two devices keeps both copies rather than merging
  prose.
- **Timestamps are normalized on the way in.** Postgres answers
  `…+00:00`, SQLite orders TEXT: cloud rows are rewritten to the local
  `…Z` form or the session list sorts wrongly.
- **No password duplication** (this was the open problem in the old file):
  Supabase owns the credentials, the local `users` row keeps a `!supabase`
  marker, and password login refuses those rows with a distinct
  `cloud_account` error instead of "wrong password".

## Verified

Against a real project (`mgnlftlzmfchhbodcvmc`), then cleaned up:

- RLS isolation proven as the `authenticated` role: user A sees only its own
  sessions/messages/actions; a cross-tenant `UPDATE` and `INSERT` affect 0
  rows; `anon` is denied at the grant level.
- Sign-in through GoTrue → local mirror user → session created locally →
  appeared in Postgres under the right owner.
- A session that existed only in the cloud was pulled into SQLite (session +
  messages + actions) and its model choice adopted.
- Deleting a session locally removed it from the cloud, messages included
  (FK cascade).
- `supabase db advisors` → no security lints (the signup trigger lives in a
  non-exposed schema with `EXECUTE` revoked, which is what keeps it clean).

## Follow-ups

- `theme` / `lang` / `gestures` still live only in the renderer; pushing
  them needs a renderer-side call (`pushPreferences` in the old sketch).
- Email confirmation is on in the hosted project, so a new signup answers
  `202 {needs_confirmation: true}` and the login form shows "check your
  inbox" (i18n: `auth.confirmTitle`).
- Supabase has no self-service account delete: without
  `SUPABASE_SERVICE_ROLE_KEY`, "Delete account" removes the local rows and
  the cloud rows (FK cascade) but leaves the Auth user, which reappears on
  the next sign-in.
- An email already used by a **local** account blocks the cloud sign-in
  (`email_conflict`, 409) instead of merging the two: adopting the local row
  would attach one person's cloud history to another person's local data.
  Resolve it by signing in with the local password, or with another email.
