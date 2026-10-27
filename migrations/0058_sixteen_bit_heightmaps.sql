-- Height samples are 16-bit (two bytes each, big-endian, like the PGM) and already include the
-- local relief of the seed (ADR 0020). Strips stored with the old 8-bit encoding cannot be
-- converted, so such a database must be regenerated before the server starts.

DO $$
DECLARE
    old_check text;
BEGIN
    SELECT conname INTO old_check
    FROM pg_constraint
    WHERE conrelid = 'heightmaps'::regclass
      AND contype = 'c'
      AND pg_get_constraintdef(oid) LIKE '%octet_length(pixels)%';
    IF old_check IS NOT NULL THEN
        EXECUTE format('ALTER TABLE heightmaps DROP CONSTRAINT %I', old_check);
    END IF;
END $$;

ALTER TABLE heightmaps
    ADD CONSTRAINT heightmaps_pixels_length CHECK (octet_length(pixels) = 12 * face_size * face_size);
