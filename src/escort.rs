//! Escort companions on the road: fierce animals threaten Šamḫat; the player must
//! drive them off. Soft fail restarts the active story quest at its checkpoint
//! (see `story::checkpoint`) and notifies the client.

use super::players::Error;
use super::AppState;
use serde::Deserialize;
use sqlx::FromRow;
use std::sync::OnceLock;

#[derive(Deserialize)]
struct StageGate {
    quest_suffix: String,
    stage: String,
}

#[derive(Deserialize)]
struct Config {
    version: u32,
    companion_id_suffix: String,
    max_health: i32,
    damage_per_tick: i32,
    tick_interval_seconds: f64,
    threat_range_m: f64,
    protect_range_m: f64,
    follow_behind_m: f64,
    stages: Vec<StageGate>,
}

fn config() -> &'static Config {
    static CONFIG: OnceLock<Config> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let config: Config = serde_json::from_str(include_str!("../etc/escort.json"))
            .expect("bundled escort must be valid");
        assert_eq!(config.version, 1);
        assert!((1..=100).contains(&config.max_health));
        assert!((1..=config.max_health).contains(&config.damage_per_tick));
        assert!((1.0..=30.0).contains(&config.tick_interval_seconds));
        assert!((3.0..=40.0).contains(&config.threat_range_m));
        assert!((2.0..=config.threat_range_m).contains(&config.protect_range_m));
        assert!((0.5..=8.0).contains(&config.follow_behind_m));
        assert!(!config.stages.is_empty());
        assert!(config.companion_id_suffix.starts_with(':'));
        config
    })
}

fn distance_squared(first: [f64; 3], second: [f64; 3]) -> f64 {
    first
        .into_iter()
        .zip(second)
        .map(|(a, b)| (a - b).powi(2))
        .sum()
}

/// World position a few metres behind the player (flat offset).
pub(super) fn follow_position(player: [f64; 3]) -> [f64; 3] {
    let behind = config().follow_behind_m;
    let flat = behind / 2.0_f64.sqrt();
    [player[0] - flat, player[1], player[2] - flat]
}

/// Full companion health (matches the escort row default).
pub(super) fn max_health() -> i32 {
    config().max_health
}

/// Active escort row for a player, if any.
#[derive(Clone, Debug, FromRow)]
pub(super) struct EscortRow {
    pub companion_id: String,
}

