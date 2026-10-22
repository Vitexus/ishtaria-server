-- Each wear-bearing item has its own max durability. Better pieces of the same kind
-- (Sword II, Claymore, metal armour, …) last longer; their damage or defence stays in
-- etc/creatures.json and etc/combat.json. Merchants charge more for those tiers.

UPDATE item_types SET max_durability = CASE id
    -- Tools (starter kit lasts a little less than mid-tier shop gear).
    WHEN 'axe' THEN 100
    WHEN 'pickaxe' THEN 100
    WHEN 'fishing_rod' THEN 80
    -- Weapons: damage rises with the creature hit table; durability follows the same ladder.
    WHEN 'knife' THEN 80
    WHEN 'dagger' THEN 100
    WHEN 'dagger_2' THEN 140
    WHEN 'axe_small' THEN 90
    WHEN 'cleaver' THEN 130
    WHEN 'axe_2' THEN 160
    WHEN 'axe_3' THEN 180
    WHEN 'axe_double' THEN 170
    WHEN 'spear' THEN 140
    WHEN 'sword' THEN 120
    WHEN 'sword_2' THEN 200
    WHEN 'trident' THEN 200
    WHEN 'talwar' THEN 220
    WHEN 'claymore' THEN 280
    WHEN 'devils_axe' THEN 300
    WHEN 'devils_sword' THEN 320
    -- Armour and shields: better defence pieces also take more punishment.
    WHEN 'glove' THEN 80
    WHEN 'armor_leather' THEN 150
    WHEN 'armor_golden' THEN 220
    WHEN 'armor_metal' THEN 300
    WHEN 'shield_round' THEN 120
    WHEN 'shield_heater' THEN 180
    WHEN 'shield_heater_2' THEN 200
    WHEN 'shield_round_2' THEN 200
    WHEN 'shield_celtic_golden' THEN 260
    ELSE max_durability
END
WHERE max_durability IS NOT NULL;

-- Pieces already worn past a newly lowered ceiling stay within the new max.
UPDATE player_inventory SET durability = item_types.max_durability
FROM item_types
WHERE player_inventory.item_id = item_types.id
  AND player_inventory.durability IS NOT NULL
  AND item_types.max_durability IS NOT NULL
  AND player_inventory.durability > item_types.max_durability;

UPDATE grave_inventory SET durability = item_types.max_durability
FROM item_types
WHERE grave_inventory.item_id = item_types.id
  AND grave_inventory.durability IS NOT NULL
  AND item_types.max_durability IS NOT NULL
  AND grave_inventory.durability > item_types.max_durability;
