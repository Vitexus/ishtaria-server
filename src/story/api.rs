//! HTTP API of the story: dialogues, quest log, translations and media.

use super::{
    disk,
    engine::{self, Choose, Op, PlayerState, Reject, Shown, VisibleChoice},
    placement::Placed,
    world::{self, StoryWorld, TALK_RANGE_M},
    Story,
};
use crate::{npc_combat, players, AppState};
use axum::{
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use players::Error;

/// How far a walking character may have gone from a player it is talking to.
const ONGOING_RANGE_M: f64 = 25.0;
/// How close to a readable structure (a tablet in its box) a player stands to read it.
const READ_RANGE_M: f64 = 3.0;
/// Synthetic dialogue choice: apologize and pay gold to clear lasting NPC hostility.
const APOLOGY_CHOICE: usize = 65_000;
const APOLOGY_TEXT_KEY: &str = "Apologize (50 gold)";
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};
use std::{collections::HashMap, sync::Arc};

type Tx<'a> = Transaction<'a, Postgres>;

fn status(code: StatusCode, message: &'static str) -> Error {
    Error::Status(code, message)
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/story/dialogue/start", post(start))
        .route("/story/dialogue/choose", post(choose))
        .route("/story/dialogue", delete(leave))
        .route("/story/npcs/{npc_id}/about", get(about))
        .route("/story/places/{place_id}/read", post(read_place))
        .route("/story/quests", get(quests))
        .route("/story/markers", get(markers))
        .layer(DefaultBodyLimit::max(1024))
        .route("/story/strings", get(strings))
        .route("/story/media/{disk}/{*path}", get(media))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    npc_id: String,
    /// Language of the client's UI (two lowercase letters): only selects the spoken line to send.
    /// It is never stored.
    #[serde(default)]
    lang: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChooseRequest {
    npc_id: String,
    /// The `seq` of the answer the choice was made from.
    seq: i64,
    choice: usize,
    /// See [`Start::lang`].
    #[serde(default)]
    lang: Option<String>,
}

#[derive(Serialize)]
struct Music {
    url: String,
    title_key: String,
    #[serde(rename = "loop")]
    looped: bool,
}

#[derive(Serialize)]
struct Reply {
    npc_id: String,
    name_key: String,
    seq: i64,
    /// `false` once the conversation has ended; no node is shown then.
    open: bool,
    node: Option<Shown>,
    #[serde(skip_serializing_if = "Option::is_none")]
    portrait: Option<String>,
    /// Whether the character has a knowledge card to tell (`GET /story/npcs/{id}/about`).
    about: bool,
    /// Whether the character holds found goods the player can buy (`GET /npcs/{id}/found`).
    can_trade: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    music: Option<Music>,
    /// Visual duel / flourish from a dialogue effect (not persisted).
    #[serde(skip_serializing_if = "Option::is_none")]
    flourish: Option<Vec<disk::FlourishStep>>,
    player: players::Player,
}

async fn story_of(state: &AppState) -> Result<Arc<StoryWorld>, Error> {
    world::world(state)
        .await?
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "this world has no story"))
}

fn reject(reject: Reject) -> Error {
    match reject {
        Reject::Insufficient => status(StatusCode::CONFLICT, "not enough to pay"),
    }
}

/// Everything the engine needs to know about a player, read under row locks.
async fn load_state(tx: &mut Tx<'_>, player_id: i64) -> Result<PlayerState, Error> {
    let flags: Vec<String> =
        sqlx::query_scalar("SELECT flag FROM player_story_flags WHERE player_id = $1")
            .bind(player_id)
            .fetch_all(&mut **tx)
            .await?;
    let stages: Vec<(String, String)> =
        sqlx::query_as("SELECT quest_id, stage FROM player_quests WHERE player_id = $1")
            .bind(player_id)
            .fetch_all(&mut **tx)
            .await?;
    let items: Vec<(String, i64)> = sqlx::query_as(
        "SELECT item_id, quantity FROM player_inventory WHERE player_id = $1 FOR UPDATE",
    )
    .bind(player_id)
    .fetch_all(&mut **tx)
    .await?;
    let once: Vec<String> =
        sqlx::query_scalar("SELECT once_key FROM player_story_once WHERE player_id = $1")
            .bind(player_id)
            .fetch_all(&mut **tx)
            .await?;
    Ok(PlayerState {
        flags: flags.into_iter().collect(),
        stages: stages.into_iter().collect(),
        items: items.into_iter().collect(),
        once_done: once.into_iter().collect(),
    })
}

