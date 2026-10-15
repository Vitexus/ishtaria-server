-- A character that talks to a player stands still while they talk. Where a walking character is depends only on its id, its
-- place and the time (src/story/walking.rs), so a conversation stops the character's own clock instead of storing a position:
-- `shift_ms` is how long the character has stood still in earlier conversations, and the current stop lasts from `pause_from_ms`
-- to `pause_to_ms` (every answer keeps it open a few minutes longer; leaving the conversation ends it). The time of the walk is
-- `now - shift_ms - (the part of the current stop that has passed)`, so the character goes on from where it stood and does not jump.

CREATE TABLE npc_pauses (
    world_id BIGINT NOT NULL REFERENCES worlds(id) ON DELETE CASCADE,
    npc_id TEXT NOT NULL CHECK (char_length(npc_id) <= 80),
    shift_ms BIGINT NOT NULL DEFAULT 0 CHECK (shift_ms >= 0),
    pause_from_ms BIGINT NOT NULL,
    pause_to_ms BIGINT NOT NULL CHECK (pause_to_ms >= pause_from_ms),
    PRIMARY KEY (world_id, npc_id)
);
