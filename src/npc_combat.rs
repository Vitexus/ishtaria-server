//! Per-player NPC health and hostility. Bars appear when an NPC is hurt. Friendly
//! escorts (Šamḫat) show green; NPCs the player struck show red and stay hostile until
//! an apology paid in gold. Hostile NPCs in range strike back until the player flees
//! or knocks them to defeat health.

use super::players::{self, Error};
use super::AppState;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use rand::{rngs::OsRng, Rng};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::sync::OnceLock;

#[derive(Deserialize)]
struct Config {
    version: u32,
    max_health: i32,
    strike_damage: i32,
    counter_damage: i32,
    counter_chance: f64,
    counter_cooldown_seconds: f64,
    engage_range_m: f64,
    strike_range_m: f64,
    apology_gold: i64,
    defeat_health: i32,
}

fn config() -> &'static Config {
    static CONFIG: OnceLock<Config> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let config: Config = serde_json::from_str(include_str!("../etc/npc_combat.json"))
            .expect("bundled npc_combat must be valid");
        assert_eq!(config.version, 1);
        assert!((1..=100).contains(&config.max_health));
        assert!((1..=config.max_health).contains(&config.strike_damage));
        assert!((1..=50).contains(&config.counter_damage));
        assert!((0.0..=1.0).contains(&config.counter_chance));
        assert!((0.5..=30.0).contains(&config.counter_cooldown_seconds));
        assert!((2.0..=30.0).contains(&config.engage_range_m));
        assert!((1.0..=config.engage_range_m).contains(&config.strike_range_m));
        assert!((1..=10_000).contains(&config.apology_gold));
        assert!((0..config.max_health).contains(&config.defeat_health));
        config
    })
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

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/story/npc/strike", post(strike))
        .route("/story/npc/apologize", post(apologize))
}

pub(super) fn apology_gold() -> i64 {
    config().apology_gold
}

/// Health bar announced on an NPC descriptor.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(super) struct HealthBar {
    pub current: i32,
    pub max: i32,
    /// `friend` (green), `foe` (red). Hidden when not visible.
    pub relation: String,
}

#[derive(FromRow)]
struct CombatRow {
    hostile: bool,
    health: i32,
    bar_visible: bool,
}

