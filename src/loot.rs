//! Dropping items on the ground, picking them up, and NPC scavenging.
//!
//! A piece held in the hand (or worn as armour) cannot be thrown away until it is put back into
//! the pack. Loose inventory can be dropped at the character's feet. What lies on the ground
//! stays until a player picks it up or an NPC comes within view and pockets it. Wayfarers then
//! divert along the road toward the nearest merchant, sell the find into the merchant's stock,
//! and the player can only buy it there afterwards — at a higher price than the finder asked.

use super::gathering::add_items;
use super::players::{self, Error};
use super::shops;
use super::story::world::{self, TALK_RANGE_M};
use super::AppState;
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::collections::HashMap;

/// How far a player may be from a pile to pick it up (metres).
const PICKUP_REACH_M: f64 = 3.0;
/// How far a standing NPC may be from a pile to pocket it (metres).
const SCOOP_REACH_M: f64 = 4.0;
/// How far a wayfarer can see a pile beside the road (metres).
const TRAVELER_VIEW_M: f64 = 14.0;
/// How close a wayfarer must be to hand goods to a merchant (metres).
const DELIVER_REACH_M: f64 = 8.0;
/// Merchants sell finds for this many times the finder's asking price.
pub(super) const MERCHANT_MARKUP: i64 = 2;
/// How far from the viewer piles are listed with the region (metres).
const LIST_RADIUS_M: f64 = 80.0;
const MAX_QUANTITY: i64 = 1000;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/players/me/drop", post(drop))
        .route("/ground/{id}/pickup", post(pickup))
        .route("/npcs/{npc_id}/found", get(found_goods))
        .route("/npcs/{npc_id}/found/buy", post(buy_found))
}

fn status(code: StatusCode, message: &'static str) -> Error {
    Error::Status(code, message)
}

#[derive(Serialize, FromRow, Clone)]
pub(super) struct GroundItem {
    pub id: i64,
    pub item_id: String,
    pub name: String,
    pub quantity: String,
    pub durability: Option<i32>,
    pub max_durability: Option<i32>,
    pub position_x: f64,
    pub position_y: f64,
    pub position_z: f64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Drop {
    item_id: String,
    #[serde(default = "one")]
    quantity: i64,
}

fn one() -> i64 {
    1
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Buy {
    item_id: String,
    #[serde(default = "one")]
    quantity: i64,
}

#[derive(Serialize)]
struct FoundGood {
    item_id: String,
    name: String,
    quantity: String,
    price: String,
    durability: Option<i32>,
    max_durability: Option<i32>,
}

#[derive(Serialize)]
struct FoundGoods {
    npc_id: String,
    /// Same shape as a merchant listing so the client trade panel can show it.
    sells: Vec<FoundGood>,
    buys: Vec<FoundGood>,
    sold_today: String,
    daily_limit: String,
}

/// Whether this NPC currently holds at least one found item.
pub(super) async fn npc_has_goods(state: &AppState, npc_id: &str) -> Result<bool, Error> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM npc_inventory WHERE world_id = $1 AND npc_id = $2)",
    )
    .bind(state.world_id)
    .bind(npc_id)
    .fetch_one(&state.pool)
    .await?)
}

/// Ground piles near `point`, for the region reply. Also lets nearby NPCs pocket what they find
/// and lets wayfarers deliver finds to merchants.
pub(super) async fn near(state: &AppState, point: [f64; 3]) -> Result<Vec<GroundItem>, Error> {
    scoop_near(state, point).await?;
    deliver_near(state, point).await?;
    Ok(sqlx::query_as(
        "SELECT g.id, g.item_id, t.name, g.quantity::text, g.durability, t.max_durability, g.position_x, g.position_y, g.position_z
         FROM ground_items g JOIN item_types t ON t.id = g.item_id
         WHERE g.world_id = $1
           AND g.position_x BETWEEN $2 - $5 AND $2 + $5
           AND (g.position_x - $2)^2 + (g.position_y - $3)^2 + (g.position_z - $4)^2 <= $5 * $5
         ORDER BY g.id LIMIT 200",
    )
    .bind(state.world_id)
    .bind(point[0])
    .bind(point[1])
    .bind(point[2])
    .bind(LIST_RADIUS_M)
    .fetch_all(&state.pool)
    .await?)
}

