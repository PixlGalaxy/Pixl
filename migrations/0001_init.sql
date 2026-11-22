-- Key/value table for internal bookkeeping (e.g. hash of the registered slash commands).
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);
