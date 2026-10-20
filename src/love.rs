//! Šamḫat's love minigame: throw hearts that leave short-lived love zones. Wild Enkidu
//! weaves around the water-hole; standing in a zone scores a hit. Ten hits and he will talk
//! (quest advances to bread / beer). Server owns every outcome.

use super::players::{self, Error};
use super::AppState;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::sync::OnceLock;

#[derive(Deserialize)]
struct Config {
    version: u32,
    quest_suffix: String,
    active_stage: String,
    won_stage: String,
    npc_suffix: String,
    hits_to_win: i32,
    zone_radius_m: f64,
    zone_ttl_seconds: f64,
    throw_cooldown_seconds: f64,
    throw_range_m: f64,
    arena_radius_m: f64,
    dodge_radius_m: f64,
    dodge_speed_rad_s: f64,
    won_flag: String,
}

fn config() -> &'static Config {
    static CONFIG: OnceLock<Config> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let config: Config = serde_json::from_str(include_str!("../etc/love.json"))
            .expect("bundled love must be valid");
        assert_eq!(config.version, 1);
        assert!((1..=20).contains(&config.hits_to_win));
        assert!((0.3..=3.0).contains(&config.zone_radius_m));
        assert!((3.0..=30.0).contains(&config.zone_ttl_seconds));
        assert!((0.2..=5.0).contains(&config.throw_cooldown_seconds));
        assert!((5.0..=80.0).contains(&config.throw_range_m));
        assert!((10.0..=120.0).contains(&config.arena_radius_m));
        assert!((4.0..=40.0).contains(&config.dodge_radius_m));
        assert!((0.1..=3.0).contains(&config.dodge_speed_rad_s));
        assert!(config.quest_suffix.starts_with(':'));
        assert!(config.npc_suffix.starts_with(':'));
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

fn length(v: [f64; 3]) -> f64 {
    distance_squared(v, [0.0, 0.0, 0.0]).sqrt()
}

fn unit(v: [f64; 3]) -> [f64; 3] {
    let len = length(v).max(1e-9);
    [v[0] / len, v[1] / len, v[2] / len]
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/story/love", get(status_get))
        .route("/story/love/throw", post(throw))
}

fn qualify_flag(quest_id: &str, flag: &str) -> String {
    match quest_id.split_once(':') {
        Some((disk, _)) if !flag.contains(':') => format!("{disk}:{flag}"),
        _ => flag.to_owned(),
    }
}

async fn notify(
    state: &AppState,
    player_id: i64,
    subject: &str,
    body: Option<String>,
) -> Result<(), Error> {
    sqlx::query(
        "INSERT INTO player_events (recipient_id, kind, subject, body) VALUES ($1, 'love', $2, $3)",
    )
    .bind(player_id)
    .bind(subject)
    .bind(body)
    .execute(&state.pool)
    .await?;
    Ok(())
}

/// Whether this NPC id is wild Enkidu for the love minigame.
pub(super) fn is_wild_enkidu(npc_id: &str) -> bool {
    npc_id.ends_with(&config().npc_suffix) && !npc_id.contains("civilized")
}

/// Active love session for a player (Enkidu dodge position), if any.
#[derive(Clone, Debug)]
pub(super) struct LoveActive {
    pub enkidu: [f64; 3],
}

#[derive(FromRow)]
struct LovePosRow {
    hits: i32,
    enkidu_x: Option<f64>,
    enkidu_y: Option<f64>,
    enkidu_z: Option<f64>,
}

#[derive(FromRow)]
struct LoveTickRow {
    player_id: i64,
    quest_id: String,
    hits: i32,
    dodge_phase: f64,
}