/// Active errands (wayfarer id → merchant id) for route diversion.
pub(super) async fn errands(state: &AppState) -> HashMap<String, String> {
    let rows: Result<Vec<(String, String)>, _> =
        sqlx::query_as("SELECT npc_id, merchant_id FROM npc_errands WHERE world_id = $1")
            .bind(state.world_id)
            .fetch_all(&state.pool)
            .await;
    rows.unwrap_or_default().into_iter().collect()
}

/// NPCs near `point` take any piles within view into their packs; wayfarers then head for a merchant.
async fn scoop_near(state: &AppState, point: [f64; 3]) -> Result<(), Error> {
    if world::world(state).await?.is_none() {
        return Ok(());
    }
    let npcs = world::npcs_near(state, point, None).await;
    if npcs.is_empty() {
        return Ok(());
    }
    let mut transaction = state.pool.begin().await?;
    for npc in &npcs {
        // `npcs_near` already applied walking and any errand diversion.
        let reach = if is_traveler(npc) {
            TRAVELER_VIEW_M
        } else {
            SCOOP_REACH_M
        };
        let piles: Vec<(i64, String, i64, Option<i32>)> = sqlx::query_as(
            "SELECT id, item_id, quantity, durability FROM ground_items
             WHERE world_id = $1
               AND (position_x - $2)^2 + (position_y - $3)^2 + (position_z - $4)^2 <= $5
             FOR UPDATE",
        )
        .bind(state.world_id)
        .bind(npc.position[0])
        .bind(npc.position[1])
        .bind(npc.position[2])
        .bind(reach * reach)
        .fetch_all(&mut *transaction)
        .await?;
        if piles.is_empty() {
            continue;
        }
        for (id, item_id, quantity, durability) in &piles {
            sqlx::query(
                "INSERT INTO npc_inventory (world_id, npc_id, item_id, quantity, durability)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (world_id, npc_id, item_id) DO UPDATE SET
                   quantity = npc_inventory.quantity + EXCLUDED.quantity,
                   durability = coalesce(npc_inventory.durability, EXCLUDED.durability)",
            )
            .bind(state.world_id)
            .bind(&npc.id)
            .bind(item_id)
            .bind(quantity)
            .bind(durability)
            .execute(&mut *transaction)
            .await?;
            sqlx::query("DELETE FROM ground_items WHERE id = $1")
                .bind(id)
                .execute(&mut *transaction)
                .await?;
        }
        if is_traveler(npc) {
            if let Some(merchant) = nearest_merchant(state, npc.position).await? {
                sqlx::query(
                    "INSERT INTO npc_errands (world_id, npc_id, merchant_id) VALUES ($1, $2, $3)
                     ON CONFLICT (world_id, npc_id) DO UPDATE SET merchant_id = EXCLUDED.merchant_id, started_at = now()",
                )
                .bind(state.world_id)
                .bind(&npc.id)
                .bind(&merchant.id)
                .execute(&mut *transaction)
                .await?;
            }
        }
    }
    transaction.commit().await?;
    Ok(())
}

