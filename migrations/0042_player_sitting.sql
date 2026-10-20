-- A character may sit on a generated rock (world object id). While set, stamina recovers
-- faster than standing idle. Cleared on stand, walk intent or jump.
ALTER TABLE players
    ADD COLUMN sitting_object_id TEXT
    CHECK (sitting_object_id IS NULL OR (char_length(sitting_object_id) BETWEEN 1 AND 100));
