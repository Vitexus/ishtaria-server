//! Achievements: milestones a character reaches once, such as the first tree felled, a first
//! friend or level 10. Their names, icons and categories are data in `etc/achievements.json`;
//! what earns one is decided here, inside the transaction of the action that earns it, so an
//! achievement is never granted for an action that did not happen.
//!
//! [`unlock`] is idempotent: the primary key of `player_achievements` keeps one row per
//! character and achievement, and only a new row tells the owner (an `achievement` event, the
//! subject being the id). Nothing a client sends can unlock one. Titles and descriptions are
//! English keys that the client translates; the server knows no language.
//!
//! A datadisk adds its own achievements (`achievements.yaml`, ids `<disk>:<id>`) after the engine's;
//! their texts are string keys of the disk (`story: true`), translated by the client from
//! `/story/strings`. Ids are permanent once released. Every entry also carries its Steam API name
//! ([`steam_api_name`]), so that a client built for Steam can mirror the server's `achievement`
//! events to `SetAchievement`: the server stays the only one that decides.

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

/// The longest achievement id, `<disk>:<id>` included; `player_events.subject` holds 64 characters.
pub(crate) const MAX_ID_LEN: usize = 64;

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

pub(crate) const CATEGORIES: [&str; 5] = ["progress", "gathering", "magic", "social", "story"];
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

/// An engine achievement id: the shape the migrations' check constraint allows, without a double
/// underscore, which [`steam_api_name`] keeps for the disk separator.
fn valid_id(id: &str) -> bool {
    (1..=48).contains(&id.len())
        && !id.contains("__")
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// The name of an achievement in Steamworks: the id in capitals, a disk's `:` written as `__`
/// (`first_harvest` -> `FIRST_HARVEST`, `gilgamesh:foundation_reader` -> `GILGAMESH__FOUNDATION_READER`).
/// Ids never contain `__`, so two ids never share a name.
pub(crate) fn steam_api_name(id: &str) -> String {
    id.replace(':', "__").to_ascii_uppercase()
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
    record(transaction, player_id, id).await
}

/// [`unlock`] for an achievement of the world's datadisks (`<disk>:<id>`).
pub(crate) async fn unlock_disk(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    story: &crate::story::Story,
    player_id: i64,
    id: &str,
) -> Result<bool, Error> {
    let known = story
        .achievements
        .iter()
        .any(|achievement| achievement.id == id);
    debug_assert!(known, "unknown disk achievement {id}");
    if !known {
        return Ok(false);
    }
    record(transaction, player_id, id).await
}

async fn record(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    player_id: i64,
    id: &str,
) -> Result<bool, Error> {
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

/// An achievement as clients see it.
#[derive(Serialize, Clone)]
struct Shown {
    id: String,
    category: String,
    icon: String,
    /// An English key, or a string key of a datadisk when `story` is set; empty while hidden.
    title: String,
    description: String,
    hidden: bool,
    /// The texts are keys of `/story/strings` (an achievement of a datadisk).
    story: bool,
    /// The achievement's name in Steamworks.
    steam: String,
}

/// The engine's achievements, then those of the world's datadisks.
fn entries(story: Option<&crate::story::Story>) -> Vec<Shown> {
    let engine = catalog().achievements.iter().map(|achievement| Shown {
        id: achievement.id.clone(),
        category: achievement.category.clone(),
        icon: achievement.icon.clone(),
        title: achievement.title.clone(),
        description: achievement.description.clone(),
        hidden: achievement.hidden,
        story: false,
        steam: steam_api_name(&achievement.id),
    });
    let disks = story
        .into_iter()
        .flat_map(|story| story.achievements.iter())
        .map(|achievement| Shown {
            id: achievement.id.clone(),
            category: achievement.category.clone(),
            icon: achievement.icon.clone(),
            title: achievement.title_key.clone(),
            description: achievement.description_key.clone(),
            hidden: achievement.hidden,
            story: true,
            steam: steam_api_name(&achievement.id),
        });
    engine.chain(disks).collect()
}

/// The datadisks of the world, when it has a story.
async fn story(
    state: &AppState,
) -> Result<Option<std::sync::Arc<crate::story::world::StoryWorld>>, Error> {
    crate::story::world::world(state).await
}

#[derive(Serialize)]
struct CatalogReply {
    achievements: Vec<Shown>,
}

async fn catalog_route(State(state): State<AppState>) -> Result<Json<CatalogReply>, Error> {
    let story = story(&state).await?;
    Ok(Json(CatalogReply {
        achievements: entries(story.as_deref().map(|world| &world.story))
            .iter()
            .map(masked)
            .collect(),
    }))
}

/// A hidden achievement without its title and description.
fn masked(achievement: &Shown) -> Shown {
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
    achievements: Vec<Shown>,
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
    let story = story(&state).await?;
    let all = entries(story.as_deref().map(|world| &world.story));
    let achievements = all
        .iter()
        .map(|achievement| {
            if rows.iter().any(|(id, _)| *id == achievement.id) {
                achievement.clone()
            } else {
                masked(achievement)
            }
        })
        .collect();
    // Rows of achievements later removed from the catalog (or of a disk no longer applied) are not shown.
    let earned = rows
        .into_iter()
        .filter(|(id, _)| all.iter().any(|achievement| achievement.id == *id))
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
    fn steam_names_are_unique_and_upper_case() {
        assert_eq!(steam_api_name("first_harvest"), "FIRST_HARVEST");
        assert_eq!(
            steam_api_name("gilgamesh:foundation_reader"),
            "GILGAMESH__FOUNDATION_READER"
        );
        assert!(!valid_id("a__b"));
        let names: std::collections::HashSet<String> = catalog()
            .achievements
            .iter()
            .map(|achievement| steam_api_name(&achievement.id))
            .collect();
        assert_eq!(names.len(), catalog().achievements.len());
    }

    #[test]
    fn hidden_achievements_are_masked() {
        let all = entries(None);
        let hidden = all
            .iter()
            .find(|achievement| achievement.hidden)
            .expect("a hidden achievement");
        let shown = masked(hidden);
        assert!(shown.title.is_empty() && shown.description.is_empty());
        assert_eq!(shown.id, hidden.id);
    }
}
