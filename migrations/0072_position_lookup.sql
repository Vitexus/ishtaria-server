-- Queries that ask "who or what is near this point" compared squared distances over three columns,
-- which no index can answer, so each of them read every row of its table. The range on the first
-- coordinate added beside those expressions is what these indexes serve; the distance still
-- decides the answer, the index only narrows what has to be looked at.
--
-- `players`: `/players/nearby` is polled every second by every client, and `chat::say` repeats the
-- same join for every spoken line.
CREATE INDEX players_world_x ON players (world_id, position_x);

-- `trades`: expired exchanges are deleted on every call that looks one up (`trade::find`).
CREATE INDEX trades_created ON trades (created_at);
