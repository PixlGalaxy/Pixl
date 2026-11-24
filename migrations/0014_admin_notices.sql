-- Operator notices set from the admin panel (scheduled downtime, info, warnings). Times are epoch
-- seconds (UTC). A notice is shown from show_from until ends_at while active = 1; the soonest one
-- is the one users see (Discord replies and the Activity banner).
CREATE TABLE IF NOT EXISTS admin_notices (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    kind       TEXT    NOT NULL CHECK (kind IN ('maintenance', 'info', 'warning')),
    title      TEXT    NOT NULL DEFAULT '',
    message    TEXT    NOT NULL DEFAULT '',
    starts_at  INTEGER NOT NULL,
    ends_at    INTEGER NOT NULL,
    show_from  INTEGER NOT NULL,
    active     INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Runtime switches set from the admin panel (maintenance mode, yt-dlp self-update pause).
-- No row = off.
CREATE TABLE IF NOT EXISTS admin_flags (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
