-- Items dropped on the ground stay there until a player picks them up or an NPC walks close
-- enough to pocket them. NPCs keep found goods in their own inventory and will trade them.

CREATE TABLE ground_items (
    id BIGSERIAL PRIMARY KEY,
    world_id BIGINT NOT NULL REFERENCES worlds (id) ON DELETE CASCADE,
    item_id TEXT NOT NULL REFERENCES item_types (id),
    quantity BIGINT NOT NULL CHECK (quantity > 0),
    durability INTEGER CHECK (durability > 0),
    position_x DOUBLE PRECISION NOT NULL,
    position_y DOUBLE PRECISION NOT NULL,
    position_z DOUBLE PRECISION NOT NULL,
    dropped_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX ground_items_area ON ground_items (world_id, position_x);

CREATE TABLE npc_inventory (
    world_id BIGINT NOT NULL REFERENCES worlds (id) ON DELETE CASCADE,
    npc_id TEXT NOT NULL,
    item_id TEXT NOT NULL REFERENCES item_types (id),
    quantity BIGINT NOT NULL CHECK (quantity > 0),
    durability INTEGER CHECK (durability > 0),
    PRIMARY KEY (world_id, npc_id, item_id)
);

CREATE INDEX npc_inventory_npc ON npc_inventory (world_id, npc_id);
