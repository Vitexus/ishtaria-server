//! Quest checkpoints: what a death means for a character on a quest (and what a companion's fail does).
//!
//! * No active quest, or still on its first stage: the engine's permanent death, a memorial keeping the inventory.
//! * Past the first stage: no death. The character returns, with the stage kept, to where the quest last changed
//!   its stage (`player_quests.checkpoint_*`, migration 0063), or where the stage's own `fail_restart` says, and
//!   loses what they gained since that change: every item (gold too) is cut back to what was carried then.

use super::world::{self, site_position, StoryWorld};
use super::Story;
use crate::{players::Error, AppState};
use axum::http::StatusCode;
use sqlx::{Postgres, Transaction};

fn status(code: StatusCode, message: &'static str) -> Error {
    Error::Status(code, message)
}

/// Stage and place to restore when the current attempt fails.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Restart {
    pub quest_id: String,
    pub stage: String,
    /// The place to return to, or (with `checkpoint`) the fallback when no checkpoint is usable.
    pub place: Option<String>,
    /// Return to where the stage was reached, keeping it.
    pub checkpoint: bool,
}

/// Resolves fail_restart on the current stage, else the current stage at its checkpoint (falling back
/// to the quest's start place).
pub fn resolve(story: &Story, quest_id: &str, stage_id: &str) -> Option<Restart> {
    let quest = story.quests.get(quest_id)?;
    let stage = quest.stages.get(stage_id)?;
    if stage.final_stage {
        return None;
    }
    if let Some(restart) = &stage.fail_restart {
        return Some(Restart {
            quest_id: quest_id.to_owned(),
            stage: restart.stage.clone(),
            place: Some(restart.place.clone()),
            checkpoint: false,
        });
    }
    Some(Restart {
        quest_id: quest_id.to_owned(),
        stage: stage_id.to_owned(),
        place: quest.start_place.clone(),
        checkpoint: true,
    })
}

/// Active non-final quest for the player (most recently updated), if any.
async fn active_quest(
    tx: &mut Transaction<'_, Postgres>,
    player_id: i64,
    story: &StoryWorld,
) -> Result<Option<(String, String)>, Error> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT quest_id, stage FROM player_quests WHERE player_id = $1 ORDER BY updated_at DESC",
    )
    .bind(player_id)
    .fetch_all(&mut **tx)
    .await?;
    for (quest_id, stage) in rows {
        if let Some(quest) = story.story.quests.get(&quest_id) {
            if let Some(def) = quest.stages.get(&stage) {
                if !def.final_stage {
                    return Ok(Some((quest_id, stage)));
                }
            }
        }
    }
    Ok(None)
}

/// Teleports the player to a placed story site and bumps the movement sequence.
pub async fn teleport_to_place(
    tx: &mut Transaction<'_, Postgres>,
    story: &StoryWorld,
    player_id: i64,
    place_id: &str,
) -> Result<(), Error> {
    let site = story
        .sites
        .get(place_id)
        .ok_or_else(|| status(StatusCode::CONFLICT, "teleport place unavailable"))?;
    teleport_to_position(tx, player_id, site_position(site)).await
}

