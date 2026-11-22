-- Per-guild settings. A missing row means defaults (everyone, vote off, 500, 100).
CREATE TABLE IF NOT EXISTS guild_settings (
    guild_id          INTEGER PRIMARY KEY NOT NULL,
    control_mode      TEXT    NOT NULL DEFAULT 'everyone' CHECK (control_mode IN ('everyone', 'dj')),
    vote_skip_enabled INTEGER NOT NULL DEFAULT 0,
    vote_skip_mode    TEXT    NOT NULL DEFAULT 'percent' CHECK (vote_skip_mode IN ('count', 'percent')),
    vote_skip_value   INTEGER NOT NULL DEFAULT 50,
    max_queue_length  INTEGER NOT NULL DEFAULT 500,
    default_volume    INTEGER NOT NULL DEFAULT 100,
    updated_at        INTEGER NOT NULL
);

-- DJs are plain user ids assigned from the panel (not Discord roles).
CREATE TABLE IF NOT EXISTS guild_djs (
    guild_id INTEGER NOT NULL,
    user_id  INTEGER NOT NULL,
    added_by INTEGER NOT NULL,
    added_at INTEGER NOT NULL,
    PRIMARY KEY (guild_id, user_id)
) WITHOUT ROWID;

-- Per-user preferences. A missing row means locale NULL and history enabled.
CREATE TABLE IF NOT EXISTS user_prefs (
    user_id         INTEGER PRIMARY KEY NOT NULL,
    locale          TEXT,
    history_enabled INTEGER NOT NULL DEFAULT 1,
    updated_at      INTEGER NOT NULL
);

-- Partial index: the few users that opted out (used to exclude them from neighbor queries).
CREATE INDEX IF NOT EXISTS user_prefs_history_off ON user_prefs (user_id) WHERE history_enabled = 0;
