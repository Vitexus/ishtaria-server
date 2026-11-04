-- A dead character no longer holds a vehicle: the one-rider rule applies to the living only.
DROP INDEX players_one_per_vehicle;
UPDATE players SET vehicle_id = NULL WHERE died_at IS NOT NULL;
CREATE UNIQUE INDEX players_one_per_vehicle ON players (world_id, vehicle_id)
    WHERE vehicle_id IS NOT NULL AND died_at IS NULL;
