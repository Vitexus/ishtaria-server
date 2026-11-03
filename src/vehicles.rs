//! Vehicles of datadisks: a structure with `vehicle` (a fishing boat) that a character boards, steers over the water at
//! the vehicle's speed and leaves at a shore. The movement itself is the walk request (`movement::walk`) with the
//! vehicle's rules; this module boards, unboards and remembers where a vehicle was left.
//!
//! A vehicle nobody has used stays where its datadisk puts it. Aboard, it follows its rider (the server keeps its
//! position in `vehicle_positions`), and it lies where the rider got off.

use super::{movement, players, AppState};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use players::Error;
use serde::{Deserialize, Serialize};

/// How far from a vehicle a character may stand to board it, in metres.
pub(super) const BOARD_REACH_M: f64 = 8.0;
/// The shortest water a vehicle floats in: a boat grounds on anything shallower.
pub(super) const MIN_DEPTH_M: f64 = 0.5;
/// How far from a vehicle the nearest walkable shore is looked for when its rider gets off, in metres.
const LANDING_REACH_M: f64 = 12.0;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/players/me/vehicle/board", post(board))
        .route("/players/me/vehicle/leave", post(leave))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Board {
    id: String,
}

#[derive(Serialize)]
struct Boarded {
    /// The vehicle the character is aboard now, `None` after leaving.
    vehicle: Option<String>,
    position: [f64; 3],
}

/// What the world knows of a vehicle: where it lies (where its rider left it, else where its disk put it), its speed
/// and the rider's clip.
pub(super) struct Found {
    pub position: [f64; 3],
}

/// The vehicle prop of a world by its place id, with the position it has now.
pub(super) async fn find(state: &AppState, id: &str) -> Option<Found> {
    let world = super::story::world::world(state).await.ok().flatten()?;
    let prop = world
        .props
        .iter()
        .find(|prop| prop.id == id && prop.vehicle.is_some())?;
    let stored: Option<(f64, f64, f64)> = sqlx::query_as(
        "SELECT position_x, position_y, position_z FROM vehicle_positions WHERE world_id = $1 AND vehicle_id = $2",
    )
    .bind(state.world_id)
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();
    Some(Found {
        position: stored.map_or(prop.position, |(x, y, z)| [x, y, z]),
    })
}

/// The speed of the vehicle a character is aboard, when they are aboard one.
pub(super) async fn speed_of(
    state: &AppState,
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    player_id: i64,
) -> Result<Option<(String, f64)>, Error> {
    let vehicle: Option<String> =
        sqlx::query_scalar("SELECT vehicle_id FROM players WHERE id = $1")
            .bind(player_id)
            .fetch_one(&mut **transaction)
            .await?;
    let Some(id) = vehicle else {
        return Ok(None);
    };
    let speed = super::story::world::world(state)
        .await
        .ok()
        .flatten()
        .and_then(|world| {
            world
                .props
                .iter()
                .find(|prop| prop.id == id)
                .and_then(|prop| prop.vehicle.as_ref().map(|vehicle| vehicle.speed_m_s))
        });
    // A vehicle the disk no longer has lets its rider off on the next step.
    Ok(speed.map(|speed| (id, speed)))
}

/// Remembers where a vehicle is.
pub(super) async fn remember(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    world_id: i64,
    vehicle_id: &str,
    point: [f64; 3],
    yaw: f64,
) -> Result<(), Error> {
    sqlx::query("INSERT INTO vehicle_positions (world_id, vehicle_id, position_x, position_y, position_z, yaw) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (world_id, vehicle_id) DO UPDATE SET position_x = EXCLUDED.position_x, position_y = EXCLUDED.position_y, position_z = EXCLUDED.position_z, yaw = EXCLUDED.yaw, updated_at = now()")
        .bind(world_id).bind(vehicle_id).bind(point[0]).bind(point[1]).bind(point[2]).bind(yaw)
        .execute(&mut **transaction).await?;
    Ok(())
}

/// Where the vehicles lie now, for the props the clients are told about.
pub(super) async fn positions(state: &AppState) -> Vec<(String, [f64; 3], f64)> {
    sqlx::query_as::<_, (String, f64, f64, f64, f64)>(
        "SELECT vehicle_id, position_x, position_y, position_z, yaw FROM vehicle_positions WHERE world_id = $1",
    )
    .bind(state.world_id)
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(id, x, y, z, yaw)| (id, [x, y, z], yaw))
    .collect()
}

