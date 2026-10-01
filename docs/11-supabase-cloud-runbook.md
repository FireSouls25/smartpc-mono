# 08 — Supabase cloud: setup and operations

Companion to `08-sync-design.md` (what it is and why). This file is the
runbook: keys, applying the schema, verifying it, and turning it off.

## 1. Keys

Dashboard → Project Settings → API. Only the **publishable** key belongs in
your `.env`:

```bash
SUPABASE_URL=https://<project-ref>.supabase.co
SUPABASE_ANON_KEY=sb_publishable_…
```

`SUPABASE_SERVICE_ROLE_KEY` is optional and grants full access to the
project — it is only read by the sidecar, only for "Delete account", and
must never be committed. Leave it unset and account deletion stays local.

`.env` is picked up by `npm run dev:electron` (the dev runner calls
`process.loadEnvFile`). For a packaged build, drop a `supabase.json` with the
same keys next to the app or in Electron's `userData` directory; the
environment still wins.

## 2. Schema

```bash
supabase db push            # applies supabase/migrations/*.sql
```

Or paste `supabase/migrations/20260929000100_cloud_history.sql` into the SQL
editor. It creates `profiles`, `chat_sessions`, `chat_messages`, `actions`,
enables RLS on all four, and wires the signup trigger.

## 3. Verify

Fast checks that need nothing but the publishable key:

```bash
# The tables exist and reject anonymous access.
curl -s "$SUPABASE_URL/rest/v1/chat_sessions?select=id&limit=1" \
  -H "apikey: $SUPABASE_ANON_KEY" -H "Authorization: Bearer $SUPABASE_ANON_KEY"
# → {"code":"42501","message":"permission denied for table chat_sessions"}
```

Then, in the app:

1. `npm run dev:electron` — the sidecar prints
   `cloud: supabase auth + history mirror on (https://…)`.
2. Create an account in the app. If the project has **Confirm email** on
   (the default), the form switches to "check your inbox"; the link is
   required before the first sign-in.
3. Send a chat message, then look at Settings → Account: the cloud card
   shows enabled, the host, and the pushed/pulled counters.
4. Sign in from a second machine (or delete the local db): the history and
   the chosen model come back with the account.

Useful queries while debugging:

```sql
select count(*) from auth.users;        -- accounts
select count(*) from public.chat_sessions;
select * from public.profiles;          -- preferences per account
```

## 4. Operations

- **Force a repair.** Settings → Account → "Sync now" (`POST /v1/cloud/sync`):
  pulls the cloud into SQLite, then pushes the whole local cache back.
- **Logging out is safe to walk away from.** Logout drains the pending
  write-through queue first (up to ~20 s), so the last turns are in Supabase
  before the session ends. If the network is dead, logout still succeeds and
  the next login pushes what is left.
- **Turn it off** without uninstalling: set `SMARTPC_CLOUD=0`, or unset the
  key. The app falls back to local accounts and stops making requests.
- **Rotate the key** in the dashboard and update `.env` / `supabase.json`.
  Publishable keys can be rotated independently of the service role.
- **Email confirmation**: Authentication → Sign In / Providers → Email. Turn
  it off only for local testing; a confirmed-email flow is the secure default
  and the app already handles the 202 answer.

## 5. Local Supabase (optional)

For development against a local stack, point the same variables at it:

```bash
SUPABASE_URL=http://127.0.0.1:54321
SUPABASE_ANON_KEY=<the local anon key printed by `supabase start`>
```

The URL is used verbatim (no scheme is forced), so a plain-HTTP local stack
works. Apply the migration with `supabase db reset` or `supabase db push`
against the local instance.