/// Wayfarers that have reached their merchant hand over everything and clear the errand.
async fn deliver_near(state: &AppState, point: [f64; 3]) -> Result<(), Error> {
    let errands = errands(state).await;
    if errands.is_empty() {
        return Ok(());
    }
    let npcs = world::npcs_near(state, point, None).await;
    let mut transaction = state.pool.begin().await?;
    for npc in &npcs {
        let Some(merchant_id) = errands.get(&npc.id) else {
            continue;
        };
        let Some(story) = world::world(state).await? else {
            break;
        };
        let Some(merchant) = story.npcs.iter().find(|npc| npc.id == *merchant_id) else {
            continue;
        };
        let merchant_here = world::position_now(state, merchant).await;
        if world::distance(npc.position, &merchant_here) > DELIVER_REACH_M {
            continue;
        }
        let goods: Vec<(String, i64, Option<i32>)> = sqlx::query_as(
            "SELECT item_id, quantity, durability FROM npc_inventory
             WHERE world_id = $1 AND npc_id = $2 FOR UPDATE",
        )
        .bind(state.world_id)
        .bind(&npc.id)
        .fetch_all(&mut *transaction)
        .await?;
        for (item_id, quantity, durability) in goods {
            sqlx::query(
                "INSERT INTO merchant_stock (world_id, npc_id, item_id, quantity, durability)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (world_id, npc_id, item_id) DO UPDATE SET
                   quantity = merchant_stock.quantity + EXCLUDED.quantity,
                   durability = coalesce(merchant_stock.durability, EXCLUDED.durability)",
            )
            .bind(state.world_id)
            .bind(merchant_id)
            .bind(&item_id)
            .bind(quantity)
            .bind(durability)
            .execute(&mut *transaction)
            .await?;
        }
        sqlx::query("DELETE FROM npc_inventory WHERE world_id = $1 AND npc_id = $2")
            .bind(state.world_id)
            .bind(&npc.id)
            .execute(&mut *transaction)
            .await?;
        sqlx::query("DELETE FROM npc_errands WHERE world_id = $1 AND npc_id = $2")
            .bind(state.world_id)
            .bind(&npc.id)
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await?;
    Ok(())
}

async fn drop(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Drop>,
) -> Result<Json<players::Player>, Error> {
    if !(1..=MAX_QUANTITY).contains(&input.quantity) {
        return Err(status(StatusCode::BAD_REQUEST, "invalid quantity"));
    }
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let mut transaction = state.pool.begin().await?;
    if !super::survival::lock_alive(&state, &mut transaction, player_id).await? {
        let notice = super::survival::obituary(&mut transaction, player_id).await?;
        transaction.commit().await?;
        return Err(Error::Obituary(Box::new(notice)));
    }
    let position: Option<(f64, f64, f64)> = sqlx::query_as(
        "SELECT position_x, position_y, position_z FROM players WHERE id = $1 AND position_x IS NOT NULL",
    )
    .bind(player_id)
    .fetch_optional(&mut *transaction)
    .await?;
    let Some((x, y, z)) = position else {
        return Err(status(StatusCode::CONFLICT, "player position unavailable"));
    };
    let equipped: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM player_equipment
            WHERE player_id = $1 AND ($2 IN (hand, offhand, body, hands))
         )",
    )
    .bind(player_id)
    .bind(&input.item_id)
    .fetch_one(&mut *transaction)
    .await?;
    if equipped {
        return Err(status(StatusCode::CONFLICT, "cannot drop an equipped item"));
    }
    let row: Option<(i64, Option<i32>, Option<i32>)> = sqlx::query_as(
        "SELECT quantity, durability, max_durability FROM player_inventory
         JOIN item_types ON item_types.id = item_id
         WHERE player_id = $1 AND item_id = $2 FOR UPDATE OF player_inventory",
    )
    .bind(player_id)
    .bind(&input.item_id)
    .fetch_optional(&mut *transaction)
    .await?;
    let Some((owned, durability, max_durability)) = row else {
        return Err(status(StatusCode::CONFLICT, "item not found"));
    };
    if owned < input.quantity {
        return Err(status(StatusCode::CONFLICT, "not enough to drop"));
    }
    // A worn piece leaves the pack one at a time so its remaining durability stays on the ground.
    let (drop_qty, drop_durability) = if max_durability.is_some() {
        if input.quantity != 1 {
            return Err(status(
                StatusCode::BAD_REQUEST,
                "drop worn gear one piece at a time",
            ));
        }
        (1, durability)
    } else {
        (input.quantity, None)
    };
    if owned == drop_qty {
        sqlx::query("DELETE FROM player_inventory WHERE player_id = $1 AND item_id = $2")
            .bind(player_id)
            .bind(&input.item_id)
            .execute(&mut *transaction)
            .await?;
    } else {
        sqlx::query(
            "UPDATE player_inventory SET quantity = quantity - $3, durability = CASE WHEN $4::bool THEN NULL ELSE durability END
             WHERE player_id = $1 AND item_id = $2",
        )
        .bind(player_id)
        .bind(&input.item_id)
        .bind(drop_qty)
        .bind(max_durability.is_some())
        .execute(&mut *transaction)
        .await?;
    }
    sqlx::query(
        "INSERT INTO ground_items (world_id, item_id, quantity, durability, position_x, position_y, position_z)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(state.world_id)
    .bind(&input.item_id)
    .bind(drop_qty)
    .bind(drop_durability)
    .bind(x)
    .bind(y)
    .bind(z)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    // An NPC already standing on the spot may take it at once.
    let _ = scoop_near(&state, [x, y, z]).await;
    Ok(Json(players::profile(&state, player_id).await?))
}

