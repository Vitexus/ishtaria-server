-- Vehicles of datadisks (a boat of a structure with `vehicle`): the character that is aboard, and where a vehicle lies
-- when somebody has taken it out. A vehicle nobody has used stays where the datadisk puts it.
ALTER TABLE players ADD COLUMN vehicle_id TEXT CHECK (vehicle_id IS NULL OR char_length(vehicle_id) BETWEEN 3 AND 200);

CREATE TABLE vehicle_positions (
    world_id BIGINT NOT NULL REFERENCES worlds(id),
    vehicle_id TEXT NOT NULL CHECK (char_length(vehicle_id) BETWEEN 3 AND 200),
    position_x DOUBLE PRECISION NOT NULL,
    position_y DOUBLE PRECISION NOT NULL,
    position_z DOUBLE PRECISION NOT NULL,
    yaw DOUBLE PRECISION NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (world_id, vehicle_id)
);

-- One vehicle carries one character at a time.
CREATE UNIQUE INDEX players_one_per_vehicle ON players (world_id, vehicle_id) WHERE vehicle_id IS NOT NULL;
