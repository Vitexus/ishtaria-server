-- Hostile NPCs remember where they are while chasing this player (per-viewer pursuit).

ALTER TABLE player_npc_combat
    ADD COLUMN chase_x DOUBLE PRECISION,
    ADD COLUMN chase_y DOUBLE PRECISION,
    ADD COLUMN chase_z DOUBLE PRECISION;

COMMENT ON COLUMN player_npc_combat.chase_x IS
    'World metres: where this hostile NPC stands while pursuing the player; NULL until chase starts.';
