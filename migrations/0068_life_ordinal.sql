-- Which life under one login a character is (1 = the first). A login may be used again after a
-- permanent death; the new character is its next life, like the number after a ruler's name.
ALTER TABLE players ADD COLUMN ordinal BIGINT NOT NULL DEFAULT 1 CHECK (ordinal >= 1);
UPDATE players SET ordinal = numbered.n
    FROM (SELECT id, row_number() OVER (PARTITION BY world_id, lower(username) ORDER BY created_at, id) AS n
          FROM players) numbered
    WHERE numbered.id = players.id;

ALTER TABLE graves ADD COLUMN ordinal BIGINT NOT NULL DEFAULT 1 CHECK (ordinal >= 1);
UPDATE graves SET ordinal = players.ordinal FROM players WHERE players.id = graves.player_id;

CREATE TRIGGER permanent_grave_ordinal
BEFORE UPDATE OF ordinal ON graves
FOR EACH ROW EXECUTE FUNCTION preserve_memorial();