/// Commits the operations the engine produced. Any failure aborts the transaction.
async fn apply_ops(
    story: &StoryWorld,
    tx: &mut Tx<'_>,
    player_id: i64,
    ops: &[Op],
) -> Result<(), Error> {
    for op in ops {
        match op {
            Op::Give(item, quantity) => {
                crate::gathering::add_items(tx, player_id, &[(item.clone(), *quantity)]).await?;
            }
            Op::Take(item, quantity) => {
                let taken = sqlx::query("UPDATE player_inventory SET quantity = quantity - $3 WHERE player_id = $1 AND item_id = $2 AND quantity >= $3")
                    .bind(player_id)
                    .bind(item)
                    .bind(quantity)
                    .execute(&mut **tx)
                    .await?;
                if taken.rows_affected() != 1 {
                    return Err(status(StatusCode::CONFLICT, "not enough to pay"));
                }
                sqlx::query("DELETE FROM player_inventory WHERE player_id = $1 AND item_id = $2 AND quantity = 0")
                    .bind(player_id)
                    .bind(item)
                    .execute(&mut **tx)
                    .await?;
            }
            Op::Flag(flag) => {
                sqlx::query("INSERT INTO player_story_flags (player_id, flag) VALUES ($1, $2) ON CONFLICT DO NOTHING")
                    .bind(player_id)
                    .bind(flag)
                    .execute(&mut **tx)
                    .await?;
            }
            Op::Stage(quest, stage) => {
                sqlx::query("INSERT INTO player_quests (player_id, quest_id, stage) VALUES ($1, $2, $3) ON CONFLICT (player_id, quest_id) DO UPDATE SET stage = EXCLUDED.stage, updated_at = now()")
                    .bind(player_id)
                    .bind(quest)
                    .bind(stage)
                    .execute(&mut **tx)
                    .await?;
                crate::achievements::unlock(tx, player_id, "first_quest").await?;
            }
            Op::Flourish(_) => {}
            Op::Playable(playable) => {
                if !crate::players::valid_character(&playable.character) {
                    return Err(status(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "invalid character",
                    ));
                }
                let (model, disk) = match (&playable.model, &playable.disk) {
                    (Some(model), Some(disk)) => (Some(model.as_str()), Some(disk.as_str())),
                    (None, _) => (None, None),
                    (Some(_), None) => {
                        return Err(status(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "invalid playable",
                        ));
                    }
                };
                sqlx::query(
                    "UPDATE players SET character = $2, story_model = $3, story_model_disk = $4, story_role = $5 WHERE id = $1",
                )
                .bind(player_id)
                .bind(&playable.character)
                .bind(model)
                .bind(disk)
                .bind(playable.hide_npc.as_deref())
                .execute(&mut **tx)
                .await?;
            }
            Op::Once(key) => {
                sqlx::query("INSERT INTO player_story_once (player_id, once_key) VALUES ($1, $2)")
                    .bind(player_id)
                    .bind(key)
                    .execute(&mut **tx)
                    .await
                    .map_err(|_| status(StatusCode::CONFLICT, "reward already granted"))?;
            }
            Op::TeleportPlace(place) => {
                super::checkpoint::teleport_to_place(tx, story, player_id, place).await?;
            }
        }
    }
    Ok(())
}

