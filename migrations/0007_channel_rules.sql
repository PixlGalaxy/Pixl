-- Optional per-guild channel restrictions set by managers in the panel. No rows = no restriction.
-- kind 'command': text channels (or a voice channel's chat) where music commands and buttons work.
-- kind 'voice': voice channels where Lynx may play.
CREATE TABLE IF NOT EXISTS guild_channel_rules (
    guild_id   INTEGER NOT NULL,
    kind       TEXT    NOT NULL CHECK (kind IN ('command', 'voice')),
    channel_id INTEGER NOT NULL,
    PRIMARY KEY (guild_id, kind, channel_id)
) WITHOUT ROWID;
