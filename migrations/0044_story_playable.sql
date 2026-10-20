-- A story may hand the player another body (Tablet I hunter → Šamḫat → …). The pack
-- look stays in `character`; an optional datadisk model and the NPC id to hide while
-- the player wears that role live here.

ALTER TABLE players
    ADD COLUMN story_model TEXT
        CHECK (story_model IS NULL OR (
            char_length(story_model) BETWEEN 20 AND 120
            AND story_model LIKE 'media/models/%.glb'
        )),
    ADD COLUMN story_model_disk TEXT
        CHECK (story_model_disk IS NULL OR (
            char_length(story_model_disk) BETWEEN 2 AND 32
            AND story_model_disk ~ '^[a-z][a-z0-9_]*$'
        )),
    ADD COLUMN story_role TEXT
        CHECK (story_role IS NULL OR (
            char_length(story_role) BETWEEN 3 AND 80
            AND story_role LIKE '%:%'
        ));

ALTER TABLE players
    ADD CONSTRAINT players_story_model_disk_pair CHECK (
        (story_model IS NULL) = (story_model_disk IS NULL)
    );
