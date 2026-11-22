-- Panel appearance, so it follows the user across devices. NULL = the panel default.
ALTER TABLE user_prefs ADD COLUMN theme TEXT;   -- 'light' | 'dark' | 'system'
ALTER TABLE user_prefs ADD COLUMN accent TEXT;  -- accent palette id (validated by the API)
