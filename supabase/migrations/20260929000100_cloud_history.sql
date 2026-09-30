-- Smart PC — cloud schema (accounts + history).
--
-- Identity: Supabase Auth (auth.users) owns the accounts. The Rust sidecar
-- never stores cloud passwords; it authenticates against GoTrue and then
-- talks to PostgREST with the *user's* access token, so every row below is
-- protected by RLS against auth.uid() — not by app-side filtering.
--
-- History is a mirror of the local SQLite cache (local-first: the sidecar
-- writes locally first, then upserts here). Ids are the sidecar's opaque
-- hex ids kept as `text` so both sides can use the same primary key and an
-- upsert is idempotent without an id-mapping table.
--
-- Apply with:  supabase db push           (CLI)
--          or: paste into the SQL editor / MCP execute_sql.

-- ---------------------------------------------------------------------------
-- profiles — one row per account, created on signup.
-- ---------------------------------------------------------------------------
create table if not exists public.profiles (
  id           uuid primary key references auth.users (id) on delete cascade,
  display_name text,
  preferences  jsonb not null default '{}'::jsonb,
  created_at   timestamptz not null default now(),
  updated_at   timestamptz not null default now()
);

comment on table public.profiles is
  'Per-account settings. preferences mirrors the local stores: {theme, lang, provider, model, gestures}.';

-- ---------------------------------------------------------------------------
-- chat_sessions — one conversation. Mirrors local chat_sessions.
-- ---------------------------------------------------------------------------
create table if not exists public.chat_sessions (
  id         text primary key,
  user_id    uuid not null references auth.users (id) on delete cascade,
  title      text not null,
  provider   text not null default 'ollama',
  model      text,
  created_at timestamptz not null,
  updated_at timestamptz not null
);

-- Serves both the RLS predicate and the history list (newest first).
create index if not exists chat_sessions_user_updated_idx
  on public.chat_sessions (user_id, updated_at desc);

-- ---------------------------------------------------------------------------
-- chat_messages — mirrors local chat_messages. user_id is denormalized on
-- purpose: it keeps the RLS predicate a single indexed equality instead of a
-- subquery per row, and the only writer (the sidecar) always knows the owner.
-- ---------------------------------------------------------------------------
create table if not exists public.chat_messages (
  id         text primary key,
  session_id text not null references public.chat_sessions (id) on delete cascade,
  user_id    uuid not null references auth.users (id) on delete cascade,
  role       text not null check (role in ('user', 'assistant', 'tool')),
  content    text not null,
  created_at timestamptz not null
);

create index if not exists chat_messages_session_idx
  on public.chat_messages (session_id, created_at);

-- ---------------------------------------------------------------------------
-- actions — commands the agent actually executed on the machine (read-only
-- history on the web; execution always stays local).
-- ---------------------------------------------------------------------------
create table if not exists public.actions (
  id         text primary key,
  session_id text references public.chat_sessions (id) on delete cascade,
  user_id    uuid not null references auth.users (id) on delete cascade,
  kind       text not null,
  title      text not null,
  status     text not null default 'running'
               check (status in ('running', 'done', 'failed')),
  created_at timestamptz not null,
  updated_at timestamptz not null
);

create index if not exists actions_session_idx
  on public.actions (session_id, created_at);

create index if not exists actions_user_updated_idx
  on public.actions (user_id, updated_at desc);

-- ---------------------------------------------------------------------------
-- RLS. Every table: own rows only, read+write, enforced in the database.
-- `(select auth.uid())` is the cached form — it is evaluated once per query
-- instead of once per row.
-- ---------------------------------------------------------------------------
alter table public.profiles      enable row level security;
alter table public.chat_sessions enable row level security;
alter table public.chat_messages enable row level security;
alter table public.actions       enable row level security;

-- profiles: the row is keyed by the user id itself.
drop policy if exists "own profile" on public.profiles;
create policy "own profile" on public.profiles
  for all
  to authenticated
  using ((select auth.uid()) = id)
  with check ((select auth.uid()) = id);

-- chat tables: ownership through user_id.
drop policy if exists "own chat_sessions" on public.chat_sessions;
create policy "own chat_sessions" on public.chat_sessions
  for all
  to authenticated
  using ((select auth.uid()) = user_id)
  with check ((select auth.uid()) = user_id);

drop policy if exists "own chat_messages" on public.chat_messages;
create policy "own chat_messages" on public.chat_messages
  for all
  to authenticated
  using ((select auth.uid()) = user_id)
  with check ((select auth.uid()) = user_id);

drop policy if exists "own actions" on public.actions;
create policy "own actions" on public.actions
  for all
  to authenticated
  using ((select auth.uid()) = user_id)
  with check ((select auth.uid()) = user_id);

-- ---------------------------------------------------------------------------
-- Data API access. RLS decides *which rows*; these grants decide whether the
-- tables are reachable at all. anon gets nothing (the app always signs in).
-- ---------------------------------------------------------------------------
grant usage on schema public to authenticated;
grant select, insert, update, delete on
  public.profiles,
  public.chat_sessions,
  public.chat_messages,
  public.actions
  to authenticated;
revoke all on
  public.profiles,
  public.chat_sessions,
  public.chat_messages,
  public.actions
  from anon;

-- ---------------------------------------------------------------------------
-- New account -> empty profile row (upsert from the sidecar still works).
--
-- The function is SECURITY DEFINER (it must insert as its owner), so it
-- lives in a non-exposed schema with EXECUTE revoked: otherwise anyone
-- could call it over the Data API as postgres. `supabase db advisors`
-- flags exactly that, so keep it out of `public`.
-- ---------------------------------------------------------------------------
create schema if not exists private;

create or replace function private.handle_new_user()
returns trigger
language plpgsql
security definer
set search_path = ''
as $$
begin
  insert into public.profiles (id, created_at, updated_at)
  values (new.id, now(), now())
  on conflict (id) do nothing;
  return new;
end;
$$;

revoke execute on function private.handle_new_user()
  from public, anon, authenticated, service_role;

drop trigger if exists on_auth_user_created on auth.users;
create trigger on_auth_user_created
  after insert on auth.users
  for each row execute function private.handle_new_user();