pub(super) async fn active_for(
    state: &AppState,
    player_id: i64,
) -> Result<Option<LoveActive>, Error> {
    let row: Option<LovePosRow> = sqlx::query_as(
        "SELECT hits, enkidu_x, enkidu_y, enkidu_z FROM player_love WHERE player_id = $1",
    )
    .bind(player_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let (Some(x), Some(y), Some(z)) = (row.enkidu_x, row.enkidu_y, row.enkidu_z) else {
        return Ok(None);
    };
    Ok(Some(LoveActive { enkidu: [x, y, z] }))
}

#[derive(FromRow)]
struct QuestRow {
    quest_id: String,
    stage: String,
    story_role: Option<String>,
    position_x: Option<f64>,
    position_y: Option<f64>,
    position_z: Option<f64>,
}

async fn load_props(state: &AppState, player_id: i64) -> Result<Option<QuestRow>, Error> {
    let suffix = format!("%{}", config().quest_suffix);
    sqlx::query_as(
        "SELECT q.quest_id, q.stage, p.story_role, p.position_x, p.position_y, p.position_z
         FROM player_quests q
         JOIN players p ON p.id = q.player_id
         WHERE q.player_id = $1 AND q.quest_id LIKE $2",
    )
    .bind(player_id)
    .bind(&suffix)
    .fetch_optional(&state.pool)
    .await
    .map_err(Error::from)
}

fn playing_as_shamhat(role: Option<&str>) -> bool {
    role.is_some_and(|r| r.ends_with(":shamhat") || r == "shamhat")
}

/// Centre of the water-hole arena in metres from the planet centre.
async fn arena_centre(state: &AppState) -> Result<Option<[f64; 3]>, Error> {
    let Ok(Some(world)) = super::story::world::world(state).await else {
        return Ok(None);
    };
    let site = world
        .sites
        .values()
        .find(|s| s.id.ends_with(":hunter_waterhole"));
    let Some(site) = site else {
        return Ok(None);
    };
    let r = super::movement::RADIUS + site.height_m;
    Ok(Some([
        site.direction[0] * r,
        site.direction[1] * r,
        site.direction[2] * r,
    ]))
}

fn dodge_position(centre: [f64; 3], phase: f64, radius_m: f64) -> [f64; 3] {
    // Local east/north on the tangent plane at centre.
    let up = unit(centre);
    let reference = if up[1].abs() < 0.9 {
        [0.0, 1.0, 0.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    let east = unit([
        up[1] * reference[2] - up[2] * reference[1],
        up[2] * reference[0] - up[0] * reference[2],
        up[0] * reference[1] - up[1] * reference[0],
    ]);
    let north = [
        up[1] * east[2] - up[2] * east[1],
        up[2] * east[0] - up[0] * east[2],
        up[0] * east[1] - up[1] * east[0],
    ];
    // Figure-eight weave: mix circle with a slowed second harmonic.
    let a = phase;
    let b = phase * 2.0;
    let local_e = radius_m * (a.cos() + 0.35 * b.sin());
    let local_n = radius_m * (a.sin() * 0.85);
    [
        centre[0] + east[0] * local_e + north[0] * local_n,
        centre[1] + east[1] * local_e + north[1] * local_n,
        centre[2] + east[2] * local_e + north[2] * local_n,
    ]
}

async fn ensure_session(
    state: &AppState,
    player_id: i64,
    quest_id: &str,
    centre: [f64; 3],
) -> Result<(), Error> {
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM player_love WHERE player_id = $1)")
            .bind(player_id)
            .fetch_one(&state.pool)
            .await?;
    if exists {
        return Ok(());
    }
    let pos = dodge_position(centre, 0.0, config().dodge_radius_m);
    sqlx::query(
        "INSERT INTO player_love (player_id, quest_id, hits, dodge_phase, enkidu_x, enkidu_y, enkidu_z)
         VALUES ($1, $2, 0, 0, $3, $4, $5)",
    )
    .bind(player_id)
    .bind(quest_id)
    .bind(pos[0])
    .bind(pos[1])
    .bind(pos[2])
    .execute(&state.pool)
    .await?;
    Ok(())
}

#[derive(Serialize)]
struct ZoneOut {
    id: i64,
    position: [f64; 3],
    /// Seconds remaining before the zone fades.
    ttl: f64,
}

#[derive(Serialize)]
struct LoveStatus {
    active: bool,
    hits: i32,
    hits_to_win: i32,
    zone_radius_m: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    enkidu: Option<[f64; 3]>,
    zones: Vec<ZoneOut>,
}

async fn status_get(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<LoveStatus>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let config = config();
    let props = load_props(&state, player_id).await?;
    let active = props.as_ref().is_some_and(|p| {
        p.stage == config.active_stage && playing_as_shamhat(p.story_role.as_deref())
    });
    if !active {
        return Ok(Json(LoveStatus {
            active: false,
            hits: 0,
            hits_to_win: config.hits_to_win,
            zone_radius_m: config.zone_radius_m,
            enkidu: None,
            zones: Vec::new(),
        }));
    }
    let props = props.unwrap();
    let Some(centre) = arena_centre(&state).await? else {
        return Err(status(StatusCode::CONFLICT, "love arena missing"));
    };
    ensure_session(&state, player_id, &props.quest_id, centre).await?;
    let row: Option<LovePosRow> = sqlx::query_as(
        "SELECT hits, enkidu_x, enkidu_y, enkidu_z FROM player_love WHERE player_id = $1",
    )
    .bind(player_id)
    .fetch_optional(&state.pool)
    .await?;
    let (hits, enkidu) = match row {
        Some(LovePosRow {
            hits: h,
            enkidu_x: Some(x),
            enkidu_y: Some(y),
            enkidu_z: Some(z),
        }) => (h, Some([x, y, z])),
        Some(LovePosRow { hits: h, .. }) => (h, None),
        None => (0, None),
    };
    let zones: Vec<(i64, f64, f64, f64, f64)> = sqlx::query_as(
        "SELECT id, position_x, position_y, position_z,
                ($2 - extract(epoch FROM (clock_timestamp() - created_at))::float8) AS ttl
         FROM love_zones WHERE player_id = $1
           AND created_at > clock_timestamp() - ($2 * interval '1 second')
         ORDER BY id",
    )
    .bind(player_id)
    .bind(config.zone_ttl_seconds)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(LoveStatus {
        active: true,
        hits,
        hits_to_win: config.hits_to_win,
        zone_radius_m: config.zone_radius_m,
        enkidu,
        zones: zones
            .into_iter()
            .filter(|z| z.4 > 0.0)
            .map(|z| ZoneOut {
                id: z.0,
                position: [z.1, z.2, z.3],
                ttl: z.4,
            })
            .collect(),
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThrowBody {
    x: f64,
    y: f64,
    z: f64,
}

#[derive(Serialize)]
struct ThrowReply {
    zone_id: i64,
    position: [f64; 3],
    hits: i32,
}

async fn throw(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ThrowBody>,
) -> Result<Json<ThrowReply>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let config = config();
    if ![body.x, body.y, body.z]
        .iter()
        .all(|v| v.is_finite() && v.abs() < 7_000_000.0)
    {
        return Err(status(StatusCode::BAD_REQUEST, "invalid aim"));
    }
    let Some(props) = load_props(&state, player_id).await? else {
        return Err(status(StatusCode::CONFLICT, "no love quest"));
    };
    if props.stage != config.active_stage {
        return Err(status(StatusCode::CONFLICT, "love already done"));
    }
    if !playing_as_shamhat(props.story_role.as_deref()) {
        return Err(status(StatusCode::FORBIDDEN, "not shamhat"));
    }
    let Some(px) = props.position_x else {
        return Err(status(StatusCode::CONFLICT, "player position unavailable"));
    };
    let player = [
        px,
        props.position_y.unwrap_or(0.0),
        props.position_z.unwrap_or(0.0),
    ];
    let aim = [body.x, body.y, body.z];
    if distance_squared(player, aim).sqrt() > config.throw_range_m {
        return Err(status(StatusCode::FORBIDDEN, "aim out of range"));
    }
    let Some(centre) = arena_centre(&state).await? else {
        return Err(status(StatusCode::CONFLICT, "love arena missing"));
    };
    if distance_squared(centre, aim).sqrt() > config.arena_radius_m {
        let _ = notify(&state, player_id, "miss", None).await;
        return Err(status(StatusCode::FORBIDDEN, "outside arena"));
    }
    ensure_session(&state, player_id, &props.quest_id, centre).await?;

    let cooldown: Option<f64> = sqlx::query_scalar(
        "SELECT extract(epoch FROM (clock_timestamp() - last_throw_at))::float8 FROM player_love WHERE player_id = $1",
    )
    .bind(player_id)
    .fetch_optional(&state.pool)
    .await?
    .flatten();
    if cooldown.is_some_and(|s| s < config.throw_cooldown_seconds) {
        return Err(status(StatusCode::TOO_MANY_REQUESTS, "throw cooldown"));
    }

    // Snap aim onto the planet surface at the aim direction.
    let dir = unit(aim);
    let height = if let Ok(terrain) = super::movement::terrain(&state).await {
        terrain.ground_height(dir).max(0.0)
    } else {
        length(aim) - super::movement::RADIUS
    };
    let land = [
        dir[0] * (super::movement::RADIUS + height),
        dir[1] * (super::movement::RADIUS + height),
        dir[2] * (super::movement::RADIUS + height),
    ];

    let mut tx = state.pool.begin().await?;
    let zone_id: i64 = sqlx::query_scalar(
        "INSERT INTO love_zones (player_id, position_x, position_y, position_z)
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(player_id)
    .bind(land[0])
    .bind(land[1])
    .bind(land[2])
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE player_love SET last_throw_at = now(), updated_at = now() WHERE player_id = $1",
    )
    .bind(player_id)
    .execute(&mut *tx)
    .await?;
    let hits: i32 = sqlx::query_scalar("SELECT hits FROM player_love WHERE player_id = $1")
        .bind(player_id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;

    notify(
        &state,
        player_id,
        "zone",
        Some(format!(
            r#"{{"id":{zone_id},"x":{},"y":{},"z":{},"ttl":{}}}"#,
            land[0], land[1], land[2], config.zone_ttl_seconds
        )),
    )
    .await?;

    Ok(Json(ThrowReply {
        zone_id,
        position: land,
        hits,
    }))
}

/// Advances dodge paths, scores zones, expires old ones, and completes the minigame.
pub(super) async fn tick(state: &AppState) -> Result<(), Error> {
    let config = config();
    let Some(centre) = arena_centre(state).await? else {
        return Ok(());
    };
    // Drop sessions whose quest left the active stage.
    let stale: Vec<i64> = sqlx::query_scalar(
        "SELECT l.player_id FROM player_love l
         JOIN players p ON p.id = l.player_id
         LEFT JOIN player_quests q ON q.player_id = l.player_id AND q.quest_id = l.quest_id
         WHERE p.world_id = $1 AND (q.stage IS NULL OR q.stage <> $2)",
    )
    .bind(state.world_id)
    .bind(&config.active_stage)
    .fetch_all(&state.pool)
    .await?;
    for id in stale {
        sqlx::query("DELETE FROM love_zones WHERE player_id = $1")
            .bind(id)
            .execute(&state.pool)
            .await?;
        sqlx::query("DELETE FROM player_love WHERE player_id = $1")
            .bind(id)
            .execute(&state.pool)
            .await?;
    }

    // Expire old zones.
    sqlx::query(
        "DELETE FROM love_zones WHERE created_at < clock_timestamp() - ($1 * interval '1 second')",
    )
    .bind(config.zone_ttl_seconds)
    .execute(&state.pool)
    .await?;

    // Ensure rows for players on the active stage as Šamḫat.
    let starters: Vec<(i64, String)> = sqlx::query_as(
        "SELECT p.id, q.quest_id FROM players p
         JOIN player_quests q ON q.player_id = p.id
         WHERE p.world_id = $1 AND p.died_at IS NULL AND q.stage = $2
           AND q.quest_id LIKE $3
           AND (p.story_role LIKE '%:shamhat' OR p.story_role = 'shamhat')",
    )
    .bind(state.world_id)
    .bind(&config.active_stage)
    .bind(format!("%{}", config.quest_suffix))
    .fetch_all(&state.pool)
    .await?;
    for (player_id, quest_id) in &starters {
        ensure_session(state, *player_id, quest_id, centre).await?;
    }

    let rows: Vec<LoveTickRow> = sqlx::query_as(
        "SELECT l.player_id, l.quest_id, l.hits, l.dodge_phase
         FROM player_love l
         JOIN players p ON p.id = l.player_id
         WHERE p.world_id = $1 AND p.died_at IS NULL",
    )
    .bind(state.world_id)
    .fetch_all(&state.pool)
    .await?;

    // ~5 s tick from main; advance phase by speed * dt with dt≈5.
    let dt = 5.0_f64;
    let radius2 = config.zone_radius_m * config.zone_radius_m;

    for row in rows {
        let LoveTickRow {
            player_id,
            quest_id,
            hits,
            dodge_phase: phase,
        } = row;
        let next_phase = phase + config.dodge_speed_rad_s * dt;
        let pos = dodge_position(centre, next_phase, config.dodge_radius_m);
        sqlx::query(
            "UPDATE player_love SET dodge_phase = $2, enkidu_x = $3, enkidu_y = $4, enkidu_z = $5, updated_at = now()
             WHERE player_id = $1",
        )
        .bind(player_id)
        .bind(next_phase)
        .bind(pos[0])
        .bind(pos[1])
        .bind(pos[2])
        .execute(&state.pool)
        .await?;

        let zones: Vec<(i64, f64, f64, f64)> = sqlx::query_as(
            "SELECT id, position_x, position_y, position_z FROM love_zones
             WHERE player_id = $1 AND NOT scored
               AND created_at > clock_timestamp() - ($2 * interval '1 second')",
        )
        .bind(player_id)
        .bind(config.zone_ttl_seconds)
        .fetch_all(&state.pool)
        .await?;

        let mut new_hits = hits;
        for (zone_id, zx, zy, zz) in zones {
            if distance_squared(pos, [zx, zy, zz]) > radius2 {
                continue;
            }
            let updated =
                sqlx::query("UPDATE love_zones SET scored = true WHERE id = $1 AND NOT scored")
                    .bind(zone_id)
                    .execute(&state.pool)
                    .await?
                    .rows_affected();
            if updated == 0 {
                continue;
            }
            new_hits = (new_hits + 1).min(config.hits_to_win);
            sqlx::query(
                "UPDATE player_love SET hits = $2, updated_at = now() WHERE player_id = $1",
            )
            .bind(player_id)
            .bind(new_hits)
            .execute(&state.pool)
            .await?;
            notify(
                state,
                player_id,
                "hit",
                Some(format!(
                    r#"{{"hits":{new_hits},"hits_to_win":{},"zone_id":{zone_id}}}"#,
                    config.hits_to_win
                )),
            )
            .await?;
            if new_hits >= config.hits_to_win {
                break;
            }
        }

        if new_hits >= config.hits_to_win {
            win(state, player_id, &quest_id).await?;
        }
    }
    Ok(())
}

async fn win(state: &AppState, player_id: i64, quest_id: &str) -> Result<(), Error> {
    let config = config();
    let mut tx = state.pool.begin().await?;
    let stage: Option<String> = sqlx::query_scalar(
        "SELECT stage FROM player_quests WHERE player_id = $1 AND quest_id = $2 FOR UPDATE",
    )
    .bind(player_id)
    .bind(quest_id)
    .fetch_optional(&mut *tx)
    .await?;
    if stage.as_deref() != Some(config.active_stage.as_str()) {
        tx.commit().await?;
        return Ok(());
    }
    sqlx::query(
        "UPDATE player_quests SET stage = $3, updated_at = now() WHERE player_id = $1 AND quest_id = $2",
    )
    .bind(player_id)
    .bind(quest_id)
    .bind(&config.won_stage)
    .execute(&mut *tx)
    .await?;
    let flag = qualify_flag(quest_id, &config.won_flag);
    sqlx::query(
        "INSERT INTO player_story_flags (player_id, flag) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(player_id)
    .bind(&flag)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM love_zones WHERE player_id = $1")
        .bind(player_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM player_love WHERE player_id = $1")
        .bind(player_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO player_events (recipient_id, kind, subject, body) VALUES ($1, 'love', 'won', $2)",
    )
    .bind(player_id)
    .bind(format!(
        r#"{{"stage":"{}"}}"#,
        config.won_stage
    ))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_rules_are_consistent() {
        let config = config();
        assert!(config.dodge_radius_m < config.arena_radius_m);
        assert!(config.zone_radius_m <= 1.0);
        assert!(is_wild_enkidu("gilgamesh:enkidu"));
        assert!(!is_wild_enkidu("gilgamesh:enkidu_civilized"));
    }

    #[test]
    fn dodge_stays_near_the_arena() {
        let centre = [super::super::movement::RADIUS, 0.0, 0.0];
        let pos = dodge_position(centre, 1.2, 18.0);
        let d = distance_squared(centre, pos).sqrt();
        assert!(d < 30.0, "d={d}");
    }
}
