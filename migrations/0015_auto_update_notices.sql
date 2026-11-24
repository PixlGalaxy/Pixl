-- Notices written by the automatic update checker (src/updates) carry a tag here, so the checker
-- finds, moves and removes its own notice without touching the ones made by hand (NULL).
-- Its settings are a JSON row in admin_flags (key auto_update).
ALTER TABLE admin_notices ADD COLUMN auto TEXT;