async fn board(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<Board>,
) -> Result<Json<Boarded>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let vehicle = find(&state, &request.id)
        .await
        .ok_or(Error::Status(StatusCode::NOT_FOUND, "no such vehicle"))?;
    let mut transaction = state.pool.begin().await?;
    if !super::survival::lock_alive(&state, &mut transaction, player_id).await? {
        return Err(Error::Status(StatusCode::GONE, "player is dead"));
    }
    let (x, y, z, airborne, aboard): (Option<f64>, Option<f64>, Option<f64>, bool, Option<String>) = sqlx::query_as(
        "SELECT position_x, position_y, position_z, movement_airborne, vehicle_id FROM players WHERE id = $1 FOR UPDATE",
    )
    .bind(player_id)
    .fetch_one(&mut *transaction)
    .await?;
    let (Some(x), Some(y), Some(z)) = (x, y, z) else {
        return Err(Error::Status(
            StatusCode::CONFLICT,
            "player position unavailable",
        ));
    };
    if aboard.is_some() || airborne {
        return Err(Error::Status(StatusCode::CONFLICT, "already aboard"));
    }
    let apart = movement::length(std::array::from_fn(|axis| {
        [x, y, z][axis] - vehicle.position[axis]
    }));
    if apart > BOARD_REACH_M {
        return Err(Error::Status(
            StatusCode::FORBIDDEN,
            "vehicle is not within reach",
        ));
    }
    let terrain = movement::terrain(&state).await?;
    if terrain.water_depth(movement::unit(vehicle.position)) < MIN_DEPTH_M {
        return Err(Error::Status(StatusCode::CONFLICT, "vehicle is aground"));
    }
    // The unique index keeps a second rider off a vehicle that already carries one.
    let taken = sqlx::query("UPDATE players SET vehicle_id = $2, position_x = $3, position_y = $4, position_z = $5, movement_support = FALSE, sitting_object_id = NULL, last_moved_at = clock_timestamp() WHERE id = $1")
        .bind(player_id).bind(&request.id)
        .bind(vehicle.position[0]).bind(vehicle.position[1]).bind(vehicle.position[2])
        .execute(&mut *transaction).await;
    if taken.is_err() {
        return Err(Error::Status(StatusCode::CONFLICT, "vehicle is taken"));
    }
    remember(
        &mut transaction,
        state.world_id,
        &request.id,
        vehicle.position,
        0.0,
    )
    .await?;
    transaction.commit().await?;
    Ok(Json(Boarded {
        vehicle: Some(request.id),
        position: vehicle.position,
    }))
}

async fn leave(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<Boarded>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let terrain = movement::terrain(&state).await?;
    let mut transaction = state.pool.begin().await?;
    if !super::survival::lock_alive(&state, &mut transaction, player_id).await? {
        return Err(Error::Status(StatusCode::GONE, "player is dead"));
    }
    let (x, y, z, aboard): (f64, f64, f64, Option<String>) = sqlx::query_as(
        "SELECT position_x, position_y, position_z, vehicle_id FROM players WHERE id = $1 AND position_x IS NOT NULL FOR UPDATE",
    )
    .bind(player_id)
    .fetch_one(&mut *transaction)
    .await?;
    let Some(vehicle_id) = aboard else {
        return Err(Error::Status(StatusCode::CONFLICT, "not aboard"));
    };
    let here = [x, y, z];
    let shore = terrain
        .landing_near(here, LANDING_REACH_M)
        .ok_or(Error::Status(StatusCode::CONFLICT, "no shore within reach"))?;
    sqlx::query("UPDATE players SET vehicle_id = NULL, position_x = $2, position_y = $3, position_z = $4, last_moved_at = clock_timestamp() WHERE id = $1")
        .bind(player_id).bind(shore[0]).bind(shore[1]).bind(shore[2])
        .execute(&mut *transaction).await?;
    remember(&mut transaction, state.world_id, &vehicle_id, here, 0.0).await?;
    transaction.commit().await?;
    Ok(Json(Boarded {
        vehicle: None,
        position: shore,
    }))
}