/// Locks the player row and checks that the player stands near the NPC.
///
/// A walking character is where its route puts it now, and once a conversation is on it may have walked
/// on while the player reads: it then counts as near within `ONGOING_RANGE_M`.
async fn near_npc<'a>(
    state: &AppState,
    tx: &mut Tx<'_>,
    story: &'a StoryWorld,
    player_id: i64,
    npc_id: &str,
    ongoing: bool,
) -> Result<&'a world::NpcDescriptor, Error> {
    let position: Option<(Option<f64>, Option<f64>, Option<f64>)> = sqlx::query_as(
        "SELECT position_x, position_y, position_z FROM players WHERE id = $1 FOR UPDATE",
    )
    .bind(player_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((Some(x), Some(y), Some(z))) = position else {
        return Err(status(StatusCode::CONFLICT, "player position unavailable"));
    };
    let npc = story
        .npcs
        .iter()
        .find(|npc| npc.id == npc_id)
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "unknown character"))?;
    let here = world::position_now(state, npc).await;
    let range = if ongoing && npc.movement.is_some() {
        ONGOING_RANGE_M
    } else {
        TALK_RANGE_M
    };
    if world::distance([x, y, z], &here) > range {
        return Err(status(StatusCode::FORBIDDEN, "character is out of reach"));
    }
    Ok(npc)
}

/// A walking character stands still while it talks to a player (migration 0040); one that stands anyway is left alone.
async fn stand_still(
    state: &AppState,
    tx: &mut Tx<'_>,
    npc: &world::NpcDescriptor,
) -> Result<(), Error> {
    if npc.movement.is_some() {
        world::keep_standing(
            &mut **tx,
            state.world_id,
            &npc.id,
            crate::movement::unix_now_ms(),
        )
        .await?;
    }
    Ok(())
}

/// The conversation is over: the character walks on from where it stood.
async fn walk_on(
    state: &AppState,
    tx: &mut Tx<'_>,
    npc: &world::NpcDescriptor,
) -> Result<(), Error> {
    if npc.movement.is_some() {
        world::let_walk(
            &mut **tx,
            state.world_id,
            &npc.id,
            crate::movement::unix_now_ms(),
        )
        .await?;
    }
    Ok(())
}

/// The language of a request: two lowercase ASCII letters, anything else is ignored.
fn request_language(lang: &Option<String>) -> Option<&str> {
    lang.as_deref()
        .filter(|l| l.len() == 2 && l.bytes().all(|b| b.is_ascii_lowercase()))
}

fn take_flourish(ops: &[Op]) -> Option<Vec<disk::FlourishStep>> {
    ops.iter().find_map(|op| match op {
        Op::Flourish(steps) => Some(steps.clone()),
        _ => None,
    })
}

/// Offers a paid apology whenever this viewer has made the NPC hostile.
fn with_apology(mut shown: Shown, hostile: bool) -> Shown {
    if hostile
        && !shown
            .choices
            .iter()
            .any(|choice| choice.index == APOLOGY_CHOICE)
    {
        shown.choices.push(VisibleChoice {
            index: APOLOGY_CHOICE,
            text_key: APOLOGY_TEXT_KEY.to_owned(),
        });
    }
    shown
}

fn reply(
    story: &StoryWorld,
    npc: &world::NpcDescriptor,
    seq: i64,
    node: Option<Shown>,
    lang: Option<&str>,
    flourish: Option<Vec<disk::FlourishStep>>,
    player: players::Player,
) -> Reply {
    let node = node.map(|mut shown| {
        shown.voice = lang
            .and_then(|l| {
                story
                    .story
                    .voices
                    .get(&(l.to_owned(), shown.text_key.clone()))
            })
            .map(|file| world::media_url(file));
        shown
    });
    let dialogue = story
        .story
        .npcs
        .get(&npc.id)
        .and_then(|n| story.story.dialogues.get(&n.dialogue));
    let music = dialogue
        .and_then(|d| d.music.as_ref())
        .and_then(|id| story.story.music.get(id))
        .map(|(file, title_key, looped)| Music {
            url: world::media_url(file),
            title_key: title_key.clone(),
            looped: *looped,
        });
    Reply {
        npc_id: npc.id.clone(),
        name_key: npc.name_key.clone(),
        seq,
        open: node.is_some(),
        node,
        portrait: npc.portrait.clone(),
        about: story
            .story
            .npcs
            .get(&npc.id)
            .is_some_and(|found| found.about.is_some()),
        can_trade: false,
        music,
        flourish,
        player,
    }
}

