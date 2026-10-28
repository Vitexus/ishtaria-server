-- A trapper may set as many snares as he carries: the one-snare-per-owner rule goes, the owner
-- index stays for the lookups.
DROP INDEX hunting_snares_one_per_owner;
CREATE INDEX hunting_snares_owner ON hunting_snares (owner_id);
