-- The waterskin: a container item of about five litres that is filled at fresh water or a fountain and drunk from on
-- long journeys. One skin to a character (stack limit 1); its fill is kept in millilitres beside the inventory, and an
-- empty or missing row means an empty skin. The fill is not carried to graves, drops or trades: a skin that changes
-- hands starts empty for its new owner.
ALTER TABLE item_types ADD COLUMN capacity_ml INTEGER CHECK (capacity_ml > 0);

INSERT INTO item_types (id, name, calories, capacity_bonus, stack_limit, multi_slot, slot_cost, category, capacity_ml)
VALUES ('waterskin', 'Waterskin', 0, 0, 1, false, 1, 'container', 5000);

CREATE TABLE player_waterskin (
    player_id BIGINT PRIMARY KEY REFERENCES players(id) ON DELETE CASCADE,
    water_ml INTEGER NOT NULL DEFAULT 0 CHECK (water_ml BETWEEN 0 AND 5000)
);

-- A skin that leaves the inventory (dropped, traded, lost with the character) takes its water with it.
CREATE FUNCTION waterskin_gone() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.item_id = 'waterskin' AND (TG_OP = 'DELETE' OR NEW.quantity < 1) THEN
        DELETE FROM player_waterskin WHERE player_id = OLD.player_id;
    END IF;
    RETURN NULL;
END $$;
CREATE TRIGGER waterskin_gone AFTER UPDATE OR DELETE ON player_inventory
    FOR EACH ROW WHEN (OLD.item_id = 'waterskin') EXECUTE FUNCTION waterskin_gone();
