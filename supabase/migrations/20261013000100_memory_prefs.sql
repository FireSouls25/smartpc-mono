-- Smart PC — cloud mirror, part 2: agent memory + renderer preferences.
--
-- memory_notes mirrors the local append-only note log (doc 14 §3) so a
-- second device adopts the agent's memories on login. Ids are the
-- sidecar's opaque hex ids (same convention as chat_*), so upserts are
-- idempotent with no mapping table. Append-only here too: rows are never
-- updated, only inserted (local deletes only happen on account wipe,
-- where the auth-user cascade owns the cleanup).
--
-- prefs mirrors the curated renderer preference keys (ui.*, voice.* — see
-- PREF_KEYS in the sidecar). The row id is "<user_id>:<key>" so the
-- sidecar's generic id-keyed upsert works unchanged. Last write wins per
-- key via updated_at (both sides compare with the shared remote_wins).
--
-- Apply with:  supabase db push           (CLI)
--          or: paste into the SQL editor / MCP execute_sql.

-- ---------------------------------------------------------------------------
-- memory_notes — mirrors local memory_notes.
-- ---------------------------------------------------------------------------
create table if not exists public.memory_notes (
  id         text primary key,
  user_id    uuid not null references auth.users (id) on delete cascade,
  text       text not null check (char_length(text) <= 280),
  topic      text,
  source     text not null default 'extracted',
  created_at timestamptz not null,
  updated_at timestamptz not null
);

create index if not exists memory_notes_user_updated_idx
  on public.memory_notes (user_id, updated_at desc);

-- ---------------------------------------------------------------------------
-- prefs — one row per (account, key). Secrets never land here: the
-- sidecar allowlists syncable UI/voice keys and refuses the rest.
-- ---------------------------------------------------------------------------
create table if not exists public.prefs (
  id         text primary key,
  user_id    uuid not null references auth.users (id) on delete cascade,
  key        text not null,
  value      text not null,
  updated_at timestamptz not null
);

create index if not exists prefs_user_idx
  on public.prefs (user_id);

-- ---------------------------------------------------------------------------
-- RLS: own rows only (same shape as the history tables).
-- ---------------------------------------------------------------------------
alter table public.memory_notes enable row level security;
alter table public.prefs           enable row level security;

drop policy if exists "own memory_notes" on public.memory_notes;
create policy "own memory_notes" on public.memory_notes
  for all
  to authenticated
  using ((select auth.uid()) = user_id)
  with check ((select auth.uid()) = user_id);

drop policy if exists "own prefs" on public.prefs;
create policy "own prefs" on public.prefs
  for all
  to authenticated
  using ((select auth.uid()) = user_id)
  with check ((select auth.uid()) = user_id);

grant select, insert, update, delete on
  public.memory_notes,
  public.prefs
  to authenticated;

revoke all on
  public.memory_notes,
  public.prefs
  from anon;