async fn start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<Start>,
) -> Result<Json<Reply>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let story = story_of(&state).await?;
    advance_reach(&state, &story, player_id).await?;
    let mut tx = state.pool.begin().await?;
    let npc = near_npc(&state, &mut tx, &story, player_id, &request.npc_id, false).await?;
    let definition = &story.story.npcs[&npc.id];
    let dialogue = &story.story.dialogues[&definition.dialogue];
    let mut player_state = load_state(&mut tx, player_id).await?;
    let mut ops = Vec::new();
    let shown = engine::enter(dialogue, &dialogue.start, &mut player_state, &mut ops)
        .map_err(reject)?
        .ok_or_else(|| {
            status(
                StatusCode::INTERNAL_SERVER_ERROR,
                "dialogue has nothing to show",
            )
        })?;
    apply_ops(&story, &mut tx, player_id, &ops).await?;
    let flourish = take_flourish(&ops);
    let hostile = npc_combat::is_hostile(&state, player_id, &npc.id).await?;
    let shown = with_apology(shown, hostile);
    let seq: i64 = sqlx::query_scalar("INSERT INTO dialogue_sessions (player_id, npc_id, node_id) VALUES ($1, $2, $3) ON CONFLICT (player_id) DO UPDATE SET npc_id = EXCLUDED.npc_id, node_id = EXCLUDED.node_id, seq = dialogue_sessions.seq + 1, updated_at = now() RETURNING seq")
        .bind(player_id)
        .bind(&npc.id)
        .bind(&shown.node)
        .fetch_one(&mut *tx)
        .await?;
    stand_still(&state, &mut tx, npc).await?;
    tx.commit().await?;
    let player = players::profile(&state, player_id).await?;
    let mut answer = reply(
        &story,
        npc,
        seq,
        Some(shown),
        request_language(&request.lang),
        flourish,
        player,
    );
    answer.can_trade = crate::loot::npc_has_goods(&state, &npc.id).await?;
    Ok(Json(answer))
}

async fn choose(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ChooseRequest>,
) -> Result<Json<Reply>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let story = story_of(&state).await?;
    let mut tx = state.pool.begin().await?;
    let npc = near_npc(&state, &mut tx, &story, player_id, &request.npc_id, true).await?;
    let session: Option<(String, String, i64)> = sqlx::query_as(
        "SELECT npc_id, node_id, seq FROM dialogue_sessions WHERE player_id = $1 FOR UPDATE",
    )
    .bind(player_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((session_npc, node, seq)) = session else {
        return Err(status(StatusCode::CONFLICT, "no open conversation"));
    };
    if session_npc != npc.id || seq != request.seq {
        return Err(status(StatusCode::CONFLICT, "the conversation moved on"));
    }
    let dialogue = &story.story.dialogues[&story.story.npcs[&npc.id].dialogue];
    if request.choice == APOLOGY_CHOICE {
        if !npc_combat::is_hostile(&state, player_id, &npc.id).await? {
            return Err(status(StatusCode::CONFLICT, "npc not hostile"));
        }
        let gold = npc_combat::apology_gold();
        let taken = sqlx::query(
            "UPDATE player_inventory SET quantity = quantity - $2
             WHERE player_id = $1 AND item_id = 'gold' AND quantity >= $2",
        )
        .bind(player_id)
        .bind(gold)
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
        npc_combat::clear_hostility(&mut tx, player_id, &npc.id).await?;
        sqlx::query("DELETE FROM dialogue_sessions WHERE player_id = $1")
            .bind(player_id)
            .execute(&mut *tx)
            .await?;
        walk_on(&state, &mut tx, npc).await?;
        tx.commit().await?;
        let player = players::profile(&state, player_id).await?;
        return Ok(Json(reply(
            &story,
            npc,
            seq + 1,
            None,
            request_language(&request.lang),
            None,
            player,
        )));
    }
    let mut player_state = load_state(&mut tx, player_id).await?;
    let mut ops = Vec::new();
    let outcome = engine::choose(dialogue, &node, request.choice, &mut player_state, &mut ops);
    let flourish = take_flourish(&ops);
    let hostile = npc_combat::is_hostile(&state, player_id, &npc.id).await?;
    let (shown, next_seq) = match outcome {
        Choose::Unavailable => return Err(status(StatusCode::CONFLICT, "choice unavailable")),
        Choose::Rejected(why) => return Err(reject(why)),
        Choose::End => {
            apply_ops(&story, &mut tx, player_id, &ops).await?;
            sqlx::query("DELETE FROM dialogue_sessions WHERE player_id = $1")
                .bind(player_id)
                .execute(&mut *tx)
                .await?;
            walk_on(&state, &mut tx, npc).await?;
            (None, seq + 1)
        }
        Choose::Next(shown) => {
            apply_ops(&story, &mut tx, player_id, &ops).await?;
            sqlx::query("UPDATE dialogue_sessions SET node_id = $2, seq = seq + 1, updated_at = now() WHERE player_id = $1")
                .bind(player_id)
                .bind(&shown.node)
                .execute(&mut *tx)
                .await?;
            stand_still(&state, &mut tx, npc).await?;
            (Some(with_apology(shown, hostile)), seq + 1)
        }
    };
    tx.commit().await?;
    let player = players::profile(&state, player_id).await?;
    let mut answer = reply(
        &story,
        npc,
        next_seq,
        shown,
        request_language(&request.lang),
        flourish,
        player,
    );
    if answer.open {
        answer.can_trade = crate::loot::npc_has_goods(&state, &npc.id).await?;
    }
    Ok(Json(answer))
}