async fn pickup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<players::Player>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let mut transaction = state.pool.begin().await?;
    if !super::survival::lock_alive(&state, &mut transaction, player_id).await? {
        let notice = super::survival::obituary(&mut transaction, player_id).await?;
        transaction.commit().await?;
        return Err(Error::Obituary(Box::new(notice)));
    }
    let position: Option<(f64, f64, f64)> = sqlx::query_as(
        "SELECT position_x, position_y, position_z FROM players WHERE id = $1 AND position_x IS NOT NULL",
    )
    .bind(player_id)
    .fetch_optional(&mut *transaction)
    .await?;
    let Some((x, y, z)) = position else {
        return Err(status(StatusCode::CONFLICT, "player position unavailable"));
    };
    let pile: Option<(String, i64, Option<i32>)> = sqlx::query_as(
        "SELECT item_id, quantity, durability FROM ground_items
         WHERE id = $1 AND world_id = $2
           AND (position_x - $3)^2 + (position_y - $4)^2 + (position_z - $5)^2 <= $6
         FOR UPDATE",
    )
    .bind(id)
    .bind(state.world_id)
    .bind(x)
    .bind(y)
    .bind(z)
    .bind(PICKUP_REACH_M * PICKUP_REACH_M)
    .fetch_optional(&mut *transaction)
    .await?;
    let Some((item_id, quantity, durability)) = pile else {
        return Err(status(StatusCode::NOT_FOUND, "nothing to pick up"));
    };
    add_items(&mut transaction, player_id, &[(item_id.clone(), quantity)]).await?;
    if let Some(left) = durability {
        sqlx::query(
            "UPDATE player_inventory SET durability = coalesce(durability, $3)
             WHERE player_id = $1 AND item_id = $2",
        )
        .bind(player_id)
        .bind(&item_id)
        .bind(left)
        .execute(&mut *transaction)
        .await?;
    }
    sqlx::query("DELETE FROM ground_items WHERE id = $1")
        .bind(id)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(Json(players::profile(&state, player_id).await?))
}

/// Gold one piece of a found item costs from the finder; prefer the general shop's sell price.
pub(super) fn found_price(item_id: &str) -> i64 {
    shops::sell_price(item_id).unwrap_or(10).max(1)
}

/// What a merchant asks once a wayfarer has sold the find to them.
pub(super) fn merchant_found_price(item_id: &str) -> i64 {
    found_price(item_id)
        .saturating_mul(MERCHANT_MARKUP)
        .max(found_price(item_id) + 1)
}

fn is_traveler(npc: &world::NpcDescriptor) -> bool {
    npc.movement
        .as_ref()
        .is_some_and(|movement| movement.kind == crate::story::disk::MovementKind::Travel)
        || npc.id.contains("traveler_")
}

/// Nearest `shop:general` merchant to `point`, if any stand in this world.
async fn nearest_merchant(
    state: &AppState,
    point: [f64; 3],
) -> Result<Option<world::NpcDescriptor>, Error> {
    let Some(story) = world::world(state).await? else {
        return Ok(None);
    };
    let mut best: Option<(f64, world::NpcDescriptor)> = None;
    for npc in &story.npcs {
        if npc.shop.as_deref() != Some("general") {
            continue;
        }
        let here = world::position_now(state, npc).await;
        let distance = world::distance(point, &here);
        if best.as_ref().map_or(true, |(far, _)| distance < *far) {
            best = Some((distance, here));
        }
    }
    Ok(best.map(|(_, npc)| npc))
}

