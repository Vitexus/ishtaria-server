//! Wear of tools, weapons and shields.
//!
//! Every item type with `max_durability` wears out; that ceiling is per item, so a better
//! sword or shield lasts longer than a common one. A stack of such items has one piece in
//! use whose remaining durability is stored with the stack (`NULL` is a new piece). A piece
//! that wears out is destroyed; the next piece of the stack is new. A blacksmith can restore
//! weapons, armour and shields while they still have durability left.

use super::players::Error;

/// What happened to the item that was used.
#[derive(Clone, Copy, serde::Serialize)]
pub(super) struct Wear {
    pub durability: i32,
    pub max_durability: i32,
    /// The piece wore out and is gone from the inventory.
    pub broken: bool,
}

/// Categories a blacksmith will mend while they still have durability.
fn repairable(category: &str) -> bool {
    matches!(category, "weapon" | "armor" | "shield")
}

/// Wears the piece in use by `amount`. Items without durability are left alone.
pub(super) async fn wear(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    player_id: i64,
    item_id: &str,
    amount: i32,
) -> Result<Option<Wear>, Error> {
    let row: Option<(i64, Option<i32>, Option<i32>)> = sqlx::query_as(
        "SELECT quantity, durability, max_durability FROM player_inventory JOIN item_types ON item_types.id = item_id WHERE player_id = $1 AND item_id = $2 FOR UPDATE OF player_inventory",
    )
    .bind(player_id)
    .bind(item_id)
    .fetch_optional(&mut **transaction)
    .await?;
    let Some((quantity, durability, Some(max))) = row else {
        return Ok(None);
    };
    let left = durability.unwrap_or(max) - amount.max(0);
    if left > 0 {
        sqlx::query(
            "UPDATE player_inventory SET durability = $3 WHERE player_id = $1 AND item_id = $2",
        )
        .bind(player_id)
        .bind(item_id)
        .bind(left)
        .execute(&mut **transaction)
        .await?;
        return Ok(Some(Wear {
            durability: left,
            max_durability: max,
            broken: false,
        }));
    }
    if quantity <= 1 {
        sqlx::query("DELETE FROM player_inventory WHERE player_id = $1 AND item_id = $2")
            .bind(player_id)
            .bind(item_id)
            .execute(&mut **transaction)
            .await?;
        sqlx::query("UPDATE player_equipment SET hand = CASE WHEN hand = $2 THEN NULL ELSE hand END, offhand = CASE WHEN offhand = $2 THEN NULL ELSE offhand END, body = CASE WHEN body = $2 THEN NULL ELSE body END, hands = CASE WHEN hands = $2 THEN NULL ELSE hands END WHERE player_id = $1")
            .bind(player_id).bind(item_id).execute(&mut **transaction).await?;
    } else {
        sqlx::query("UPDATE player_inventory SET quantity = quantity - 1, durability = NULL WHERE player_id = $1 AND item_id = $2")
            .bind(player_id).bind(item_id).execute(&mut **transaction).await?;
    }
    Ok(Some(Wear {
        durability: 0,
        max_durability: max,
        broken: true,
    }))
}

/// Gold to restore `missing` points of durability (at least one coin when anything is worn).
pub(super) fn repair_cost(missing: i32) -> i64 {
    if missing <= 0 {
        0
    } else {
        i64::from((missing + 2) / 3).max(1)
    }
}

/// Restores a worn weapon, armour piece or shield to full durability for gold.
pub(super) async fn repair(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    player_id: i64,
    item_id: &str,
) -> Result<i64, Error> {
    let row: Option<(Option<i32>, Option<i32>, String)> = sqlx::query_as(
        "SELECT durability, max_durability, category FROM player_inventory JOIN item_types ON item_types.id = item_id WHERE player_id = $1 AND item_id = $2 FOR UPDATE OF player_inventory",
    )
    .bind(player_id)
    .bind(item_id)
    .fetch_optional(&mut **transaction)
    .await?;
    let Some((durability, Some(max), category)) = row else {
        return Err(Error::Status(
            axum::http::StatusCode::NOT_FOUND,
            "nothing to repair",
        ));
    };
    if !repairable(&category) {
        return Err(Error::Status(
            axum::http::StatusCode::CONFLICT,
            "this cannot be repaired",
        ));
    }
    let current = durability.unwrap_or(max);
    if current <= 0 || current >= max {
        return Err(Error::Status(
            axum::http::StatusCode::CONFLICT,
            if current <= 0 {
                "nothing to repair"
            } else {
                "already in good repair"
            },
        ));
    }
    let cost = repair_cost(max - current);
    let gold: Option<i64> = sqlx::query_scalar(
        "SELECT quantity FROM player_inventory WHERE player_id = $1 AND item_id = 'gold' FOR UPDATE",
    )
    .bind(player_id)
    .fetch_optional(&mut **transaction)
    .await?;
    if gold.unwrap_or(0) < cost {
        return Err(Error::Status(
            axum::http::StatusCode::CONFLICT,
            "not enough to pay",
        ));
    }
    if gold == Some(cost) {
        sqlx::query("DELETE FROM player_inventory WHERE player_id = $1 AND item_id = 'gold'")
            .bind(player_id)
            .execute(&mut **transaction)
            .await?;
    } else {
        sqlx::query(
            "UPDATE player_inventory SET quantity = quantity - $2 WHERE player_id = $1 AND item_id = 'gold'",
        )
        .bind(player_id)
        .bind(cost)
        .execute(&mut **transaction)
        .await?;
    }
    sqlx::query(
        "UPDATE player_inventory SET durability = $3 WHERE player_id = $1 AND item_id = $2",
    )
    .bind(player_id)
    .bind(item_id)
    .bind(max)
    .execute(&mut **transaction)
    .await?;
    Ok(cost)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repair_cost_scales_with_missing_points() {
        assert_eq!(repair_cost(0), 0);
        assert_eq!(repair_cost(1), 1);
        assert_eq!(repair_cost(3), 1);
        assert_eq!(repair_cost(4), 2);
        assert_eq!(repair_cost(150), 50);
    }
}
