-- Chat domain schema. Applied by ChatStore::open on every boot.
-- Everything hangs off users(id): deleting an account wipes its data.
CREATE TABLE IF NOT EXISTS chat_sessions (
  id          TEXT PRIMARY KEY,
  user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  title       TEXT NOT NULL,
  provider    TEXT NOT NULL DEFAULT 'ollama',
  model       TEXT,
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS chat_sessions_user_idx
  ON chat_sessions (user_id, updated_at DESC);

CREATE TABLE IF NOT EXISTS chat_messages (
  id          TEXT PRIMARY KEY,
  session_id  TEXT NOT NULL REFERENCES chat_sessions(id) ON DELETE CASCADE,
  role        TEXT NOT NULL,
  content     TEXT NOT NULL,
  created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS chat_messages_session_idx
  ON chat_messages (session_id, created_at);

CREATE TABLE IF NOT EXISTS actions (
  id          TEXT PRIMARY KEY,
  session_id  TEXT REFERENCES chat_sessions(id) ON DELETE CASCADE,
  user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  kind        TEXT NOT NULL,
  title       TEXT NOT NULL,
  status      TEXT NOT NULL DEFAULT 'running',
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS actions_session_idx ON actions (session_id, created_at);
CREATE INDEX IF NOT EXISTS actions_user_idx ON actions (user_id, created_at DESC);

CREATE TABLE IF NOT EXISTS ai_selection (
  user_id     TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
  provider    TEXT NOT NULL,
  model       TEXT,
  updated_at  TEXT NOT NULL
);

-- T4 D5: renderer-visible preferences (not secrets — consistent with
-- ai_selection living here too). `allow_risky_input` = "1"/"0".
CREATE TABLE IF NOT EXISTS prefs (
  user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  key         TEXT NOT NULL,
  value       TEXT NOT NULL,
  updated_at  TEXT NOT NULL,
  PRIMARY KEY (user_id, key)
);
