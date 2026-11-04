//! Per-player NPC health and hostility. Bars appear when an NPC is hurt. Friendly
//! escorts (Šamḫat) show green; NPCs the player struck show red and stay hostile until
//! an apology paid in gold, the NPC is defeated, or the player dies. Hostile NPCs
//! chase the player and strike when in reach; a kill leaves a permanent memorial.

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

#[derive(Clone, Deserialize)]
struct NpcRules {
    max_health: Option<i32>,
    strike_damage: Option<i32>,
    counter_damage: Option<i32>,
    counter_chance: Option<f64>,
    counter_cooldown_seconds: Option<f64>,
    chase_range_m: Option<f64>,
    chase_speed_m_s: Option<f64>,
}

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
    chase_range_m: f64,
    chase_speed_m_s: f64,
    apology_gold: i64,
    defeat_health: i32,
    #[serde(default)]
    npcs: std::collections::BTreeMap<String, NpcRules>,
}

/// Resolved numbers for one strike (defaults, or a demigod like wild Enkidu).
struct Rules {
    max_health: i32,
    strike_damage: i32,
    counter_damage: i32,
    counter_chance: f64,
    counter_cooldown_seconds: f64,
    chase_range_m: f64,
    chase_speed_m_s: f64,
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
        assert!((config.engage_range_m..=500.0).contains(&config.chase_range_m));
        assert!((1.0..=20.0).contains(&config.chase_speed_m_s));
        assert!((1..=10_000).contains(&config.apology_gold));
        assert!((0..config.max_health).contains(&config.defeat_health));
        for (id, rules) in &config.npcs {
            let max = rules.max_health.unwrap_or(config.max_health);
            assert!((1..=500).contains(&max), "{id}");
            let strike = rules.strike_damage.unwrap_or(config.strike_damage);
            assert!((1..=max).contains(&strike), "{id}");
            let counter = rules.counter_damage.unwrap_or(config.counter_damage);
            assert!((1..=100).contains(&counter), "{id}");
            let chance = rules.counter_chance.unwrap_or(config.counter_chance);
            assert!((0.0..=1.0).contains(&chance), "{id}");
            let cool = rules
                .counter_cooldown_seconds
                .unwrap_or(config.counter_cooldown_seconds);
            assert!((0.5..=30.0).contains(&cool), "{id}");
            let chase = rules.chase_range_m.unwrap_or(config.chase_range_m);
            assert!((config.engage_range_m..=500.0).contains(&chase), "{id}");
            let speed = rules.chase_speed_m_s.unwrap_or(config.chase_speed_m_s);
            assert!((1.0..=20.0).contains(&speed), "{id}");
        }
        config
    })
}

