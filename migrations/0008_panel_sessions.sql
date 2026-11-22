-- Activity panel sign-ins, so a restart or an update does not log everyone out.
-- Only the SHA-256 of the session token is stored (hex); the raw token never touches the disk.
CREATE TABLE IF NOT EXISTS panel_sessions (
    token_hash        TEXT    NOT NULL PRIMARY KEY,
    user_id           INTEGER NOT NULL,
    username          TEXT    NOT NULL,
    global_name       TEXT,
    avatar_url        TEXT,
    guild_id          INTEGER,
    launch_channel_id INTEGER,           -- channel the Activity was opened from (informational)
    created_at        INTEGER NOT NULL,
    expires_at        INTEGER NOT NULL
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS panel_sessions_expiry ON panel_sessions (expires_at);
