-- One row per active player (guild + voice channel), restored after a restart.
CREATE TABLE IF NOT EXISTS sessions (
    guild_id         INTEGER NOT NULL,
    voice_channel_id INTEGER NOT NULL,
    bot_user_id      INTEGER NOT NULL,
    text_channel_id  INTEGER NOT NULL,
    current_index    INTEGER NOT NULL DEFAULT 0,
    loop_mode        TEXT    NOT NULL DEFAULT 'off' CHECK (loop_mode IN ('off', 'track', 'queue')),
    volume           INTEGER NOT NULL DEFAULT 100,
    paused           INTEGER NOT NULL DEFAULT 0,
    started_by       INTEGER NOT NULL,
    ai_state         TEXT,                -- JSON, NULL when the AI radio is off
    updated_at       INTEGER NOT NULL,
    PRIMARY KEY (guild_id, voice_channel_id)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS sessions_bot ON sessions (bot_user_id);

-- Full track list of a session (played + upcoming), ordered by position.
CREATE TABLE IF NOT EXISTS session_tracks (
    guild_id         INTEGER NOT NULL,
    voice_channel_id INTEGER NOT NULL,
    position         INTEGER NOT NULL,
    uid              TEXT    NOT NULL,
    info             TEXT    NOT NULL,    -- TrackInfo JSON
    requested_by     INTEGER NOT NULL,
    PRIMARY KEY (guild_id, voice_channel_id, position),
    FOREIGN KEY (guild_id, voice_channel_id) REFERENCES sessions (guild_id, voice_channel_id) ON DELETE CASCADE
) WITHOUT ROWID;