fn rules_for(npc_id: &str) -> Rules {
    let config = config();
    // Story NPCs are announced as `disk:local_id` (e.g. gilgamesh:enkidu); overrides key on the local id.
    let over = config.npcs.get(npc_id).or_else(|| {
        npc_id
            .rsplit_once(':')
            .and_then(|(_, local)| config.npcs.get(local))
    });
    Rules {
        max_health: over.and_then(|r| r.max_health).unwrap_or(config.max_health),
        strike_damage: over
            .and_then(|r| r.strike_damage)
            .unwrap_or(config.strike_damage),
        counter_damage: over
            .and_then(|r| r.counter_damage)
            .unwrap_or(config.counter_damage),
        counter_chance: over
            .and_then(|r| r.counter_chance)
            .unwrap_or(config.counter_chance),
        counter_cooldown_seconds: over
            .and_then(|r| r.counter_cooldown_seconds)
            .unwrap_or(config.counter_cooldown_seconds),
        chase_range_m: over
            .and_then(|r| r.chase_range_m)
            .unwrap_or(config.chase_range_m),
        chase_speed_m_s: over
            .and_then(|r| r.chase_speed_m_s)
            .unwrap_or(config.chase_speed_m_s),
    }
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
    let rules = rules_for(npc_id);
    if !row.bar_visible && row.health >= rules.max_health {
        return Ok(None);
    }
    Ok(Some(HealthBar {
        current: row.health,
        max: rules.max_health,
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
        "UPDATE player_npc_combat SET hostile = false, health = $3, bar_visible = false,
            chase_x = NULL, chase_y = NULL, chase_z = NULL, updated_at = now()
         WHERE player_id = $1 AND npc_id = $2",
    )
    .bind(player_id)
    .bind(npc_id)
    .bind(rules_for(npc_id).max_health)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Where a hostile NPC stands for this viewer while chasing them, if any.
/// Also faces the player when their position is known.
pub(super) async fn chase_pose(
    state: &AppState,
    player_id: i64,
    npc_id: &str,
    player_pos: Option<[f64; 3]>,
) -> Result<Option<([f64; 3], f64)>, Error> {
    type ChaseRow = (Option<f64>, Option<f64>, Option<f64>, bool);
    let row: Option<ChaseRow> = sqlx::query_as(
        "SELECT chase_x, chase_y, chase_z, hostile FROM player_npc_combat
         WHERE player_id = $1 AND npc_id = $2",
    )
    .bind(player_id)
    .bind(npc_id)
    .fetch_optional(&state.pool)
    .await?;
    Ok(match row {
        Some((Some(x), Some(y), Some(z), true)) => {
            let at = [x, y, z];
            let yaw = player_pos
                .map(|player| yaw_toward(at, player))
                .unwrap_or(0.0);
            Some((at, yaw))
        }
        _ => None,
    })
}

fn step_toward(from: [f64; 3], to: [f64; 3], step_m: f64) -> [f64; 3] {
    let delta = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
    let dist = distance_squared(from, to).sqrt();
    if dist <= f64::EPSILON || step_m <= 0.0 {
        return from;
    }
    if step_m >= dist {
        return to;
    }
    let t = step_m / dist;
    [
        from[0] + delta[0] * t,
        from[1] + delta[1] * t,
        from[2] + delta[2] * t,
    ]
}

/// A counter-blow lands only if the NPC stood within reach when the tick began and still does after
/// its step. The step is a jump of up to `speed * dt` metres, so judging by the position after it
/// alone let a hostile NPC hit a player who had run far beyond its reach.
fn counter_in_reach(distance_before_m: f64, distance_after_m: f64, engage_range_m: f64) -> bool {
    distance_before_m <= engage_range_m && distance_after_m <= engage_range_m
}

fn yaw_toward(from: [f64; 3], to: [f64; 3]) -> f64 {
    let dx = to[0] - from[0];
    let dz = to[2] - from[2];
    if dx * dx + dz * dz <= f64::EPSILON {
        return 0.0;
    }
    dx.atan2(dz)
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
    let rules = rules_for(&body.npc_id);
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
    // Datadisk tag `friendly`: story allies (e.g. the hunter's father) never fight back.
    if world
        .story
        .npcs
        .get(&body.npc_id)
        .is_some_and(|def| def.tags.iter().any(|t| t == "friendly"))
    {
        return Err(status(StatusCode::FORBIDDEN, "cannot strike a friend"));
    }
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
        "INSERT INTO player_npc_combat (player_id, npc_id, hostile, health, bar_visible,
            chase_x, chase_y, chase_z, updated_at)
         VALUES ($1, $2, true, $3, true, $4, $5, $6, now())
         ON CONFLICT (player_id, npc_id) DO NOTHING",
    )
    .bind(player_id)
    .bind(&body.npc_id)
    .bind(rules.max_health)
    .bind(now.position[0])
    .bind(now.position[1])
    .bind(now.position[2])
    .execute(&mut *tx)
    .await?;
    let health: i32 = sqlx::query_scalar(
        "UPDATE player_npc_combat SET
            hostile = true,
            health = greatest($3, health - $4),
            bar_visible = true,
            chase_x = coalesce(chase_x, $5),
            chase_y = coalesce(chase_y, $6),
            chase_z = coalesce(chase_z, $7),
            updated_at = now()
         WHERE player_id = $1 AND npc_id = $2
         RETURNING health",
    )
    .bind(player_id)
    .bind(&body.npc_id)
    .bind(config.defeat_health)
    .bind(rules.strike_damage)
    .bind(now.position[0])
    .bind(now.position[1])
    .bind(now.position[2])
    .fetch_one(&mut *tx)
    .await?;
    let defeated = health <= config.defeat_health;
    if defeated {
        sqlx::query(
            "UPDATE player_npc_combat SET hostile = false, chase_x = NULL, chase_y = NULL, chase_z = NULL,
                updated_at = now()
             WHERE player_id = $1 AND npc_id = $2",
        )
        .bind(player_id)
        .bind(&body.npc_id)
        .execute(&mut *tx)
        .await?;
    }
    let mut counter = None;
    // Only living hostiles within engage range strike back.
    if !defeated
        && distance_squared(player, now.position).sqrt() <= config.engage_range_m
        && OsRng.gen_bool(rules.counter_chance)
    {
        let died = crate::survival::hurt(
            &state,
            &mut tx,
            player_id,
            rules.counter_damage,
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
            damage: rules.counter_damage,
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
        max_health: rules.max_health,
        hostile: !defeated,
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

/// Hostile NPCs chase the player and strike when close enough (self-defence pursuit).
pub(super) async fn tick(state: &AppState) -> Result<(), Error> {
    let config = config();
    type ChaseTick = (
        i64,
        String,
        i32,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
    );
    let rows: Vec<ChaseTick> = sqlx::query_as(
        "SELECT c.player_id, c.npc_id, c.health,
                extract(epoch FROM (clock_timestamp() - c.last_hit_at))::float8,
                extract(epoch FROM (clock_timestamp() - c.updated_at))::float8,
                c.chase_x, c.chase_y, c.chase_z
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
    // Survival loop runs every 5 s; clamp so a delayed tick does not teleport.
    const TICK_DT_S: f64 = 5.0;
    for (player_id, npc_id, _health, since_hit, since_update, cx, cy, cz) in rows {
        let rules = rules_for(&npc_id);
        let Some(npc) = world.npcs.iter().find(|n| n.id == npc_id) else {
            continue;
        };
        let home = crate::story::world::position_now(state, npc).await;
        let position: Option<(f64, f64, f64)> = sqlx::query_as(
            "SELECT position_x, position_y, position_z FROM players WHERE id = $1 AND position_x IS NOT NULL",
        )
        .bind(player_id)
        .fetch_optional(&state.pool)
        .await?;
        let Some(player) = position else {
            continue;
        };
        let player = [player.0, player.1, player.2];
        let mut chase = match (cx, cy, cz) {
            (Some(x), Some(y), Some(z)) => [x, y, z],
            _ => home.position,
        };
        let to_player = distance_squared(chase, player).sqrt();
        if to_player > rules.chase_range_m {
            // Too far: give up the chase and calm down.
            let mut tx = state.pool.begin().await?;
            clear_hostility(&mut tx, player_id, &npc_id).await?;
            tx.commit().await?;
            continue;
        }
        let dt = since_update
            .unwrap_or(TICK_DT_S)
            .clamp(0.5, TICK_DT_S * 1.5);
        if to_player > config.strike_range_m {
            let step = rules.chase_speed_m_s * dt;
            // Stop just inside strike reach so the next blow can land.
            let stop_at = config.strike_range_m * 0.85;
            let move_m = (to_player - stop_at).max(0.0).min(step);
            chase = step_toward(chase, player, move_m);
        }
        let mut tx = state.pool.begin().await?;
        sqlx::query(
            "UPDATE player_npc_combat SET chase_x = $3, chase_y = $4, chase_z = $5, updated_at = now()
             WHERE player_id = $1 AND npc_id = $2",
        )
        .bind(player_id)
        .bind(&npc_id)
        .bind(chase[0])
        .bind(chase[1])
        .bind(chase[2])
        .execute(&mut *tx)
        .await?;
        let in_reach = counter_in_reach(
            to_player,
            distance_squared(chase, player).sqrt(),
            config.engage_range_m,
        );
        let cooled = !since_hit.is_some_and(|s| s < rules.counter_cooldown_seconds);
        if in_reach && cooled && OsRng.gen_bool(rules.counter_chance) {
            let died = crate::survival::hurt(
                state,
                &mut tx,
                player_id,
                rules.counter_damage,
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
        let enkidu = rules_for("enkidu");
        assert_eq!(enkidu.max_health, 500);
        assert_eq!(enkidu.strike_damage, 3);
        assert_eq!(enkidu.counter_damage, 55);
        assert!((enkidu.counter_chance - 1.0).abs() < f64::EPSILON);
        assert!((enkidu.chase_speed_m_s - 6.0).abs() < f64::EPSILON);
        assert!((enkidu.chase_range_m - 200.0).abs() < f64::EPSILON);
        // Qualified story ids use the same override.
        assert_eq!(rules_for("gilgamesh:enkidu").max_health, 500);
        // Two counters drop a full-health hunter (100).
        assert!(enkidu.counter_damage * 2 > 100);
        // A fleeing player 25 m ahead is out of reach even though the NPC's step would close the gap.
        assert!(!counter_in_reach(25.0, 3.8, config.engage_range_m));
        assert!(counter_in_reach(5.0, 3.8, config.engage_range_m));
        let stepped = step_toward([0.0, 0.0, 0.0], [10.0, 0.0, 0.0], 4.0);
        assert!((stepped[0] - 4.0).abs() < 1e-9);
        assert!(
            (yaw_toward([0.0, 0.0, 0.0], [1.0, 0.0, 0.0]) - std::f64::consts::FRAC_PI_2).abs()
                < 1e-9
        );
    }
}
