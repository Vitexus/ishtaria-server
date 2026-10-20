//! Hunter snares for Tablet I: place a snare, stand clear, wait for wildlife, then wild
//! Enkidu destroys the trap and the quest advances. The server owns every outcome.

use super::players::Error;
use super::AppState;
use axum::http::StatusCode;
use serde::Deserialize;
use sqlx::FromRow;
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Deserialize)]
struct StageAdvance {
    next: String,
    give_snare: bool,
    flag: String,
}

#[derive(Deserialize)]
struct Config {
    version: u32,
    quest_suffix: String,
    item_id: String,
    player_clearance_m: f64,
    clear_hold_seconds: f64,
    animal_approach_seconds: f64,
    stages: HashMap<String, StageAdvance>,
}

fn config() -> &'static Config {
    static CONFIG: OnceLock<Config> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let config: Config = serde_json::from_str(include_str!("../etc/traps.json"))
            .expect("bundled traps must be valid");
        assert_eq!(config.version, 1);
        assert!(config.player_clearance_m > 5.0 && config.player_clearance_m < 200.0);
        assert!(config.clear_hold_seconds > 1.0 && config.clear_hold_seconds < 120.0);
        assert!(config.animal_approach_seconds > 1.0 && config.animal_approach_seconds < 120.0);
        assert!(!config.stages.is_empty());
        config
    })
}

#[cfg(test)]
pub(super) fn configured_items() -> Vec<String> {
    vec![config().item_id.clone()]
}

fn status(code: StatusCode, message: &'static str) -> Error {
    Error::Status(code, message)
}

fn distance_squared(first: [f64; 3], second: [f64; 3]) -> f64 {
    first
        .into_iter()
        .zip(second)
        .map(|(a, b)| (a - b).powi(2))
        .sum()
}

fn qualify_flag(quest_id: &str, flag: &str) -> String {
    match quest_id.split_once(':') {
        Some((disk, _)) if !flag.contains(':') => format!("{disk}:{flag}"),
        _ => flag.to_owned(),
    }
}

/// Whether this item is the hunter snare.
pub(super) fn is_snare(item_id: &str) -> bool {
    item_id == config().item_id
}

/// The trapper may set a snare only while the hunter quest is on a trap stage, and only one
/// armed snare at a time.
pub(super) async fn may_place(state: &AppState, player_id: i64) -> Result<(), Error> {
    let config = config();
    let suffix = format!("%{}", config.quest_suffix);
    let stage: Option<String> = sqlx::query_scalar(
        "SELECT stage FROM player_quests WHERE player_id = $1 AND quest_id LIKE $2",
    )
    .bind(player_id)
    .bind(&suffix)
    .fetch_optional(&state.pool)
    .await?;
    let Some(stage) = stage else {
        return Err(status(StatusCode::CONFLICT, "no snare quest"));
    };
    if !config.stages.contains_key(&stage) {
        return Err(status(StatusCode::CONFLICT, "snares already done"));
    }
    let armed: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM hunting_snares WHERE owner_id = $1)")
            .bind(player_id)
            .fetch_one(&state.pool)
            .await?;
    if armed {
        return Err(status(StatusCode::CONFLICT, "snare already set"));
    }
    Ok(())
}

/// Arms a newly placed snare for the hunter wait / wildlife / Enkidu sequence.
pub(super) async fn arm(state: &AppState, player_id: i64, placed_id: i64) -> Result<(), Error> {
    let config = config();
    let suffix = format!("%{}", config.quest_suffix);
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT quest_id, stage FROM player_quests WHERE player_id = $1 AND quest_id LIKE $2",
    )
    .bind(player_id)
    .bind(&suffix)
    .fetch_optional(&state.pool)
    .await?;
    let Some((quest_id, stage)) = row else {
        return Err(status(StatusCode::CONFLICT, "no snare quest"));
    };
    if !config.stages.contains_key(&stage) {
        return Err(status(StatusCode::CONFLICT, "snares already done"));
    }
    sqlx::query(
        "INSERT INTO hunting_snares (placed_id, owner_id, quest_id, stage, phase) VALUES ($1, $2, $3, $4, 'watching')",
    )
    .bind(placed_id)
    .bind(player_id)
    .bind(&quest_id)
    .bind(&stage)
    .execute(&state.pool)
    .await?;
    notify(
        state,
        player_id,
        "armed",
        Some(format!(
            "{{\"placed_id\":{placed_id},\"clearance_m\":{}}}",
            config.player_clearance_m
        )),
    )
    .await?;
    Ok(())
}

