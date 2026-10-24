-- Ambush bites from fierce animals (combat::ambush_tick) notify the client with kind 'hurt'.
ALTER TABLE player_events
    DROP CONSTRAINT player_events_kind_check,
    ADD CONSTRAINT player_events_kind_check
        CHECK (kind IN (
            'level_up', 'say', 'whisper', 'trade', 'solar',
            'snare', 'escort', 'love', 'achievement', 'story', 'hurt'
        ));
