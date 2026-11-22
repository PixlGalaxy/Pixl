-- Playing position of the current track (time checkpoint), so a restart resumes where it was.
ALTER TABLE sessions ADD COLUMN position_ms INTEGER NOT NULL DEFAULT 0;
