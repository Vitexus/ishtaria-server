-- Quest checkpoints: when a quest changes its stage the server keeps where the character stood and what they carried.
-- A death past the first stage of a quest returns them there with the stage kept, and takes away what they gained
-- since (story/checkpoint.rs); a death without a quest or on its first stage leaves the usual memorial.
ALTER TABLE player_quests
    ADD COLUMN checkpoint_x DOUBLE PRECISION,
    ADD COLUMN checkpoint_y DOUBLE PRECISION,
    ADD COLUMN checkpoint_z DOUBLE PRECISION,
    -- When the stage last changed; NULL for rows from before this migration (no inventory was recorded for them).
    ADD COLUMN checkpoint_at TIMESTAMPTZ,
    ADD CONSTRAINT player_quests_checkpoint_whole CHECK (
        (checkpoint_x IS NULL) = (checkpoint_y IS NULL)
        AND (checkpoint_y IS NULL) = (checkpoint_z IS NULL)
    );

-- The inventory at the checkpoint of a quest.
CREATE TABLE player_quest_checkpoint_items (
    player_id BIGINT NOT NULL REFERENCES players(id) ON DELETE CASCADE,
    quest_id TEXT NOT NULL CHECK (char_length(quest_id) BETWEEN 3 AND 100),
    item_id TEXT NOT NULL REFERENCES item_types(id),
    quantity BIGINT NOT NULL CHECK (quantity > 0),
    PRIMARY KEY (player_id, quest_id, item_id)
);

-- One rule for every writer of a stage (dialogue effects, reached places, hunting, escorts).
CREATE OR REPLACE FUNCTION quest_checkpoint() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' OR NEW.stage IS DISTINCT FROM OLD.stage THEN
        SELECT position_x, position_y, position_z
          INTO NEW.checkpoint_x, NEW.checkpoint_y, NEW.checkpoint_z
          FROM players
         WHERE id = NEW.player_id
           AND position_x IS NOT NULL AND position_y IS NOT NULL AND position_z IS NOT NULL;
        NEW.checkpoint_at := now();
        DELETE FROM player_quest_checkpoint_items WHERE player_id = NEW.player_id AND quest_id = NEW.quest_id;
        INSERT INTO player_quest_checkpoint_items (player_id, quest_id, item_id, quantity)
            SELECT player_id, NEW.quest_id, item_id, quantity FROM player_inventory WHERE player_id = NEW.player_id;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER player_quests_checkpoint
    BEFORE INSERT OR UPDATE OF stage ON player_quests
    FOR EACH ROW EXECUTE FUNCTION quest_checkpoint();
