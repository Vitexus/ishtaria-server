-- Which readable structures of the datadisks a character has read (POST /story/places/{id}/read).
-- A disk achievement may need a whole set of them (`read_all` of achievements.yaml), for example every
-- tablet of one legend. The place id is the qualified `<disk>:<place>`; the primary key makes reading
-- idempotent, so a second reading never counts twice.

CREATE TABLE player_place_reads (
    player_id BIGINT NOT NULL REFERENCES players(id) ON DELETE CASCADE,
    place_id TEXT NOT NULL CHECK (char_length(place_id) BETWEEN 1 AND 100),
    read_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (player_id, place_id)
);