async fn notify(
    state: &AppState,
    player_id: i64,
    subject: &str,
    body: Option<String>,
) -> Result<(), Error> {
    sqlx::query(
        "INSERT INTO player_events (recipient_id, kind, subject, body) VALUES ($1, 'snare', $2, $3)",
    )
    .bind(player_id)
    .bind(subject)
    .bind(body)
    .execute(&state.pool)
    .await?;
    Ok(())
}

#[derive(FromRow)]
struct SnareRow {
    placed_id: i64,
    owner_id: i64,
    quest_id: String,
    stage: String,
    phase: String,
    /// Seconds since clear_since, or null when not clearing yet.
    clear_for: Option<f64>,
    /// Seconds since animal_since, or null when no animal yet.
    animal_for: Option<f64>,
    position_x: f64,
    position_y: f64,
    position_z: f64,
    player_x: Option<f64>,
    player_y: Option<f64>,
    player_z: Option<f64>,
}

/// Advances every armed snare: clearance → animal approach → Enkidu destroys and quest moves on.
pub(super) async fn tick(state: &AppState) -> Result<(), Error> {
    let config = config();
    let rows: Vec<SnareRow> = sqlx::query_as(
        "SELECT s.placed_id, s.owner_id, s.quest_id, s.stage, s.phase,
                CASE WHEN s.clear_since IS NULL THEN NULL ELSE extract(epoch FROM (clock_timestamp() - s.clear_since))::float8 END AS clear_for,
                CASE WHEN s.animal_since IS NULL THEN NULL ELSE extract(epoch FROM (clock_timestamp() - s.animal_since))::float8 END AS animal_for,
                o.position_x, o.position_y, o.position_z,
                p.position_x AS player_x, p.position_y AS player_y, p.position_z AS player_z
         FROM hunting_snares s
         JOIN placed_objects o ON o.id = s.placed_id
         JOIN players p ON p.id = s.owner_id
         WHERE o.world_id = $1 AND p.died_at IS NULL",
    )
    .bind(state.world_id)
    .fetch_all(&state.pool)
    .await?;
    let clearance2 = config.player_clearance_m * config.player_clearance_m;
    for row in rows {
        let player_near = match (row.player_x, row.player_y, row.player_z) {
            (Some(x), Some(y), Some(z)) => {
                distance_squared([x, y, z], [row.position_x, row.position_y, row.position_z])
                    < clearance2
            }
            _ => true,
        };
        match row.phase.as_str() {
            "watching" => {
                if player_near {
                    if row.clear_for.is_some() {
                        sqlx::query(
                            "UPDATE hunting_snares SET clear_since = NULL WHERE placed_id = $1",
                        )
                        .bind(row.placed_id)
                        .execute(&state.pool)
                        .await?;
                        notify(state, row.owner_id, "too_close", None).await?;
                    }
                    continue;
                }
                if row.clear_for.is_none() {
                    sqlx::query(
                        "UPDATE hunting_snares SET clear_since = now() WHERE placed_id = $1",
                    )
                    .bind(row.placed_id)
                    .execute(&state.pool)
                    .await?;
                    continue;
                }
                if row.clear_for.unwrap_or(0.0) < config.clear_hold_seconds {
                    continue;
                }
                sqlx::query(
                    "UPDATE hunting_snares SET phase = 'animal', animal_since = now(), clear_since = NULL WHERE placed_id = $1",
                )
                .bind(row.placed_id)
                .execute(&state.pool)
                .await?;
                notify(
                    state,
                    row.owner_id,
                    "animal",
                    Some(format!(
                        "{{\"placed_id\":{},\"x\":{},\"y\":{},\"z\":{}}}",
                        row.placed_id, row.position_x, row.position_y, row.position_z
                    )),
                )
                .await?;
            }
            "animal" => {
                if player_near {
                    sqlx::query(
                        "UPDATE hunting_snares SET phase = 'watching', animal_since = NULL, clear_since = NULL WHERE placed_id = $1",
                    )
                    .bind(row.placed_id)
                    .execute(&state.pool)
                    .await?;
                    notify(state, row.owner_id, "animal_fled", None).await?;
                    continue;
                }
                if row.animal_for.unwrap_or(0.0) < config.animal_approach_seconds {
                    continue;
                }
                ruin(state, &row).await?;
            }
            _ => {}
        }
    }
    Ok(())
}

