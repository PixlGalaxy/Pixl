-- Generic on-disk cache for metadata (Spotify->YouTube matches, searches, tags, lyrics).
-- Entries expire (CACHE_TTL_DAYS, default 7) and are swept periodically.
CREATE TABLE IF NOT EXISTS cache (
    namespace  TEXT    NOT NULL,
    key        TEXT    NOT NULL,
    value      TEXT    NOT NULL,         -- JSON
    expires_at INTEGER NOT NULL,         -- unix seconds
    PRIMARY KEY (namespace, key)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS cache_expires_at ON cache (expires_at);