/// Closes the conversation without choosing (the player walked away or pressed escape).
async fn leave(State(state): State<AppState>, headers: HeaderMap) -> Result<StatusCode, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let left: Option<String> =
        sqlx::query_scalar("DELETE FROM dialogue_sessions WHERE player_id = $1 RETURNING npc_id")
            .bind(player_id)
            .fetch_optional(&state.pool)
            .await?;
    if let Some(npc_id) = left {
        world::let_walk(
            &state.pool,
            state.world_id,
            &npc_id,
            crate::movement::unix_now_ms(),
        )
        .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct QuestsQuery {
    /// Language of the client's UI (two lowercase letters): only selects the narrator line.
    /// It is never stored.
    #[serde(default)]
    lang: Option<String>,
}

#[derive(Serialize)]
struct QuestExcerptView {
    paraphrase_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    akkadian_key: Option<String>,
    /// Spoken paraphrase for the tablet intro, when a recording exists for `lang`.
    #[serde(skip_serializing_if = "Option::is_none")]
    voice: Option<String>,
}

#[derive(Serialize)]
struct QuestEntry {
    quest: String,
    title_key: String,
    stage: String,
    text_key: String,
    #[serde(rename = "final")]
    final_stage: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    excerpt: Option<QuestExcerptView>,
}

fn ui_lang(lang: Option<&str>) -> Option<&str> {
    lang.filter(|value| value.len() == 2 && value.chars().all(|c| c.is_ascii_lowercase()))
}

/// Moves quests on whose current stage asks the player to reach a place, when they have.
/// Each step is conditional on the stage it started from, so concurrent calls cannot repeat it.
async fn advance_reach(state: &AppState, story: &StoryWorld, player_id: i64) -> Result<(), Error> {
    let position: Option<(Option<f64>, Option<f64>, Option<f64>)> =
        sqlx::query_as("SELECT position_x, position_y, position_z FROM players WHERE id = $1")
            .bind(player_id)
            .fetch_optional(&state.pool)
            .await?;
    let Some((Some(x), Some(y), Some(z))) = position else {
        return Ok(());
    };
    // A short chain of reach stages may complete in one go.
    for _ in 0..4 {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT quest_id, stage FROM player_quests WHERE player_id = $1")
                .bind(player_id)
                .fetch_all(&state.pool)
                .await?;
        let mut moved = false;
        for (quest, stage) in rows {
            let Some(reach) = story
                .story
                .quests
                .get(&quest)
                .and_then(|q| q.stages.get(&stage))
                .and_then(|s| s.reach.as_ref())
            else {
                continue;
            };
            let Some(site) = story.sites.get(&reach.place) else {
                continue;
            };
            let at = world::site_position(site);
            let distance = ((x - at[0]).powi(2) + (y - at[1]).powi(2) + (z - at[2]).powi(2)).sqrt();
            if distance <= reach.radius_m {
                let done = sqlx::query("UPDATE player_quests SET stage = $4, updated_at = now() WHERE player_id = $1 AND quest_id = $2 AND stage = $3")
                    .bind(player_id)
                    .bind(&quest)
                    .bind(&stage)
                    .bind(&reach.goto)
                    .execute(&state.pool)
                    .await?;
                moved |= done.rows_affected() == 1;
            }
        }
        if !moved {
            break;
        }
    }
    Ok(())
}

/// What a character tells about itself: a knowledge card. Texts are keys of `/story/strings` and the picture is a
/// media path, so the language and the link to the article (`https://<language>.wikipedia.org/wiki/<title>`) are
/// the client's to choose; nothing is stored.
#[derive(Serialize)]
struct About {
    title_key: String,
    text_key: String,
    image: String,
    credit_key: String,
    wikipedia: std::collections::BTreeMap<String, String>,
}

async fn about(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(npc_id): Path<String>,
) -> Result<Json<About>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let story = story_of(&state).await?;
    let mut tx = state.pool.begin().await?;
    let npc = near_npc(&state, &mut tx, &story, player_id, &npc_id, false).await?;
    drop(tx);
    let card = story
        .story
        .npcs
        .get(&npc.id)
        .and_then(|found| found.about.as_ref())
        .and_then(|id| story.story.knowledge.get(id))
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "this character has nothing to tell"))?;
    Ok(Json(About {
        title_key: card.title_key.clone(),
        text_key: card.text_key.clone(),
        image: world::media_url(&card.image),
        credit_key: card.credit_key.clone(),
        wikipedia: card.wikipedia.clone(),
    }))
}

