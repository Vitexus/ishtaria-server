-- Soft quest restart (story::checkpoint) notifies the client with kind 'story'.
ALTER TABLE player_events
    DROP CONSTRAINT player_events_kind_check,
    ADD CONSTRAINT player_events_kind_check
        CHECK (kind IN (
            'level_up', 'say', 'whisper', 'trade', 'solar',
            'snare', 'escort', 'love', 'achievement', 'story'
        ));
