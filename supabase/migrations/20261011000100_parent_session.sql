-- Rotation parent link (doc 14 §2): a successor row points at the frozen
-- row it continues. NULL for unrotated and pre-rotation rows. Mirrors the
-- local chat_sessions.parent_session_id (ON DELETE SET NULL there too, so
-- deleting a parent unlinks children instead of cascading them away).
alter table public.chat_sessions
  add column if not exists parent_session_id text
  references public.chat_sessions (id) on delete set null;

-- Chain walks (child → parent). No RLS change: the column rides the
-- existing user_id predicate, and only the sidecar writes it.
create index if not exists chat_sessions_parent_idx
  on public.chat_sessions (parent_session_id);
