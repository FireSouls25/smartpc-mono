-- Auth domain schema. Applied by Store::open on every boot (IF NOT EXISTS).
CREATE TABLE IF NOT EXISTS users (
  id            TEXT PRIMARY KEY,
  email         TEXT NOT NULL UNIQUE,
  password_hash TEXT NOT NULL,
  created_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS users_email_lower_idx ON users (lower(email));

CREATE TABLE IF NOT EXISTS refresh_tokens (
  id          TEXT PRIMARY KEY,
  user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  token_hash  TEXT NOT NULL UNIQUE,
  expires_at  TEXT NOT NULL,
  revoked_at  TEXT,
  created_at  TEXT NOT NULL,
  -- 'local' | 'cloud': who can renew this chain. Pre-migration databases
  -- lack the column; Store::open adds it (default 'local').
  source      TEXT NOT NULL DEFAULT 'local'
);
CREATE INDEX IF NOT EXISTS refresh_tokens_user_idx ON refresh_tokens (user_id);

-- Cloud accounts keep a marker instead of a password hash: Supabase Auth
-- owns the credentials, and password login must refuse these rows.
CREATE TABLE IF NOT EXISTS cloud_accounts (
  user_id     TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
  linked_at   TEXT NOT NULL
);