/// Puts the player at a position (metres from the planet centre) and bumps the movement sequence.
async fn teleport_to_position(
    tx: &mut Transaction<'_, Postgres>,
    player_id: i64,
    position: [f64; 3],
) -> Result<(), Error> {
    sqlx::query(
        "UPDATE players SET position_x = $2, position_y = $3, position_z = $4,
         movement_sequence = movement_sequence + 1, movement_airborne = false,
         jump_vertical_speed = 0, vehicle_id = NULL
         WHERE id = $1",
    )
    .bind(player_id)
    .bind(position[0])
    .bind(position[1])
    .bind(position[2])
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Resets the active quest to its checkpoint and restores the player. Returns true when
/// a restart was applied (caller must not create a memorial).
///
/// `death_cause` is set when zero health triggered the restart (`creature`, `fall`, …) so the
/// client can show a funeral notice before the quest toast; escort soft-fails pass `None`.
pub async fn restart_active_quest(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    player_id: i64,
    death_cause: Option<&str>,
) -> Result<bool, Error> {
    let Some(world) = world::world(state).await? else {
        return Ok(false);
    };
    let Some((quest_id, stage_id)) = active_quest(tx, player_id, &world).await? else {
        return Ok(false);
    };
    // Dying on the first stage of a quest is the engine's death: nothing has been achieved to return to.
    if death_cause.is_some() && on_first_stage(&world.story, &quest_id, &stage_id) {
        return Ok(false);
    }
    let Some(restart) = resolve(&world.story, &quest_id, &stage_id) else {
        return Ok(false);
    };
    if death_cause.is_some() {
        give_back_since_checkpoint(tx, player_id, &quest_id).await?;
    }
    let checkpoint = if restart.checkpoint {
        usable_checkpoint(state, tx, player_id, &restart.quest_id).await?
    } else {
        None
    };
    match (checkpoint, &restart.place) {
        (Some(position), _) => teleport_to_position(tx, player_id, position).await?,
        (None, Some(place)) => teleport_to_place(tx, &world, player_id, place).await?,
        (None, None) => {}
    }
    // After the teleport, so a changed stage records the place it restarts from as its checkpoint.
    sqlx::query(
        "UPDATE player_quests SET stage = $3, updated_at = now()
         WHERE player_id = $1 AND quest_id = $2",
    )
    .bind(player_id)
    .bind(&restart.quest_id)
    .bind(&restart.stage)
    .execute(&mut **tx)
    .await?;
    // Full reserves: thirst/exhaustion/hunger soft-restart must not kill the character again.
    sqlx::query(
        "UPDATE players SET health = 100, health_fraction = 0, stamina = 100.0, stamina_fraction = 0,
         water = 100.0, water_fraction = 0, food = 100, last_ate_at = now(), activity_seconds = 0,
         survival_updated_at = clock_timestamp() WHERE id = $1",
    )
    .bind(player_id)
    .execute(&mut **tx)
    .await?;
    // Soft-reset escort companion if one is active.
    sqlx::query("DELETE FROM player_escorts WHERE player_id = $1")
        .bind(player_id)
        .execute(&mut **tx)
        .await?;
    let body = match death_cause {
        Some(cause) => format!(
            r#"{{"quest":"{}","stage":"{}","cause":"{}"}}"#,
            restart.quest_id, restart.stage, cause
        ),
        None => format!(
            r#"{{"quest":"{}","stage":"{}"}}"#,
            restart.quest_id, restart.stage
        ),
    };
    sqlx::query(
        "INSERT INTO player_events (recipient_id, kind, subject, body) VALUES ($1, 'story', 'quest_restart', $2)",
    )
    .bind(player_id)
    .bind(body)
    .execute(&mut **tx)
    .await?;
    Ok(true)
}

/// Whether a quest is still on the stage it starts with.
pub fn on_first_stage(story: &Story, quest_id: &str, stage_id: &str) -> bool {
    story
        .quests
        .get(quest_id)
        .is_some_and(|quest| quest.start_stage == stage_id)
}

/// Takes back what the character gained since the quest's checkpoint: every item (gold too) is cut back to what
/// they carried when the stage last changed; what they used up meanwhile stays used. Worn and held things that are
/// gone are put down. Rows from before the checkpoints were recorded keep everything.
async fn give_back_since_checkpoint(
    tx: &mut Transaction<'_, Postgres>,
    player_id: i64,
    quest_id: &str,
) -> Result<(), Error> {
    let recorded: Option<bool> = sqlx::query_scalar(
        "SELECT checkpoint_at IS NOT NULL FROM player_quests WHERE player_id = $1 AND quest_id = $2",
    )
    .bind(player_id)
    .bind(quest_id)
    .fetch_optional(&mut **tx)
    .await?;
    if recorded != Some(true) {
        return Ok(());
    }
    sqlx::query(
        "UPDATE player_inventory AS i SET quantity = c.quantity
         FROM player_quest_checkpoint_items AS c
         WHERE i.player_id = $1 AND c.player_id = $1 AND c.quest_id = $2 AND c.item_id = i.item_id
           AND i.quantity > c.quantity",
    )
    .bind(player_id)
    .bind(quest_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "DELETE FROM player_inventory AS i WHERE i.player_id = $1 AND NOT EXISTS (
             SELECT 1 FROM player_quest_checkpoint_items AS c
             WHERE c.player_id = $1 AND c.quest_id = $2 AND c.item_id = i.item_id)",
    )
    .bind(player_id)
    .bind(quest_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE player_equipment AS e SET
             hand = CASE WHEN EXISTS (SELECT 1 FROM player_inventory WHERE player_id = $1 AND item_id = e.hand) THEN e.hand END,
             offhand = CASE WHEN EXISTS (SELECT 1 FROM player_inventory WHERE player_id = $1 AND item_id = e.offhand) THEN e.offhand END,
             body = CASE WHEN EXISTS (SELECT 1 FROM player_inventory WHERE player_id = $1 AND item_id = e.body) THEN e.body END,
             hands = CASE WHEN EXISTS (SELECT 1 FROM player_inventory WHERE player_id = $1 AND item_id = e.hands) THEN e.hands END
         WHERE e.player_id = $1",
    )
    .bind(player_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Where the quest last changed its stage, when that spot is dry ground (a checkpoint taken while
/// swimming would put the character back into the water).
async fn usable_checkpoint(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    player_id: i64,
    quest_id: &str,
) -> Result<Option<[f64; 3]>, Error> {
    let stored: Option<(Option<f64>, Option<f64>, Option<f64>)> = sqlx::query_as(
        "SELECT checkpoint_x, checkpoint_y, checkpoint_z FROM player_quests WHERE player_id = $1 AND quest_id = $2",
    )
    .bind(player_id)
    .bind(quest_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((Some(x), Some(y), Some(z))) = stored else {
        return Ok(None);
    };
    let position = [x, y, z];
    let distance = (x * x + y * y + z * z).sqrt();
    if !position.iter().all(|value| value.is_finite()) || distance < 1.0 {
        return Ok(None);
    }
    let terrain = crate::movement::terrain(state).await?;
    let direction = position.map(|value| value / distance);
    Ok((terrain.water_depth(direction) <= 0.0).then_some(position))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::story::disk::{FailRestart, Quest, Stage};
    use std::collections::BTreeMap;

    #[test]
    fn fail_restart_overrides_quest_start() {
        let mut stages = BTreeMap::new();
        stages.insert(
            "start".to_owned(),
            Stage {
                text_key: "t.start".to_owned(),
                final_stage: false,
                reach: None,
                guide: None,
                fail_restart: None,
            },
        );
        stages.insert(
            "road".to_owned(),
            Stage {
                text_key: "t.road".to_owned(),
                final_stage: false,
                reach: None,
                guide: None,
                fail_restart: Some(FailRestart {
                    stage: "start".to_owned(),
                    place: "d:temple".to_owned(),
                }),
            },
        );
        let mut story = Story::default();
        story.quests.insert(
            "d:q".to_owned(),
            Quest {
                id: "d:q".to_owned(),
                title_key: "t".to_owned(),
                start_stage: "start".to_owned(),
                start_place: Some("d:town".to_owned()),
                known_places: Vec::new(),
                excerpt: None,
                stages,
            },
        );
        let restart = resolve(&story, "d:q", "road").unwrap();
        assert_eq!(restart.stage, "start");
        assert_eq!(restart.place.as_deref(), Some("d:temple"));
        assert!(!restart.checkpoint);
        // Without its own fail_restart a stage is kept: the character returns to where it was reached,
        // with the start place only as the fallback.
        let restart = resolve(&story, "d:q", "start").unwrap();
        assert_eq!(restart.stage, "start");
        assert_eq!(restart.place.as_deref(), Some("d:town"));
        assert!(restart.checkpoint);
    }

    #[test]
    fn quest_restart_body_names_a_death_cause() {
        let with_cause = format!(
            r#"{{"quest":"{}","stage":"{}","cause":"{}"}}"#,
            "gilgamesh:hunter", "lay_snares", "creature"
        );
        let parsed: serde_json::Value = serde_json::from_str(&with_cause).unwrap();
        assert_eq!(parsed["cause"], "creature");
        assert_eq!(parsed["quest"], "gilgamesh:hunter");
        let without = format!(
            r#"{{"quest":"{}","stage":"{}"}}"#,
            "gilgamesh:hunter", "start"
        );
        let plain: serde_json::Value = serde_json::from_str(&without).unwrap();
        assert!(plain.get("cause").is_none());
    }
}