/// Loads the escort row when the player is on a gated quest stage.
pub(super) async fn active_for(
    state: &AppState,
    player_id: i64,
) -> Result<Option<EscortRow>, Error> {
    sqlx::query_as("SELECT companion_id FROM player_escorts WHERE player_id = $1")
        .bind(player_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(Error::from)
}

async fn notify(
    state: &AppState,
    player_id: i64,
    subject: &str,
    body: Option<String>,
) -> Result<(), Error> {
    sqlx::query(
        "INSERT INTO player_events (recipient_id, kind, subject, body) VALUES ($1, 'escort', $2, $3)",
    )
    .bind(player_id)
    .bind(subject)
    .bind(body)
    .execute(&state.pool)
    .await?;
    Ok(())
}

/// Finds a gated quest stage for this player and the matching companion id in the world story.
async fn gated_stage(state: &AppState, player_id: i64) -> Result<Option<(String, String)>, Error> {
    let config = config();
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT quest_id, stage FROM player_quests WHERE player_id = $1")
            .bind(player_id)
            .fetch_all(&state.pool)
            .await?;
    for (quest_id, stage) in rows {
        for gate in &config.stages {
            if quest_id.ends_with(&gate.quest_suffix) && stage == gate.stage {
                return Ok(Some((quest_id, stage)));
            }
        }
    }
    Ok(None)
}

async fn resolve_companion_id(state: &AppState) -> Result<Option<String>, Error> {
    let suffix = &config().companion_id_suffix;
    let Ok(Some(world)) = super::story::world::world(state).await else {
        return Ok(None);
    };
    Ok(world
        .npcs
        .iter()
        .find(|npc| npc.id.ends_with(suffix.as_str()))
        .map(|npc| npc.id.clone()))
}

/// Ensures an escort row exists while the player is on a gated stage; drops it otherwise.
async fn sync_row(
    state: &AppState,
    player_id: i64,
    companion_id: &str,
) -> Result<Option<EscortRow>, Error> {
    let Some((_quest, _stage)) = gated_stage(state, player_id).await? else {
        let removed = sqlx::query("DELETE FROM player_escorts WHERE player_id = $1")
            .bind(player_id)
            .execute(&state.pool)
            .await?
            .rows_affected();
        if removed > 0 {
            notify(state, player_id, "ended", None).await?;
        }
        return Ok(None);
    };
    let existing: Option<EscortRow> =
        sqlx::query_as("SELECT companion_id FROM player_escorts WHERE player_id = $1")
            .bind(player_id)
            .fetch_optional(&state.pool)
            .await?;
    if let Some(row) = existing {
        if row.companion_id == companion_id {
            return Ok(Some(row));
        }
        sqlx::query("DELETE FROM player_escorts WHERE player_id = $1")
            .bind(player_id)
            .execute(&state.pool)
            .await?;
    }
    sqlx::query(
        "INSERT INTO player_escorts (player_id, companion_id, health) VALUES ($1, $2, $3)
         ON CONFLICT (player_id) DO UPDATE SET companion_id = EXCLUDED.companion_id, health = EXCLUDED.health,
           threatened_by = NULL, threat_since = NULL, last_tick_at = now()",
    )
    .bind(player_id)
    .bind(companion_id)
    .bind(config().max_health)
    .execute(&state.pool)
    .await?;
    notify(
        state,
        player_id,
        "started",
        Some(format!(r#"{{"companion":"{companion_id}"}}"#)),
    )
    .await?;
    active_for(state, player_id).await
}

/// Clears the current threat when the player strikes or kills that animal.
pub(super) async fn clear_threat(
    state: &AppState,
    player_id: i64,
    animal_id: &str,
) -> Result<(), Error> {
    let updated = sqlx::query(
        "UPDATE player_escorts SET threatened_by = NULL, threat_since = NULL, last_tick_at = now()
         WHERE player_id = $1 AND threatened_by = $2",
    )
    .bind(player_id)
    .bind(animal_id)
    .execute(&state.pool)
    .await?
    .rows_affected();
    if updated > 0 {
        notify(state, player_id, "protected", None).await?;
    }
    Ok(())
}

#[derive(FromRow)]
struct TickPlayer {
    player_id: i64,
    position_x: Option<f64>,
    position_y: Option<f64>,
    position_z: Option<f64>,
    story_role: Option<String>,
    seconds_since_tick: f64,
    health: i32,
    companion_id: String,
    threatened_by: Option<String>,
}

/// Advances every active escort: sync rows from quest stages, then threat / damage / fail.
pub(super) async fn tick(state: &AppState) -> Result<(), Error> {
    let config = config();
    let Some(companion_id) = resolve_companion_id(state).await? else {
        return Ok(());
    };
    // Sync: create escorts for players on gated stages; drop the rest for this world.
    let candidates: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT q.player_id FROM player_quests q
         JOIN players p ON p.id = q.player_id
         WHERE p.world_id = $1 AND p.died_at IS NULL",
    )
    .bind(state.world_id)
    .fetch_all(&state.pool)
    .await?;
    for player_id in &candidates {
        let _ = sync_row(state, *player_id, &companion_id).await?;
    }
    // Drop escorts whose players left every quest (sync_row covers gated exit).
    sqlx::query(
        "DELETE FROM player_escorts e USING players p
         WHERE e.player_id = p.id AND p.world_id = $1
           AND NOT EXISTS (
             SELECT 1 FROM player_quests q WHERE q.player_id = e.player_id
           )",
    )
    .bind(state.world_id)
    .execute(&state.pool)
    .await?;

    let rows: Vec<TickPlayer> = sqlx::query_as(
        "SELECT e.player_id, p.position_x, p.position_y, p.position_z, p.story_role,
                extract(epoch FROM (clock_timestamp() - e.last_tick_at))::float8 AS seconds_since_tick,
                e.health, e.companion_id, e.threatened_by
         FROM player_escorts e
         JOIN players p ON p.id = e.player_id
         WHERE p.world_id = $1 AND p.died_at IS NULL",
    )
    .bind(state.world_id)
    .fetch_all(&state.pool)
    .await?;

    let Ok(terrain) = super::movement::terrain(state).await else {
        return Ok(());
    };
    let now_ms = super::movement::unix_now_ms();
    let threat2 = config.threat_range_m * config.threat_range_m;
    let protect2 = config.protect_range_m * config.protect_range_m;

    for row in rows {
        // Still gated?
        if gated_stage(state, row.player_id).await?.is_none() {
            sqlx::query("DELETE FROM player_escorts WHERE player_id = $1")
                .bind(row.player_id)
                .execute(&state.pool)
                .await?;
            notify(state, row.player_id, "ended", None).await?;
            continue;
        }
        let Some(px) = row.position_x else {
            continue;
        };
        let Some(py) = row.position_y else {
            continue;
        };
        let Some(pz) = row.position_z else {
            continue;
        };
        let player = [px, py, pz];
        // Companion stands with the player unless the player *is* the companion (story_role).
        let as_player = row.story_role.as_deref() == Some(row.companion_id.as_str());
        let companion_pos = if as_player {
            player
        } else {
            follow_position(player)
        };

        // Resolve current threat animal, or pick the nearest fierce one in range.
        let mut threat_id = row.threatened_by.clone();
        let mut threat_pos: Option<[f64; 3]> = None;
        if let Some(id) = &threat_id {
            match terrain.animal_by_id(id, now_ms) {
                Some(animal) if super::combat::is_fierce(&animal.model) => {
                    threat_pos = Some(animal.position);
                }
                _ => {
                    threat_id = None;
                    sqlx::query(
                        "UPDATE player_escorts SET threatened_by = NULL, threat_since = NULL WHERE player_id = $1",
                    )
                    .bind(row.player_id)
                    .execute(&state.pool)
                    .await?;
                }
            }
        }
        if threat_id.is_none() {
            let mut best: Option<(String, [f64; 3], f64)> = None;
            for (animal, _) in terrain.moving_animals(companion_pos, now_ms) {
                if !super::combat::is_fierce(&animal.model) {
                    continue;
                }
                let d2 = distance_squared(animal.position, companion_pos);
                if d2 > threat2 {
                    continue;
                }
                let closer = match &best {
                    None => true,
                    Some((_, _, far)) => d2 < *far,
                };
                if closer {
                    best = Some((animal.id.clone(), animal.position, d2));
                }
            }
            if let Some((id, pos, _)) = best {
                sqlx::query(
                    "UPDATE player_escorts SET threatened_by = $2, threat_since = now(), last_tick_at = now()
                     WHERE player_id = $1",
                )
                .bind(row.player_id)
                .bind(&id)
                .execute(&state.pool)
                .await?;
                notify(
                    state,
                    row.player_id,
                    "threatened",
                    Some(format!(r#"{{"animal":"{id}"}}"#)),
                )
                .await?;
                threat_id = Some(id);
                threat_pos = Some(pos);
            }
        }

        let Some(animal_id) = threat_id else {
            continue;
        };
        let Some(animal_pos) = threat_pos else {
            continue;
        };

        // Player within protect range of the animal: holding them off, no damage this tick.
        if distance_squared(player, animal_pos) <= protect2 {
            sqlx::query("UPDATE player_escorts SET last_tick_at = now() WHERE player_id = $1")
                .bind(row.player_id)
                .execute(&state.pool)
                .await?;
            continue;
        }

        if row.seconds_since_tick < config.tick_interval_seconds {
            continue;
        }

        let next_health = (row.health - config.damage_per_tick).max(0);
        if next_health == 0 {
            // Soft-fail: restart the active story quest at its checkpoint (Uruk for the
            // hunter–Šamḫat road). Checkpoint clears the escort row.
            let mut tx = state.pool.begin().await?;
            let _ =
                crate::story::checkpoint::restart_active_quest(state, &mut tx, row.player_id, None)
                    .await?;
            tx.commit().await?;
            notify(
                state,
                row.player_id,
                "failed",
                Some(format!(
                    r#"{{"companion":"{}","as_player":{},"animal":"{}","quest_restart":true}}"#,
                    row.companion_id, as_player, animal_id
                )),
            )
            .await?;
        } else {
            sqlx::query(
                "UPDATE player_escorts SET health = $2, last_tick_at = now() WHERE player_id = $1",
            )
            .bind(row.player_id)
            .bind(next_health)
            .execute(&state.pool)
            .await?;
            notify(
                state,
                row.player_id,
                "hurt",
                Some(format!(r#"{{"health":{next_health}}}"#)),
            )
            .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_rules_are_consistent() {
        let config = config();
        assert!(config.protect_range_m <= config.threat_range_m);
        let companion = format!("gilgamesh{}", config.companion_id_suffix);
        assert!(companion.ends_with(&config.companion_id_suffix));
        assert!(!String::from("gilgamesh:enkidu").ends_with(&config.companion_id_suffix));
    }

    #[test]
    fn follow_stays_near_the_player() {
        let player = [100.0, 10.0, 200.0];
        let spot = follow_position(player);
        let d = distance_squared(player, spot).sqrt();
        assert!((d - config().follow_behind_m).abs() < 0.01);
    }
}
