-- Per-guild channel options set by managers in the panel. No row = the defaults below.
-- music_channel_id: every automatic Lynx message (now playing, notices) is sent there, and
--   commands and buttons only work there (plus the command channels, if any). NULL = off.
-- managers_exempt: managers ignore every channel rule (music, command and playback channels).
CREATE TABLE IF NOT EXISTS guild_channel_options (
    guild_id         INTEGER PRIMARY KEY,
    music_channel_id INTEGER,
    managers_exempt  INTEGER NOT NULL DEFAULT 1
);
