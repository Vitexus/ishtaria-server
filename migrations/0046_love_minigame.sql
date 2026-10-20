-- Šamḫat love minigame (Tablet I): throw hearts, love zones tame wild Enkidu before beer.

ALTER TABLE player_events
    DROP CONSTRAINT player_events_kind_check,
    ADD CONSTRAINT player_events_kind_check
        CHECK (kind IN ('level_up', 'say', 'whisper', 'trade', 'solar', 'snare', 'escort', 'love'));

CREATE TABLE player_love (
    player_id BIGINT PRIMARY KEY REFERENCES players(id) ON DELETE CASCADE,
    quest_id TEXT NOT NULL CHECK (char_length(quest_id) BETWEEN 1 AND 80),
    hits INTEGER NOT NULL DEFAULT 0 CHECK (hits >= 0 AND hits <= 20),
    -- Phase along the dodge circle (radians), advanced by the tick.
    dodge_phase DOUBLE PRECISION NOT NULL DEFAULT 0,
    enkidu_x DOUBLE PRECISION,
    enkidu_y DOUBLE PRECISION,
    enkidu_z DOUBLE PRECISION,
    last_throw_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE love_zones (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    player_id BIGINT NOT NULL REFERENCES players(id) ON DELETE CASCADE,
    position_x DOUBLE PRECISION NOT NULL,
    position_y DOUBLE PRECISION NOT NULL,
    position_z DOUBLE PRECISION NOT NULL,
    -- A zone scores at most one hit.
    scored BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX love_zones_player ON love_zones (player_id);
