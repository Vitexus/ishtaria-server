-- Achievements: milestones a character reaches once (the first tree felled, level 10, a first
-- friend). The catalog is etc/achievements.json; a row here means the character has earned it.
-- The primary key makes unlocking idempotent, and only a new row tells the owner, through an
-- 'achievement' event whose subject is the achievement id.

CREATE TABLE player_achievements (
    player_id BIGINT NOT NULL REFERENCES players(id) ON DELETE CASCADE,
    achievement TEXT NOT NULL CHECK (achievement ~ '^[a-z0-9_]{1,48}$'),
    unlocked_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (player_id, achievement)
);

ALTER TABLE player_events
    DROP CONSTRAINT player_events_kind_check,
    ADD CONSTRAINT player_events_kind_check
        CHECK (kind IN ('level_up', 'say', 'whisper', 'trade', 'solar', 'snare', 'escort', 'love', 'achievement'));
