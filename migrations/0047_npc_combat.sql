-- Per-player NPC combat: health bars and lasting hostility (cleared only by apology + gold).

CREATE TABLE player_npc_combat (
    player_id BIGINT NOT NULL REFERENCES players(id) ON DELETE CASCADE,
    npc_id TEXT NOT NULL CHECK (char_length(npc_id) BETWEEN 1 AND 80),
    -- Remembered after the player strikes the NPC; only an apology clears it.
    hostile BOOLEAN NOT NULL DEFAULT false,
    health INTEGER NOT NULL DEFAULT 100 CHECK (health >= 0 AND health <= 100),
    -- Show the health bar until the NPC is at full health and not in a fight.
    bar_visible BOOLEAN NOT NULL DEFAULT false,
    -- Last time a hostile NPC landed a blow (rate-limit counter-attacks).
    last_hit_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (player_id, npc_id)
);
CREATE INDEX player_npc_combat_hostile ON player_npc_combat (player_id) WHERE hostile;
