-- Scrolls of Day and Night: reading one teaches a spell that turns the world's shared solar
-- clock to the next dawn or dusk. The shift is a per-world offset from the real clock that
-- `GET /world` adds to the announced solar time (the operator's --solar-offset-seconds comes
-- on top of it). Clients hear about a change through a `solar` event and ask for the world
-- again.

ALTER TABLE worlds
    ADD COLUMN solar_offset_seconds BIGINT NOT NULL DEFAULT 0
        CHECK (solar_offset_seconds >= -86400 AND solar_offset_seconds <= 86400);

INSERT INTO item_types (id, name, calories, capacity_bonus, stack_limit, multi_slot, slot_cost, category) VALUES
    ('scroll_day', 'Scroll of Day', 0, 0, 5, false, 1, 'document'),
    ('scroll_night', 'Scroll of Night', 0, 0, 5, false, 1, 'document');

ALTER TABLE player_events
    DROP CONSTRAINT player_events_kind_check,
    ADD CONSTRAINT player_events_kind_check CHECK (kind IN ('level_up', 'say', 'whisper', 'trade', 'solar'));