async fn found_goods(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(npc_id): Path<String>,
) -> Result<Json<FoundGoods>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let position: Option<(f64, f64, f64)> = sqlx::query_as(
        "SELECT position_x, position_y, position_z FROM players WHERE id = $1 AND position_x IS NOT NULL",
    )
    .bind(player_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some((x, y, z)) = position else {
        return Err(status(StatusCode::CONFLICT, "player position unavailable"));
    };
    let here = world::npcs_near(&state, [x, y, z], Some(player_id))
        .await
        .into_iter()
        .find(|npc| npc.id == npc_id)
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "unknown character"))?;
    if world::distance([x, y, z], &here) > TALK_RANGE_M {
        return Err(status(StatusCode::FORBIDDEN, "character is out of reach"));
    }
    #[derive(FromRow)]
    struct Stock {
        item_id: String,
        name: String,
        quantity: i64,
        durability: Option<i32>,
        max_durability: Option<i32>,
    }
    let rows: Vec<Stock> = sqlx::query_as(
        "SELECT n.item_id, t.name, n.quantity, n.durability, t.max_durability
         FROM npc_inventory n JOIN item_types t ON t.id = n.item_id
         WHERE n.world_id = $1 AND n.npc_id = $2 ORDER BY n.item_id",
    )
    .bind(state.world_id)
    .bind(&npc_id)
    .fetch_all(&state.pool)
    .await?;
    let sells = rows
        .into_iter()
        .map(|row| FoundGood {
            price: found_price(&row.item_id).to_string(),
            item_id: row.item_id,
            name: row.name,
            quantity: row.quantity.to_string(),
            durability: row.durability,
            max_durability: row.max_durability,
        })
        .collect();
    Ok(Json(FoundGoods {
        npc_id,
        sells,
        buys: Vec::new(),
        sold_today: "0".into(),
        daily_limit: "0".into(),
    }))
}

async fn buy_found(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(npc_id): Path<String>,
    Json(deal): Json<Buy>,
) -> Result<Json<players::Player>, Error> {
    if !(1..=MAX_QUANTITY).contains(&deal.quantity) {
        return Err(status(StatusCode::BAD_REQUEST, "invalid quantity"));
    }
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let cost = found_price(&deal.item_id)
        .checked_mul(deal.quantity)
        .ok_or_else(|| status(StatusCode::BAD_REQUEST, "invalid quantity"))?;
    let position: Option<(f64, f64, f64)> = sqlx::query_as(
        "SELECT position_x, position_y, position_z FROM players WHERE id = $1 AND position_x IS NOT NULL",
    )
    .bind(player_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some((x, y, z)) = position else {
        return Err(status(StatusCode::CONFLICT, "player position unavailable"));
    };
    let here = world::npcs_near(&state, [x, y, z], Some(player_id))
        .await
        .into_iter()
        .find(|npc| npc.id == npc_id)
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "unknown character"))?;
    if world::distance([x, y, z], &here) > TALK_RANGE_M {
        return Err(status(StatusCode::FORBIDDEN, "character is out of reach"));
    }
    let mut transaction = state.pool.begin().await?;
    if !super::survival::lock_alive(&state, &mut transaction, player_id).await? {
        let notice = super::survival::obituary(&mut transaction, player_id).await?;
        transaction.commit().await?;
        return Err(Error::Obituary(Box::new(notice)));
    }
    let stock: Option<(i64, Option<i32>)> = sqlx::query_as(
        "SELECT quantity, durability FROM npc_inventory
         WHERE world_id = $1 AND npc_id = $2 AND item_id = $3 FOR UPDATE",
    )
    .bind(state.world_id)
    .bind(&npc_id)
    .bind(&deal.item_id)
    .fetch_optional(&mut *transaction)
    .await?;
    let Some((owned, durability)) = stock else {
        return Err(status(
            StatusCode::NOT_FOUND,
            "the merchant does not sell this",
        ));
    };
    if owned < deal.quantity {
        return Err(status(StatusCode::CONFLICT, "not enough stock"));
    }
    shops::take(&mut transaction, player_id, "gold", cost).await?;
    if owned == deal.quantity {
        sqlx::query(
            "DELETE FROM npc_inventory WHERE world_id = $1 AND npc_id = $2 AND item_id = $3",
        )
        .bind(state.world_id)
        .bind(&npc_id)
        .bind(&deal.item_id)
        .execute(&mut *transaction)
        .await?;
    } else {
        sqlx::query(
            "UPDATE npc_inventory SET quantity = quantity - $4, durability = CASE WHEN $5::bool THEN NULL ELSE durability END
             WHERE world_id = $1 AND npc_id = $2 AND item_id = $3",
        )
        .bind(state.world_id)
        .bind(&npc_id)
        .bind(&deal.item_id)
        .bind(deal.quantity)
        .bind(durability.is_some() && deal.quantity == 1)
        .execute(&mut *transaction)
        .await?;
    }
    add_items(
        &mut transaction,
        player_id,
        &[(deal.item_id.clone(), deal.quantity)],
    )
    .await?;
    if let Some(left) = durability.filter(|_| deal.quantity == 1) {
        sqlx::query(
            "UPDATE player_inventory SET durability = coalesce(durability, $3)
             WHERE player_id = $1 AND item_id = $2",
        )
        .bind(player_id)
        .bind(&deal.item_id)
        .bind(left)
        .execute(&mut *transaction)
        .await?;
    }
    transaction.commit().await?;
    Ok(Json(players::profile(&state, player_id).await?))
}