/// The player reads what a structure of a place carries (an inscription): the place's knowledge card, as
/// [`about`] answers it. Reading where the disk grants an achievement earns it in the same transaction, so it is
/// only earned by a player who stands there.
async fn read_place(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(place_id): Path<String>,
) -> Result<Json<About>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let story = story_of(&state).await?;
    let structure = story
        .story
        .anchors
        .iter()
        .find(|anchor| anchor.id == place_id)
        .and_then(|anchor| anchor.place.structure.as_ref())
        .filter(|structure| structure.about.is_some())
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "nothing to read here"))?;
    let card = structure
        .about
        .as_ref()
        .and_then(|id| story.story.knowledge.get(id))
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "nothing to read here"))?;
    // The structure as it was built: a place whose model could not be loaded has nothing to read.
    let at = story
        .props
        .iter()
        .find(|prop| prop.id == place_id && prop.readable)
        .map(|prop| prop.position)
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "nothing to read here"))?;
    let mut tx = state.pool.begin().await?;
    let position: Option<(Option<f64>, Option<f64>, Option<f64>)> = sqlx::query_as(
        "SELECT position_x, position_y, position_z FROM players WHERE id = $1 FOR UPDATE",
    )
    .bind(player_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((Some(x), Some(y), Some(z))) = position else {
        return Err(status(StatusCode::CONFLICT, "player position unavailable"));
    };
    let distance = ((x - at[0]).powi(2) + (y - at[1]).powi(2) + (z - at[2]).powi(2)).sqrt();
    if distance > READ_RANGE_M {
        return Err(status(StatusCode::FORBIDDEN, "too far away to read"));
    }
    if let Some(achievement) = &structure.achievement {
        crate::achievements::unlock_disk(&mut tx, &story.story, player_id, achievement).await?;
    }
    tx.commit().await?;
    Ok(Json(About {
        title_key: card.title_key.clone(),
        text_key: card.text_key.clone(),
        image: world::media_url(&card.image),
        credit_key: card.credit_key.clone(),
        wikipedia: card.wikipedia.clone(),
    }))
}

