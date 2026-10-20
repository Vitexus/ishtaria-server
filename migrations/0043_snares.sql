-- Hunter snares: a placed snare waits until the trapper stands clear, then wildlife
-- approaches and wild Enkidu destroys it (Tablet I). State lives beside placed_objects.

INSERT INTO item_types (id, name, calories, capacity_bonus, stack_limit, multi_slot, slot_cost, category) VALUES
    ('snare', 'Snare', 0, 0, 5, false, 1, 'placeable');

CREATE TABLE hunting_snares (
    placed_id BIGINT PRIMARY KEY REFERENCES placed_objects(id) ON DELETE CASCADE,
    owner_id BIGINT NOT NULL REFERENCES players(id) ON DELETE CASCADE,
    quest_id TEXT NOT NULL CHECK (char_length(quest_id) BETWEEN 1 AND 80),
    stage TEXT NOT NULL CHECK (char_length(stage) BETWEEN 1 AND 40),
    -- watching: waiting for the trapper to stay clear; animal: wildlife approaching.
    phase TEXT NOT NULL CHECK (phase IN ('watching', 'animal')),
    clear_since TIMESTAMPTZ,
    animal_since TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX hunting_snares_one_per_owner ON hunting_snares (owner_id);
CREATE INDEX hunting_snares_phase ON hunting_snares (phase);
