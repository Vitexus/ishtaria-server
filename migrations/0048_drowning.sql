-- A character in water drowns: health drains until they reach a dry shore or die.
ALTER TABLE players
    DROP CONSTRAINT players_death_cause_check,
    ADD CONSTRAINT players_death_cause_check CHECK (death_cause IN ('starvation', 'disease', 'fall', 'falling_tree', 'hostile_player', 'exhaustion', 'dehydration', 'creature', 'drowning'));
ALTER TABLE graves
    DROP CONSTRAINT graves_cause_check,
    ADD CONSTRAINT graves_cause_check CHECK (cause IN ('starvation', 'disease', 'fall', 'falling_tree', 'hostile_player', 'exhaustion', 'dehydration', 'creature', 'drowning'));
