//! Easter-egg unlocks: rare NPC clips gated by server-evaluated conditions.

use super::disk::{EasterEgg, EggCondition};
use super::engine::{PlayerState, GOLD};
use crate::atmosphere::{self, HORIZON_ELEVATION_DEGREES};
use crate::AppState;
use std::collections::HashSet;

/// Everything needed to decide which eggs a signed-in player has unlocked.
pub struct EggContext {
    pub state: PlayerState,
    pub spells: HashSet<String>,
    pub solar_seconds: f64,
}

impl EggContext {
    /// Loads flags, inventory, known spells and the world's solar clock for `player_id`.
    pub async fn load(state: &AppState, player_id: i64) -> Result<Self, sqlx::Error> {
        let flags: Vec<String> =
            sqlx::query_scalar("SELECT flag FROM player_story_flags WHERE player_id = $1")
                .bind(player_id)
                .fetch_all(&state.pool)
                .await?;
        let stages: Vec<(String, String)> =
            sqlx::query_as("SELECT quest_id, stage FROM player_quests WHERE player_id = $1")
                .bind(player_id)
                .fetch_all(&state.pool)
                .await?;
        let items: Vec<(String, i64)> =
            sqlx::query_as("SELECT item_id, quantity FROM player_inventory WHERE player_id = $1")
                .bind(player_id)
                .fetch_all(&state.pool)
                .await?;
        let once: Vec<String> =
            sqlx::query_scalar("SELECT once_key FROM player_story_once WHERE player_id = $1")
                .bind(player_id)
                .fetch_all(&state.pool)
                .await?;
        let spells: Vec<String> =
            sqlx::query_scalar("SELECT spell_id FROM player_spells WHERE player_id = $1")
                .bind(player_id)
                .fetch_all(&state.pool)
                .await?;
        let offset: i64 =
            sqlx::query_scalar("SELECT solar_offset_seconds FROM worlds WHERE id = $1")
                .bind(state.world_id)
                .fetch_one(&state.pool)
                .await?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        Ok(Self {
            state: PlayerState {
                flags: flags.into_iter().collect(),
                stages: stages.into_iter().collect(),
                items: items.into_iter().collect(),
                once_done: once.into_iter().collect(),
            },
            spells: spells.into_iter().collect(),
            solar_seconds: now + offset as f64,
        })
    }
}

/// Clip names unlocked for `npc_id` at `position` (world metres) for this player.
pub fn unlocked_clips(
    eggs: &[EasterEgg],
    npc_id: &str,
    position: [f64; 3],
    ctx: &EggContext,
) -> Vec<String> {
    let night = is_night(position, ctx.solar_seconds);
    let mut clips = Vec::new();
    for egg in eggs {
        if egg.npc != npc_id {
            continue;
        }
        if !passes(egg.condition.as_ref(), &ctx.state, &ctx.spells, night) {
            continue;
        }
        if !clips.iter().any(|c| c == &egg.clip) {
            clips.push(egg.clip.clone());
        }
    }
    clips
}

fn is_night(position: [f64; 3], solar_seconds: f64) -> bool {
    let length =
        (position[0] * position[0] + position[1] * position[1] + position[2] * position[2]).sqrt();
    if length < 1.0 {
        return false;
    }
    let observer = [
        position[0] / length,
        position[1] / length,
        position[2] / length,
    ];
    let sun = atmosphere::Solar::at(solar_seconds).direction;
    let elevation = observer[0] * sun[0] + observer[1] * sun[1] + observer[2] * sun[2];
    elevation < HORIZON_ELEVATION_DEGREES.to_radians().sin()
}

fn passes(
    condition: Option<&EggCondition>,
    state: &PlayerState,
    spells: &HashSet<String>,
    night: bool,
) -> bool {
    match condition {
        Some(condition) => holds(condition, state, spells, night),
        None => true,
    }
}