async fn ruin(state: &AppState, row: &SnareRow) -> Result<(), Error> {
    let config = config();
    let Some(advance) = config.stages.get(&row.stage) else {
        sqlx::query("DELETE FROM hunting_snares WHERE placed_id = $1")
            .bind(row.placed_id)
            .execute(&state.pool)
            .await?;
        sqlx::query("DELETE FROM placed_objects WHERE id = $1")
            .bind(row.placed_id)
            .execute(&state.pool)
            .await?;
        return Ok(());
    };
    let mut transaction = state.pool.begin().await?;
    let current: Option<String> = sqlx::query_scalar(
        "SELECT stage FROM player_quests WHERE player_id = $1 AND quest_id = $2 FOR UPDATE",
    )
    .bind(row.owner_id)
    .bind(&row.quest_id)
    .fetch_optional(&mut *transaction)
    .await?;
    if current.as_deref() != Some(row.stage.as_str()) {
        // Quest moved on elsewhere; drop the snare quietly.
        sqlx::query("DELETE FROM hunting_snares WHERE placed_id = $1")
            .bind(row.placed_id)
            .execute(&mut *transaction)
            .await?;
        sqlx::query("DELETE FROM placed_objects WHERE id = $1")
            .bind(row.placed_id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        return Ok(());
    }
    sqlx::query(
        "UPDATE player_quests SET stage = $3, updated_at = now() WHERE player_id = $1 AND quest_id = $2",
    )
    .bind(row.owner_id)
    .bind(&row.quest_id)
    .bind(&advance.next)
    .execute(&mut *transaction)
    .await?;
    let flag = qualify_flag(&row.quest_id, &advance.flag);
    sqlx::query(
        "INSERT INTO player_story_flags (player_id, flag) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(row.owner_id)
    .bind(&flag)
    .execute(&mut *transaction)
    .await?;
    if advance.give_snare {
        crate::gathering::add_items(
            &mut transaction,
            row.owner_id,
            &[(config.item_id.clone(), 1)],
        )
        .await?;
    }
    sqlx::query("DELETE FROM hunting_snares WHERE placed_id = $1")
        .bind(row.placed_id)
        .execute(&mut *transaction)
        .await?;
    sqlx::query("DELETE FROM placed_objects WHERE id = $1")
        .bind(row.placed_id)
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "INSERT INTO player_events (recipient_id, kind, subject, body) VALUES ($1, 'snare', 'ruined', $2)",
    )
    .bind(row.owner_id)
    .bind(format!(
        "{{\"placed_id\":{},\"x\":{},\"y\":{},\"z\":{},\"stage\":\"{}\"}}",
        row.placed_id, row.position_x, row.position_y, row.position_z, advance.next
    ))
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trap_config_loads() {
        let config = config();
        assert_eq!(config.item_id, "snare");
        assert!(config.stages.contains_key("lay_snares"));
        assert_eq!(config.stages["snare_ruined_2"].next, "snare_ruined_3");
    }

    #[test]
    fn flags_take_the_disk_prefix_from_the_quest() {
        assert_eq!(
            qualify_flag("gilgamesh:the_hunter_and_the_wild_man", "snare_ruined_once"),
            "gilgamesh:snare_ruined_once"
        );
        assert_eq!(
            qualify_flag("gilgamesh:q", "gilgamesh:already"),
            "gilgamesh:already"
        );
    }
}
