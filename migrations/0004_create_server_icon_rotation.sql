CREATE TABLE IF NOT EXISTS server_icon_rotation
(
	guild_id          TEXT PRIMARY KEY NOT NULL,
	next_rotation_at  INTEGER NOT NULL,
	paused_at         INTEGER,
	pending_icon_path TEXT,
	current_icon_path TEXT
);
