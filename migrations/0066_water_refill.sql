-- A water seller refills the waterskin for a few coins (shops.json: the pseudo-item `water_refill`). It is a thing the
-- shop lists and charges for, never an item of an inventory: buying it fills the buyer's waterskin (src/shops.rs).
INSERT INTO item_types (id, name, calories, capacity_bonus, stack_limit, multi_slot, slot_cost, category)
VALUES ('water_refill', 'Fill the waterskin', 0, 0, 1, false, 1, 'consumable');
