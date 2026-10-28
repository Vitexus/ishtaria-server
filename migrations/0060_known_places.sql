-- Places a player has been sent to by a quest (or where one of his quests began) stay on his map and
-- compass after the quest has moved on: the hunter's water-hole is still there on the way to Uruk.
CREATE TABLE player_known_places (
    player_id BIGINT NOT NULL REFERENCES players(id) ON DELETE CASCADE,
    place_id TEXT NOT NULL CHECK (char_length(place_id) BETWEEN 1 AND 120),
    quest_id TEXT NOT NULL CHECK (char_length(quest_id) BETWEEN 1 AND 100),
    learned_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (player_id, place_id)
);
