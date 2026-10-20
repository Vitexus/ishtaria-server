-- Escort companions (Tablet I–II): protect Šamḫat from fierce animals on the road.
-- Soft fail plays Die and resets companion health; the quest stage stays put.

ALTER TABLE player_events
    DROP CONSTRAINT player_events_kind_check,
    ADD CONSTRAINT player_events_kind_check
        CHECK (kind IN ('level_up', 'say', 'whisper', 'trade', 'solar', 'snare', 'escort')),
    DROP CONSTRAINT player_events_message_body,
    ADD CONSTRAINT player_events_message_body CHECK (
        (kind IN ('say', 'whisper') AND body IS NOT NULL)
        OR (kind NOT IN ('say', 'whisper'))
    );

CREATE TABLE player_escorts (
    player_id BIGINT PRIMARY KEY REFERENCES players(id) ON DELETE CASCADE,
    companion_id TEXT NOT NULL CHECK (char_length(companion_id) BETWEEN 1 AND 80),
    health INTEGER NOT NULL CHECK (health >= 0 AND health <= 100),
    threatened_by TEXT CHECK (threatened_by IS NULL OR char_length(threatened_by) BETWEEN 1 AND 120),
    threat_since TIMESTAMPTZ,
    last_tick_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX player_escorts_companion ON player_escorts (companion_id);
