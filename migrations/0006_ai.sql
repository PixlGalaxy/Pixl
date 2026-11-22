-- Global track catalog. key = YouTube id, or 'isrc:<ISRC>' (then 'spotify:<id>' / 'url:<url>' as fallbacks).
CREATE TABLE IF NOT EXISTS tracks (
    id           INTEGER PRIMARY KEY,
    key          TEXT    NOT NULL UNIQUE,
    title        TEXT    NOT NULL,
    artists      TEXT    NOT NULL,        -- JSON array of strings
    url          TEXT    NOT NULL,
    duration_ms  INTEGER,
    isrc         TEXT,
    info         TEXT    NOT NULL,        -- latest TrackInfo JSON (to re-queue the track)
    tag_status   TEXT    NOT NULL DEFAULT 'pending' CHECK (tag_status IN ('pending', 'done', 'failed')),
    tag_attempts INTEGER NOT NULL DEFAULT 0,
    tagged_at    INTEGER,
    created_at   INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS tracks_isrc ON tracks (isrc) WHERE isrc IS NOT NULL;
CREATE INDEX IF NOT EXISTS tracks_tag_pending ON tracks (tag_attempts, created_at) WHERE tag_status = 'pending';

-- Normalized genres/tags per track.
CREATE TABLE IF NOT EXISTS track_tags (
    track_id INTEGER NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
    tag      TEXT    NOT NULL,
    weight   REAL    NOT NULL,
    source   TEXT    NOT NULL CHECK (source IN ('lastfm', 'musicbrainz', 'ollama')),
    PRIMARY KEY (track_id, tag)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS track_tags_tag ON track_tags (tag, weight);

-- Cached tags per artist (normalized name). Expired by fetched_at during maintenance.
CREATE TABLE IF NOT EXISTS artist_tags (
    artist_norm TEXT    NOT NULL,
    tag         TEXT    NOT NULL,
    weight      REAL    NOT NULL,
    source      TEXT    NOT NULL CHECK (source IN ('lastfm', 'musicbrainz', 'ollama')),
    fetched_at  INTEGER NOT NULL,
    PRIMARY KEY (artist_norm, tag)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS artist_tags_fetched_at ON artist_tags (fetched_at);

-- Distinct tracks per user (cap 2000, oldest last_played evicted).
CREATE TABLE IF NOT EXISTS user_history (
    user_id      INTEGER NOT NULL,
    track_id     INTEGER NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
    play_count   INTEGER NOT NULL DEFAULT 1,
    first_played INTEGER NOT NULL,
    last_played  INTEGER NOT NULL,
    PRIMARY KEY (user_id, track_id)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS user_history_recent ON user_history (user_id, last_played);
CREATE INDEX IF NOT EXISTS user_history_top ON user_history (user_id, play_count);
CREATE INDEX IF NOT EXISTS user_history_track ON user_history (track_id);

-- Like (+1) / dislike (-1).
CREATE TABLE IF NOT EXISTS user_feedback (
    user_id  INTEGER NOT NULL,
    track_id INTEGER NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
    value    INTEGER NOT NULL CHECK (value IN (-1, 1)),
    at       INTEGER NOT NULL,
    PRIMARY KEY (user_id, track_id)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS user_feedback_track ON user_feedback (track_id, value);

-- Genre vector per user. version bumps on every play/feedback; the background job writes
-- computed_version = version it read, so rows with version > computed_version need recomputing.
CREATE TABLE IF NOT EXISTS user_taste (
    user_id          INTEGER PRIMARY KEY NOT NULL,
    vector           TEXT,                -- JSON object {genre: weight}, NULL until first computed
    version          INTEGER NOT NULL DEFAULT 0,
    computed_version INTEGER NOT NULL DEFAULT 0,
    updated_at       INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS user_taste_dirty ON user_taste (user_id) WHERE version > computed_version;

-- Top-K similar users.
CREATE TABLE IF NOT EXISTS user_neighbors (
    user_id     INTEGER NOT NULL,
    neighbor_id INTEGER NOT NULL,
    score       REAL    NOT NULL,
    PRIMARY KEY (user_id, neighbor_id)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS user_neighbors_neighbor ON user_neighbors (neighbor_id);
