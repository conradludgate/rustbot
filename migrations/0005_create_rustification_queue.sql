CREATE TABLE rustification_queue (
    guild_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    joined_at INTEGER,
    next_attempt_at INTEGER NOT NULL,
    PRIMARY KEY (guild_id, user_id)
);
CREATE INDEX rustification_queue_due ON rustification_queue (guild_id, next_attempt_at);
