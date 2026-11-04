-- A character who is told of the gods' sky ships sees one fly low over them (story effect `sky_ship`): the client
-- draws it from this short-lived event, whose subject is the ship's id.
ALTER TABLE player_events
    DROP CONSTRAINT player_events_kind_check,
    ADD CONSTRAINT player_events_kind_check
        CHECK (kind IN (
            'level_up', 'say', 'whisper', 'trade', 'solar',
            'snare', 'escort', 'love', 'achievement', 'story', 'hurt', 'sky_flyover'
        ));