fn holds(
    condition: &EggCondition,
    state: &PlayerState,
    spells: &HashSet<String>,
    night: bool,
) -> bool {
    if let Some(all) = &condition.all {
        return all.iter().all(|c| holds(c, state, spells, night));
    }
    if let Some(any) = &condition.any {
        return any.iter().any(|c| holds(c, state, spells, night));
    }
    if let Some(not) = &condition.not {
        return !holds(not, state, spells, night);
    }
    if let Some(flag) = &condition.flag {
        return state.flags.contains(flag);
    }
    if let Some(stage) = &condition.quest_stage {
        return state.stages.get(&stage.quest) == Some(&stage.stage);
    }
    if let Some(item) = &condition.has_item {
        return owned(state, &item.item) >= item.quantity();
    }
    if let Some(gold) = condition.gold_at_least {
        return owned(state, GOLD) >= gold;
    }
    if let Some(want_night) = condition.night {
        return night == want_night;
    }
    if let Some(spell) = &condition.knows_spell {
        return spells.contains(spell);
    }
    false
}

fn owned(state: &PlayerState, item: &str) -> i64 {
    state.items.get(item).copied().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::story::disk::EasterEgg;

    fn egg(id: &str, npc: &str, clip: &str, condition: EggCondition) -> EasterEgg {
        EasterEgg {
            id: id.to_owned(),
            npc: npc.to_owned(),
            clip: clip.to_owned(),
            condition: Some(condition),
        }
    }

    #[test]
    fn night_and_spell_unlock_dance() {
        let eggs = [egg(
            "anu.dance",
            "gilgamesh:anu",
            "Twerk",
            EggCondition {
                all: Some(vec![
                    EggCondition {
                        night: Some(true),
                        ..Default::default()
                    },
                    EggCondition {
                        knows_spell: Some("night".into()),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            },
        )];
        let mut ctx = EggContext {
            state: PlayerState::default(),
            spells: HashSet::from(["night".into()]),
            // A solar time where the sun is opposite the +X observer → night on +X.
            solar_seconds: 946728000.0 + 43200.0,
        };
        let clips = unlocked_clips(&eggs, "gilgamesh:anu", [6_371_000.0, 0.0, 0.0], &ctx);
        assert_eq!(clips, vec!["Twerk".to_owned()]);
        ctx.spells.clear();
        assert!(unlocked_clips(&eggs, "gilgamesh:anu", [6_371_000.0, 0.0, 0.0], &ctx).is_empty());
    }

    #[test]
    fn flag_unlocks_without_night() {
        let eggs = [egg(
            "anu.dwarf",
            "gilgamesh:anu",
            "Dwarf",
            EggCondition {
                flag: Some("gilgamesh:knows_equality".into()),
                ..Default::default()
            },
        )];
        let mut state = PlayerState::default();
        state.flags.insert("gilgamesh:knows_equality".into());
        let ctx = EggContext {
            state,
            spells: HashSet::new(),
            solar_seconds: 0.0,
        };
        assert_eq!(
            unlocked_clips(&eggs, "gilgamesh:anu", [0.0, 6_371_000.0, 0.0], &ctx),
            vec!["Dwarf".to_owned()]
        );
    }

    #[test]
    fn gold_gate() {
        let eggs = [egg(
            "potter.dance",
            "gilgamesh:potter",
            "Dance",
            EggCondition {
                gold_at_least: Some(200),
                ..Default::default()
            },
        )];
        let mut state = PlayerState::default();
        state.items.insert(GOLD.to_owned(), 199);
        let mut ctx = EggContext {
            state,
            spells: HashSet::new(),
            solar_seconds: 0.0,
        };
        assert!(
            unlocked_clips(&eggs, "gilgamesh:potter", [0.0, 6_371_000.0, 0.0], &ctx).is_empty()
        );
        ctx.state.items.insert(GOLD.to_owned(), 200);
        assert_eq!(
            unlocked_clips(&eggs, "gilgamesh:potter", [0.0, 6_371_000.0, 0.0], &ctx),
            vec!["Dance".to_owned()]
        );
    }
}