async fn quests(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<QuestsQuery>,
) -> Result<Json<Vec<QuestEntry>>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let story = story_of(&state).await?;
    advance_reach(&state, &story, player_id).await?;
    let lang = ui_lang(query.lang.as_deref());
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT quest_id, stage FROM player_quests WHERE player_id = $1 ORDER BY updated_at DESC",
    )
    .bind(player_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .filter_map(|(quest, stage)| {
                let definition = story.story.quests.get(&quest)?;
                let found = definition.stages.get(&stage)?;
                // Tablet prologue only at the opening stage (reconnect mid-quest must not re-open it).
                let excerpt = if stage == definition.start_stage {
                    definition.excerpt.as_ref().map(|excerpt| QuestExcerptView {
                        paraphrase_key: excerpt.paraphrase_key.clone(),
                        akkadian_key: excerpt.akkadian_key.clone(),
                        voice: lang.and_then(|l| {
                            story
                                .story
                                .voices
                                .get(&(l.to_owned(), excerpt.paraphrase_key.clone()))
                                .map(|file| world::media_url(file))
                        }),
                    })
                } else {
                    None
                };
                Some(QuestEntry {
                    title_key: definition.title_key.clone(),
                    text_key: found.text_key.clone(),
                    final_stage: found.final_stage,
                    excerpt,
                    quest,
                    stage,
                })
            })
            .collect(),
    ))
}

/// Where the player is pointed to: a place of an active quest or the start of one not yet begun.
#[derive(Serialize)]
pub(super) struct Marker {
    /// Qualified place id.
    pub(super) id: String,
    pub(super) quest: String,
    pub(super) name_key: String,
    pub(super) kind: String,
    /// World metres, like the positions of NPCs.
    pub(super) position: [f64; 3],
    /// The place where a quest not yet begun waits (instead of one of an active quest).
    pub(super) next: bool,
    /// A place the player already knows from a quest that has moved on (not the current goal).
    pub(super) known: bool,
}

/// The item that reveals markers for quests not yet begun (Faust / Endland intro).
const GUIDE_ITEM: &str = "aetherglass";

