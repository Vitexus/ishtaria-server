//! Quest checkpoints: on death or companion fail, restart the active quest instead of
//! permanent memorial death (Gilgamesh finite narrative).

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
    pub place: Option<String>,
}

/// Resolves fail_restart on the current stage, else the quest start.
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
        });
    }
    Some(Restart {
        quest_id: quest_id.to_owned(),
        stage: quest.start_stage.clone(),
        place: quest.start_place.clone(),
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
    let position = site_position(site);
    sqlx::query(
        "UPDATE players SET position_x = $2, position_y = $3, position_z = $4,
         movement_sequence = movement_sequence + 1, movement_airborne = false,
         jump_vertical_speed = 0
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
    let Some(restart) = resolve(&world.story, &quest_id, &stage_id) else {
        return Ok(false);
    };
    sqlx::query(
        "UPDATE player_quests SET stage = $3, updated_at = now()
         WHERE player_id = $1 AND quest_id = $2",
    )
    .bind(player_id)
    .bind(&restart.quest_id)
    .bind(&restart.stage)
    .execute(&mut **tx)
    .await?;
    if let Some(place) = &restart.place {
        teleport_to_place(tx, &world, player_id, place).await?;
    }
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
                excerpt: None,
                stages,
            },
        );
        let restart = resolve(&story, "d:q", "road").unwrap();
        assert_eq!(restart.stage, "start");
        assert_eq!(restart.place.as_deref(), Some("d:temple"));
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