fn metres(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

/// Route a wayfarer along its trail toward `merchant` (preferring the road waypoints it already walks).
pub(super) fn errand_wander(
    npc: &world::NpcDescriptor,
    merchant: &world::NpcDescriptor,
    now_ms: i64,
) -> Vec<[f64; 4]> {
    let speed = npc
        .movement
        .as_ref()
        .map(|movement| movement.speed_m_s)
        .unwrap_or(1.4)
        .max(0.4);
    let goal = merchant.position;
    let mut stops = Vec::new();
    if let Some(movement) = &npc.movement {
        if matches!(
            movement.kind,
            crate::story::disk::MovementKind::Travel | crate::story::disk::MovementKind::Patrol
        ) {
            let height = crate::movement::length(npc.position) - crate::movement::RADIUS;
            let points: Vec<[f64; 3]> = movement
                .waypoints
                .iter()
                .map(|spot| {
                    let (direction, _) = crate::scenery::world_from_local(
                        npc.site,
                        spot[0],
                        spot[1],
                        |_| 0.0,
                        crate::movement::RADIUS,
                    );
                    direction.map(|axis| axis * (crate::movement::RADIUS + height.max(0.0)))
                })
                .collect();
            if points.len() >= 2 {
                let nearest = points
                    .iter()
                    .enumerate()
                    .min_by(|(_, a), (_, b)| {
                        metres(npc.position, **a)
                            .partial_cmp(&metres(npc.position, **b))
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|(index, _)| index)
                    .unwrap_or(0);
                let toward_end = metres(points[points.len() - 1], goal) <= metres(points[0], goal);
                if toward_end {
                    stops.extend(points[nearest..].iter().copied());
                } else {
                    stops.extend(points[..=nearest].iter().rev().copied());
                }
            }
        }
    }
    stops.push(goal);
    let mut route = Vec::new();
    let mut time = now_ms;
    let mut from = npc.position;
    route.push([time as f64, from[0], from[1], from[2]]);
    for to in stops {
        let step_ms = ((metres(from, to) / speed) * 1000.0).round() as i64;
        time += step_ms.max(500);
        route.push([time as f64, to[0], to[1], to[2]]);
        from = to;
        if time - now_ms > 90_000 {
            break;
        }
    }
    route
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn found_prices_are_positive() {
        assert!(found_price("apple") >= 1);
        assert!(found_price("sword") >= found_price("apple"));
    }

    #[test]
    fn merchant_asks_more_than_the_finder() {
        assert!(merchant_found_price("sword") > found_price("sword"));
    }

    #[test]
    fn errand_route_ends_at_the_merchant() {
        let npc = world::NpcDescriptor {
            id: "travelers:traveler_00".into(),
            name_key: String::new(),
            character: String::new(),
            model: None,
            position: [crate::movement::RADIUS + 10.0, 0.0, 0.0],
            yaw: 0.0,
            scale_m: 1.0,
            portrait: None,
            shop: None,
            movement: Some(crate::story::disk::Movement {
                kind: crate::story::disk::MovementKind::Travel,
                radius_m: 25.0,
                speed_m_s: 1.4,
                pause_s: 0.0,
                waypoints: vec![[0.0, 0.0], [100.0, 0.0], [200.0, 0.0]],
                phase_s: 0.0,
                follow: None,
            }),
            site: [1.0, 0.0, 0.0],
            wander: None,
            unlocked_eggs: Vec::new(),
            health: None,
        };
        let merchant = world::NpcDescriptor {
            position: [crate::movement::RADIUS + 50.0, 20.0, 0.0],
            ..npc.clone()
        };
        let route = errand_wander(&npc, &merchant, 1_000);
        assert!(route.len() >= 2);
        let last = route.last().unwrap();
        assert!((last[1] - merchant.position[0]).abs() < 1.0 || route.len() > 2);
        assert_eq!(route[0][0], 1_000.0);
    }
}
