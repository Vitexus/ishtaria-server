-- Travelers walk the trails between towns. When they pocket a dropped item they head for the
-- nearest merchant (preferring those roads), sell it into the merchant's stock, and the player
-- can then only buy it there — at a higher price than the finder asked.

CREATE TABLE merchant_stock (
    world_id BIGINT NOT NULL REFERENCES worlds (id) ON DELETE CASCADE,
    npc_id TEXT NOT NULL CHECK (char_length(npc_id) <= 80),
    item_id TEXT NOT NULL REFERENCES item_types (id),
    quantity BIGINT NOT NULL CHECK (quantity > 0),
    durability INTEGER CHECK (durability > 0),
    PRIMARY KEY (world_id, npc_id, item_id)
);

CREATE INDEX merchant_stock_npc ON merchant_stock (world_id, npc_id);

CREATE TABLE npc_errands (
    world_id BIGINT NOT NULL REFERENCES worlds (id) ON DELETE CASCADE,
    npc_id TEXT NOT NULL CHECK (char_length(npc_id) <= 80),
    merchant_id TEXT NOT NULL CHECK (char_length(merchant_id) <= 80),
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (world_id, npc_id)
);
