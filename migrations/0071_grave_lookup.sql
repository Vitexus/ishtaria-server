-- Memorials are asked for on every `/world/objects` request, which is the client's most frequent
-- call, and the query selected graves by a cone around the viewer's direction: both conditions
-- were expressions, so every grave in the world was evaluated each time. Graves are permanent by
-- design and grow with every death, so the cost grew with the age of the world and never fell.
--
-- `survival::memorials` now also bounds the Cartesian box around the point it is asking about,
-- which this index can answer.
CREATE INDEX graves_world_x ON graves (world_id, position_x);
