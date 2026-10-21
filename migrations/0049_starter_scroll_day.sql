-- Every new character starts with one Scroll of Day so they can learn Daybreak
-- once they reach the spell's level requirement (craft remains the other source).

CREATE OR REPLACE FUNCTION starter_inventory() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO player_inventory (player_id, item_id, quantity) VALUES
        (NEW.id, 'apple', 3), (NEW.id, 'bread', 3), (NEW.id, 'cheese', 2), (NEW.id, 'carrot', 2),
        (NEW.id, 'gold', 100),
        (NEW.id, 'axe', 1), (NEW.id, 'pickaxe', 1), (NEW.id, 'sword', 1),
        (NEW.id, 'scroll_day', 1);
    INSERT INTO player_equipment (player_id, hand) VALUES (NEW.id, 'axe');
    RETURN NEW;
END;
$$;
