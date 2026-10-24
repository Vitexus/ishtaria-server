-- Demigods (wild Enkidu) may have more than 100 health; the original check capped at 100.
ALTER TABLE player_npc_combat DROP CONSTRAINT player_npc_combat_health_check;
ALTER TABLE player_npc_combat ADD CONSTRAINT player_npc_combat_health_check
    CHECK (health >= 0 AND health <= 500);
