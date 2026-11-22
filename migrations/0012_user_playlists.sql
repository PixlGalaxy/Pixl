-- Playlists, albums and artist pages a user added (Playlists tab). Per user, not per guild.
-- Only the reference is stored (name, image, platform, canonical URL); the songs are fetched
-- again through the shared 60-minute cache when the list is opened or queued.
CREATE TABLE IF NOT EXISTS user_playlists (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id    INTEGER NOT NULL,
    url        TEXT    NOT NULL,          -- canonical URL (sources::lists::ListRef)
    platform   TEXT    NOT NULL,          -- 'spotify' | 'youtube' | 'youtube_music' | 'apple_music'
    name       TEXT    NOT NULL DEFAULT '',
    image_url  TEXT,                      -- upstream URL (rewritten to the proxies by the API)
    pinned     INTEGER NOT NULL DEFAULT 0,
    added_at   INTEGER NOT NULL,          -- unix seconds, first save
    used_at    INTEGER NOT NULL,          -- unix seconds, last time it was added or queued
    used_seq   INTEGER NOT NULL,          -- per-user counter: order of use (ties of used_at)
    UNIQUE (user_id, url)
);

CREATE INDEX IF NOT EXISTS user_playlists_order ON user_playlists (user_id, pinned, used_seq);
