-- Whether node bot X currently has guild commands registered in guild Y (only where the main bot is absent).
CREATE TABLE IF NOT EXISTS node_guild_commands (
    node_app_id   INTEGER NOT NULL,
    guild_id      INTEGER NOT NULL,
    registered_at INTEGER NOT NULL,
    commands_hash TEXT    NOT NULL,
    PRIMARY KEY (node_app_id, guild_id)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS node_guild_commands_guild ON node_guild_commands (guild_id);