/// Loads the bar for one NPC from the player's point of view.
pub(super) async fn bar_for(
    state: &AppState,
    player_id: i64,
    npc_id: &str,
) -> Result<Option<HealthBar>, Error> {
    // Escort companion: green bar while hurt / threatened.
    if let Some(escort) = crate::escort::active_for(state, player_id).await? {
        if escort.companion_id == npc_id {
            let health: Option<i32> =
                sqlx::query_scalar("SELECT health FROM player_escorts WHERE player_id = $1")
                    .bind(player_id)
                    .fetch_optional(&state.pool)
                    .await?;
            if let Some(health) = health {
                let max = crate::escort::max_health();
                if health < max {
                    return Ok(Some(HealthBar {
                        current: health,
                        max,
                        relation: "friend".into(),
                    }));
                }
            }
        }
    }
    let row: Option<CombatRow> = sqlx::query_as(
        "SELECT hostile, health, bar_visible FROM player_npc_combat WHERE player_id = $1 AND npc_id = $2",
    )
    .bind(player_id)
    .bind(npc_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    if !row.bar_visible && !row.hostile {
        return Ok(None);
    }
    if !row.bar_visible && row.health >= config().max_health {
        return Ok(None);
    }
    Ok(Some(HealthBar {
        current: row.health,
        max: config().max_health,
        relation: if row.hostile {
            "foe".into()
        } else {
            "friend".into()
        },
    }))
}

pub(super) async fn is_hostile(
    state: &AppState,
    player_id: i64,
    npc_id: &str,
) -> Result<bool, Error> {
    let hostile: Option<bool> = sqlx::query_scalar(
        "SELECT hostile FROM player_npc_combat WHERE player_id = $1 AND npc_id = $2",
    )
    .bind(player_id)
    .bind(npc_id)
    .fetch_optional(&state.pool)
    .await?;
    Ok(hostile.unwrap_or(false))
}

/// Clears hostility and restores full health after a paid apology.
pub(super) async fn clear_hostility(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    player_id: i64,
    npc_id: &str,
) -> Result<(), Error> {
    sqlx::query(
        "UPDATE player_npc_combat SET hostile = false, health = $3, bar_visible = false, updated_at = now()
         WHERE player_id = $1 AND npc_id = $2",
    )
    .bind(player_id)
    .bind(npc_id)
    .bind(config().max_health)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrikeBody {
    npc_id: String,
}

#[derive(Serialize)]
struct StrikeReply {
    npc_id: String,
    health: i32,
    max_health: i32,
    hostile: bool,
    defeated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    counter: Option<CounterOut>,
}

#[derive(Serialize)]
struct CounterOut {
    damage: i32,
    died: bool,
}

async fn strike(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<StrikeBody>,
) -> Result<Json<StrikeReply>, Error> {
    let config = config();
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    if body.npc_id.is_empty() || body.npc_id.len() > 80 {
        return Err(status(StatusCode::BAD_REQUEST, "invalid npc"));
    }
    // Do not let the player beat their escorted friend.
    if let Some(escort) = crate::escort::active_for(&state, player_id).await? {
        if escort.companion_id == body.npc_id {
            return Err(status(StatusCode::FORBIDDEN, "cannot strike companion"));
        }
    }
    let Ok(Some(world)) = crate::story::world::world(&state).await else {
        return Err(status(StatusCode::SERVICE_UNAVAILABLE, "story unavailable"));
    };
    let Some(npc) = world.npcs.iter().find(|n| n.id == body.npc_id) else {
        return Err(status(StatusCode::NOT_FOUND, "npc not found"));
    };
    let now = crate::story::world::position_now(&state, npc).await;
    let position: Option<(f64, f64, f64)> = sqlx::query_as(
        "SELECT position_x, position_y, position_z FROM players WHERE id = $1 AND position_x IS NOT NULL",
    )
    .bind(player_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some(player) = position else {
        return Err(status(StatusCode::CONFLICT, "player position unavailable"));
    };
    let player = [player.0, player.1, player.2];
    if distance_squared(player, now.position).sqrt() > config.strike_range_m {
        return Err(status(StatusCode::FORBIDDEN, "npc out of reach"));
    }

    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO player_npc_combat (player_id, npc_id, hostile, health, bar_visible, updated_at)
         VALUES ($1, $2, true, $3, true, now())
         ON CONFLICT (player_id, npc_id) DO NOTHING",
    )
    .bind(player_id)
    .bind(&body.npc_id)
    .bind(config.max_health)
    .execute(&mut *tx)
    .await?;
    let health: i32 = sqlx::query_scalar(
        "UPDATE player_npc_combat SET
            hostile = true,
            health = greatest($3, health - $4),
            bar_visible = true,
            updated_at = now()
         WHERE player_id = $1 AND npc_id = $2
         RETURNING health",
    )
    .bind(player_id)
    .bind(&body.npc_id)
    .bind(config.defeat_health)
    .bind(config.strike_damage)
    .fetch_one(&mut *tx)
    .await?;
    let defeated = health <= config.defeat_health;
    let mut counter = None;
    // Only living hostiles within engage range strike back.
    if !defeated
        && distance_squared(player, now.position).sqrt() <= config.engage_range_m
        && OsRng.gen_bool(config.counter_chance)
    {
        let died = crate::survival::hurt(
            &state,
            &mut tx,
            player_id,
            config.counter_damage,
            crate::survival::DamageCause::HostilePlayer,
        )
        .await?;
        sqlx::query(
            "UPDATE player_npc_combat SET last_hit_at = now() WHERE player_id = $1 AND npc_id = $2",
        )
        .bind(player_id)
        .bind(&body.npc_id)
        .execute(&mut *tx)
        .await?;
        counter = Some(CounterOut {
            damage: config.counter_damage,
            died,
        });
        if died {
            let notice = crate::survival::obituary(&mut tx, player_id).await?;
            tx.commit().await?;
            return Err(Error::Obituary(Box::new(notice)));
        }
    }
    tx.commit().await?;
    Ok(Json(StrikeReply {
        npc_id: body.npc_id,
        health,
        max_health: config.max_health,
        hostile: true,
        defeated,
        counter,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApologizeBody {
    npc_id: String,
}

#[derive(Serialize)]
struct ApologizeReply {
    npc_id: String,
    gold_paid: i64,
}

async fn apologize(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ApologizeBody>,
) -> Result<Json<ApologizeReply>, Error> {
    let config = config();
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    if !is_hostile(&state, player_id, &body.npc_id).await? {
        return Err(status(StatusCode::CONFLICT, "npc not hostile"));
    }
    let mut tx = state.pool.begin().await?;
    let taken = sqlx::query(
        "UPDATE player_inventory SET quantity = quantity - $2
         WHERE player_id = $1 AND item_id = 'gold' AND quantity >= $2",
    )
    .bind(player_id)
    .bind(config.apology_gold)
    .execute(&mut *tx)
    .await?;
    if taken.rows_affected() != 1 {
        return Err(status(StatusCode::CONFLICT, "not enough gold"));
    }
    sqlx::query(
        "DELETE FROM player_inventory WHERE player_id = $1 AND item_id = 'gold' AND quantity = 0",
    )
    .bind(player_id)
    .execute(&mut *tx)
    .await?;
    clear_hostility(&mut tx, player_id, &body.npc_id).await?;
    tx.commit().await?;
    Ok(Json(ApologizeReply {
        npc_id: body.npc_id,
        gold_paid: config.apology_gold,
    }))
}

/// Hostile NPCs within engage range occasionally strike the player (self-defence).
pub(super) async fn tick(state: &AppState) -> Result<(), Error> {
    let config = config();
    let rows: Vec<(i64, String, i32, Option<f64>)> = sqlx::query_as(
        "SELECT c.player_id, c.npc_id, c.health,
                extract(epoch FROM (clock_timestamp() - c.last_hit_at))::float8
         FROM player_npc_combat c
         JOIN players p ON p.id = c.player_id
         WHERE p.world_id = $1 AND p.died_at IS NULL AND c.hostile AND c.health > $2",
    )
    .bind(state.world_id)
    .bind(config.defeat_health)
    .fetch_all(&state.pool)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let Ok(Some(world)) = crate::story::world::world(state).await else {
        return Ok(());
    };
    let engage2 = config.engage_range_m * config.engage_range_m;
    for (player_id, npc_id, _health, since) in rows {
        if since.is_some_and(|s| s < config.counter_cooldown_seconds) {
            continue;
        }
        let Some(npc) = world.npcs.iter().find(|n| n.id == npc_id) else {
            continue;
        };
        let now = crate::story::world::position_now(state, npc).await;
        let position: Option<(f64, f64, f64)> = sqlx::query_as(
            "SELECT position_x, position_y, position_z FROM players WHERE id = $1 AND position_x IS NOT NULL",
        )
        .bind(player_id)
        .fetch_optional(&state.pool)
        .await?;
        let Some(player) = position else {
            continue;
        };
        if distance_squared([player.0, player.1, player.2], now.position) > engage2 {
            continue;
        }
        if !OsRng.gen_bool(config.counter_chance) {
            continue;
        }
        let mut tx = state.pool.begin().await?;
        let died = crate::survival::hurt(
            state,
            &mut tx,
            player_id,
            config.counter_damage,
            crate::survival::DamageCause::HostilePlayer,
        )
        .await?;
        sqlx::query(
            "UPDATE player_npc_combat SET last_hit_at = now() WHERE player_id = $1 AND npc_id = $2",
        )
        .bind(player_id)
        .bind(&npc_id)
        .execute(&mut *tx)
        .await?;
        if died {
            let _ = crate::survival::obituary(&mut tx, player_id).await;
        }
        tx.commit().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_rules_are_consistent() {
        let config = config();
        assert!(config.strike_range_m <= config.engage_range_m);
        assert_eq!(config.apology_gold, 50);
    }
}