/// Markers of the player's quests. Active quest guides always show (the Gilgamesh hunter
/// never meets Faust). Markers for quests not yet started need the aetherglass.
async fn markers(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<Marker>>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let story = story_of(&state).await?;
    let holds: Option<(i64,)> = sqlx::query_as(
        "SELECT quantity FROM player_inventory WHERE player_id = $1 AND item_id = $2 AND quantity > 0",
    )
    .bind(player_id)
    .bind(GUIDE_ITEM)
    .fetch_optional(&state.pool)
    .await?;
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT quest_id, stage FROM player_quests WHERE player_id = $1")
            .bind(player_id)
            .fetch_all(&state.pool)
            .await?;
    let mut found = markers_of(&story.story, &story.sites, &rows);
    if holds.is_none() {
        found.retain(|marker| !marker.next);
    }
    // Every goal shown and every place where a begun quest started becomes known for good.
    let mut learned: Vec<(String, String)> = found
        .iter()
        .filter(|marker| !marker.next)
        .map(|marker| (marker.id.clone(), marker.quest.clone()))
        .collect();
    for (quest, _) in &rows {
        if let Some(place) = story
            .story
            .quests
            .get(quest)
            .and_then(|q| q.start_place.clone())
        {
            learned.push((place, quest.clone()));
        }
    }
    for (place, quest) in &learned {
        sqlx::query(
            "INSERT INTO player_known_places (player_id, place_id, quest_id) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(player_id)
        .bind(place)
        .bind(quest)
        .execute(&state.pool)
        .await?;
    }
    let known: Vec<(String, String)> = sqlx::query_as(
        "SELECT place_id, quest_id FROM player_known_places WHERE player_id = $1 ORDER BY learned_at",
    )
    .bind(player_id)
    .fetch_all(&state.pool)
    .await?;
    found.extend(known_markers(&story.story, &story.sites, &known, &found));
    found.truncate(64);
    Ok(Json(found))
}

/// Markers of known places (`(place, quest)`) that are not among the current ones.
pub(super) fn known_markers(
    story: &Story,
    sites: &HashMap<String, Placed>,
    known: &[(String, String)],
    current: &[Marker],
) -> Vec<Marker> {
    known
        .iter()
        .filter(|(place, _)| current.iter().all(|marker| &marker.id != place))
        .filter_map(|(place, quest)| {
            let site = sites.get(place)?;
            let anchor = story.anchors.iter().find(|anchor| &anchor.id == place)?;
            Some(Marker {
                id: place.clone(),
                quest: quest.clone(),
                name_key: anchor.place.name_key.clone(),
                kind: anchor.place.kind.clone(),
                position: world::site_position(site),
                next: false,
                known: true,
            })
        })
        .collect()
}

/// The places the quests point to, given the player's `(quest, stage)` rows.
pub(super) fn markers_of(
    story: &Story,
    sites: &HashMap<String, Placed>,
    rows: &[(String, String)],
) -> Vec<Marker> {
    let mut found = Vec::new();
    for (id, definition) in &story.quests {
        let current = rows.iter().find(|(quest, _)| quest == id);
        let (place, next) = match current {
            Some((_, stage)) => {
                let Some(stage) = definition.stages.get(stage) else {
                    continue;
                };
                if stage.final_stage {
                    continue;
                }
                let place = stage
                    .guide
                    .as_ref()
                    .or(stage.reach.as_ref().map(|reach| &reach.place));
                (place, false)
            }
            None => (definition.start_place.as_ref(), true),
        };
        let Some(place) = place else { continue };
        let (Some(site), Some(anchor)) = (
            sites.get(place),
            story.anchors.iter().find(|anchor| &anchor.id == place),
        ) else {
            continue;
        };
        found.push(Marker {
            id: place.clone(),
            quest: id.clone(),
            name_key: anchor.place.name_key.clone(),
            kind: anchor.place.kind.clone(),
            position: world::site_position(site),
            next,
            known: false,
        });
    }
    found.truncate(64);
    found
}

fn etag(value: &str) -> String {
    format!("\"{value}\"")
}

fn not_modified(headers: &HeaderMap, tag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == tag)
}

/// All translations of every applied disk. They are not selected by language, so every
/// language is handed out and the client picks (dialogue requests do name a language, but
/// only to choose the spoken line).
async fn strings(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, Error> {
    let story = story_of(&state).await?;
    let version = format!(
        "{:x}",
        Sha256::digest(
            story
                .story
                .disks
                .iter()
                .map(|(id, _, sha)| format!("{id}:{sha}"))
                .collect::<Vec<_>>()
                .join(",")
                .as_bytes()
        )
    );
    let tag = etag(&version);
    if not_modified(&headers, &tag) {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, tag)]).into_response());
    }
    let body = serde_json::json!({ "version": version, "languages": story.story.strings });
    Ok((
        [
            (header::ETAG, tag),
            (header::CACHE_CONTROL, "no-cache".to_owned()),
        ],
        Json(body),
    )
        .into_response())
}

/// Portraits and music of the applied disks. Only files the loaded disks name are served.
async fn media(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((disk, path)): Path<(String, String)>,
) -> Result<Response, Error> {
    let story = story_of(&state).await?;
    let key = format!("{disk}/{path}");
    let (file, sha256) = story
        .story
        .media
        .get(&key)
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "no such media"))?;
    let content_type = super::disk::media_type(&path)
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "no such media"))?;
    let tag = etag(sha256);
    if not_modified(&headers, &tag) {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, tag)]).into_response());
    }
    let bytes = tokio::fs::read(file)
        .await
        .map_err(|_| status(StatusCode::SERVICE_UNAVAILABLE, "media unavailable"))?;
    if format!("{:x}", Sha256::digest(&bytes)) != *sha256 {
        return Err(status(
            StatusCode::SERVICE_UNAVAILABLE,
            "media changed on disk",
        ));
    }
    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::ETAG, tag.parse().expect("hex digest"));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=3600"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}
