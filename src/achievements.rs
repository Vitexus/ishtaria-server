//! Achievements: milestones a character reaches once, such as the first tree felled, a first
//! friend or level 10. Their names, icons and categories are data in `etc/achievements.json`;
//! what earns one is decided here, inside the transaction of the action that earns it, so an
//! achievement is never granted for an action that did not happen.
//!
//! [`unlock`] is idempotent: the primary key of `player_achievements` keeps one row per
//! character and achievement, and only a new row tells the owner (an `achievement` event, the
//! subject being the id). Nothing a client sends can unlock one. Titles and descriptions are
//! English keys that the client translates; the server knows no language.

use super::players::{self, Error};
use super::AppState;
use axum::{extract::State, http::HeaderMap, routing::get, Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/achievements", get(catalog_route))
        .route("/players/me/achievements", get(earned))
}

#[derive(Deserialize)]
struct Catalog {
    version: u32,
    achievements: Vec<Achievement>,
}

#[derive(Deserialize, Serialize, Clone)]
struct Achievement {
    id: String,
    category: String,
    /// An item id whose icon the client shows.
    icon: String,
    title: String,
    description: String,
    /// A hidden achievement shows neither title nor description until it is earned.
    hidden: bool,
}

const CATEGORIES: [&str; 5] = ["progress", "gathering", "magic", "social", "story"];
/// The levels that earn `level_<n>` when reached.
const LEVELS: [i32; 4] = [5, 10, 20, 30];

fn catalog() -> &'static Catalog {
    static CONFIG: OnceLock<Catalog> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let config: Catalog = serde_json::from_str(include_str!("../etc/achievements.json"))
            .expect("bundled achievements must be valid");
        assert_eq!(config.version, 1);
        for (index, achievement) in config.achievements.iter().enumerate() {
            assert!(
                valid_id(&achievement.id),
                "bad achievement id {}",
                achievement.id
            );
            assert!(
                CATEGORIES.contains(&achievement.category.as_str()),
                "unknown category of {}",
                achievement.id
            );
            assert!(
                valid_id(&achievement.icon),
                "bad icon of {}",
                achievement.id
            );
            assert!(!achievement.title.is_empty() && achievement.title.len() <= 64);
            assert!(!achievement.description.is_empty() && achievement.description.len() <= 200);
            assert!(
                !config.achievements[..index]
                    .iter()
                    .any(|other| other.id == achievement.id),
                "duplicate achievement {}",
                achievement.id
            );
        }
        config
    })
}

/// The same shape the migration's check constraint allows.
fn valid_id(id: &str) -> bool {
    (1..=48).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn known(id: &str) -> bool {
    catalog()
        .achievements
        .iter()
        .any(|achievement| achievement.id == id)
}

/// Gives `player_id` the achievement `id` unless they have it already; a new one is announced to
/// them as an event. Returns whether it was new. An id missing from the catalog is a bug in
/// the caller, so it is refused before touching the database.
pub(super) async fn unlock(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    player_id: i64,
    id: &str,
) -> Result<bool, Error> {
    debug_assert!(known(id), "unknown achievement {id}");
    if !known(id) {
        return Ok(false);
    }
    let inserted = sqlx::query(
        "INSERT INTO player_achievements (player_id, achievement) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(player_id)
    .bind(id)
    .execute(&mut **transaction)
    .await?
    .rows_affected()
        == 1;
    if inserted {
        sqlx::query(
            "INSERT INTO player_events (recipient_id, kind, subject) VALUES ($1, 'achievement', $2)",
        )
        .bind(player_id)
        .bind(id)
        .execute(&mut **transaction)
        .await?;
    }
    Ok(inserted)
}

/// The level achievements for every threshold from `old_level` (exclusive) to `level`.
pub(super) async fn levels_reached(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    player_id: i64,
    old_level: i32,
    level: i32,
) -> Result<(), Error> {
    for threshold in level_thresholds(old_level, level) {
        unlock(transaction, player_id, &format!("level_{threshold}")).await?;
    }
    Ok(())
}

fn level_thresholds(old_level: i32, level: i32) -> impl Iterator<Item = i32> {
    LEVELS
        .into_iter()
        .filter(move |threshold| old_level < *threshold && *threshold <= level)
}

#[derive(Serialize)]
struct CatalogReply {
    achievements: Vec<Achievement>,
}

async fn catalog_route() -> Json<CatalogReply> {
    Json(CatalogReply {
        achievements: catalog().achievements.iter().map(masked).collect(),
    })
}

/// A hidden achievement without its title and description.
fn masked(achievement: &Achievement) -> Achievement {
    let mut shown = achievement.clone();
    if shown.hidden {
        shown.title.clear();
        shown.description.clear();
    }
    shown
}

#[derive(Serialize)]
struct Earned {
    id: String,
    /// Seconds since the Unix epoch.
    unlocked_at: i64,
}

#[derive(Serialize)]
struct EarnedReply {
    /// The whole catalog, hidden entries masked unless earned.
    achievements: Vec<Achievement>,
    earned: Vec<Earned>,
}

async fn earned(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<EarnedReply>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT achievement, extract(epoch FROM unlocked_at)::bigint FROM player_achievements WHERE player_id = $1 ORDER BY unlocked_at, achievement",
    )
    .bind(player_id)
    .fetch_all(&state.pool)
    .await?;
    let achievements = catalog()
        .achievements
        .iter()
        .map(|achievement| {
            if rows.iter().any(|(id, _)| *id == achievement.id) {
                achievement.clone()
            } else {
                masked(achievement)
            }
        })
        .collect();
    // Rows of achievements later removed from the catalog are not shown.
    let earned = rows
        .into_iter()
        .filter(|(id, _)| known(id))
        .map(|(id, unlocked_at)| Earned { id, unlocked_at })
        .collect();
    Ok(Json(EarnedReply {
        achievements,
        earned,
    }))
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn the_catalog_is_valid_and_has_every_level() {
        let config = catalog();
        assert!(!config.achievements.is_empty());
        for level in LEVELS {
            assert!(known(&format!("level_{level}")), "level_{level} missing");
        }
    }

    #[test]
    fn thresholds_are_crossed_once() {
        assert_eq!(level_thresholds(1, 4).count(), 0);
        assert_eq!(level_thresholds(4, 5).collect::<Vec<_>>(), [5]);
        assert_eq!(level_thresholds(5, 6).count(), 0);
        assert_eq!(level_thresholds(4, 21).collect::<Vec<_>>(), [5, 10, 20]);
        assert_eq!(level_thresholds(29, 40).collect::<Vec<_>>(), [30]);
    }

    #[test]
    fn ids_follow_the_database_constraint() {
        assert!(valid_id("first_harvest") && valid_id("level_30"));
        assert!(!valid_id("") && !valid_id("First") && !valid_id("a-b"));
        assert!(!valid_id(&"a".repeat(49)));
    }

    #[test]
    fn hidden_achievements_are_masked() {
        let hidden = catalog()
            .achievements
            .iter()
            .find(|achievement| achievement.hidden)
            .expect("a hidden achievement");
        let shown = masked(hidden);
        assert!(shown.title.is_empty() && shown.description.is_empty());
        assert_eq!(shown.id, hidden.id);
    }
}
