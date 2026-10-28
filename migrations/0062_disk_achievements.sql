-- Datadisks define achievements of their own next to the engine's (etc/achievements.json): their ids
-- are qualified with the disk, `<disk>:<id>` (for example `gilgamesh:foundation_reader`). The id keeps
-- fitting player_events.subject (64 characters), which announces a new achievement.

ALTER TABLE player_achievements
    DROP CONSTRAINT player_achievements_achievement_check,
    ADD CONSTRAINT player_achievements_achievement_check
        CHECK (achievement ~ '^([a-z0-9_]{1,48}:)?[a-z0-9_]{1,48}$' AND char_length(achievement) <= 64);
