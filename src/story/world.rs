//! The story of one running world: the disks it was generated with, where their places
//! stand, and where their NPCs are. Built lazily from the database and cached.

use super::disk::MAN_HEIGHT_M;
pub use super::walking::Pause;
use super::{
    placement::{self, Placed},
    Story,
};
use crate::{
    movement::{self, length, RADIUS},
    players::Error,
    AppState,
};
use axum::http::StatusCode;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

/// Directory holding installed datadisks (`<dir>/<disk id>/datadisk.yaml`).
const DEFAULT_DISK_DIR: &str = "/usr/share/ishtaria/datadisks";
/// How long a built world is reused before the disk selection is read again.
const REFRESH: Duration = Duration::from_secs(30);
/// NPCs are sent to clients within this distance of the requested point.
const NPC_RADIUS_M: f64 = 400.0;
/// A player may talk to an NPC within this distance.
pub const TALK_RANGE_M: f64 = 6.0;

pub fn disk_dir() -> PathBuf {
    std::env::var_os("ISHTARIA_DATADISK_DIR")
        .map_or_else(|| PathBuf::from(DEFAULT_DISK_DIR), PathBuf::from)
}

/// An NPC as sent to clients near a position.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct NpcDescriptor {
    pub id: String,
    pub name_key: String,
    /// `<pack>/<skin>` of the Kenney character models.
    pub character: String,
    /// URL path of a glTF model drawn instead of `character`, when the NPC has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub position: [f64; 3],
    pub yaw: f64,
    pub scale_m: f64,
    /// URL path of the portrait, when the NPC has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub portrait: Option<String>,
    /// The shop of etc/shops.json this NPC keeps (tag `shop:<id>`), when it is a merchant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shop: Option<String>,
    /// How the character walks, when it does: not sent, the route is.
    #[serde(skip)]
    pub movement: Option<super::disk::Movement>,
    /// Unit direction of the place of the character (the origin of the waypoints of a patrol).
    #[serde(skip)]
    pub site: [f64; 3],
    /// Waypoints `[unix milliseconds, x, y, z]` of a walking character, from the start of the current slot
    /// on; `position` is where the character stands now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wander: Option<Vec<[f64; 4]>>,
    /// Clip names unlocked for this viewer (Easter eggs); empty for guests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unlocked_eggs: Vec<String>,
    /// Health bar for this viewer: hidden until the NPC is hurt or hostile.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<crate::npc_combat::HealthBar>,
}

/// A region around a place where a track plays; clients fade it in and out by distance.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct AreaDescriptor {
    pub id: String,
    pub position: [f64; 3],
    pub radius_m: f64,
    pub music: AreaMusic,
}

/// A landscape patch of a datadisk place near the client (ADR 0013): the client fetches the
/// height and water PNGs from `heightmap`/`water` and replaces the base terrain with them inside
/// `extent_m` around `position`, feathered across `feather_m`.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct TerrainDescriptor {
    pub place: String,
    pub position: [f64; 3],
    /// Ground height of the anchor: the patch's 0x8000 means exactly this.
    pub height_m: f64,
    pub extent_m: [f64; 2],
    pub offset_m: [f64; 2],
    pub feather_m: f64,
    /// Distance from the patch edge up to which the patch is announced.
    pub visible_m: f64,
    pub heightmap: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub water: Option<String>,
    /// Cleared / built ground for the minimap city silhouette (ADR 0013 clear mask).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clear: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct AreaMusic {
    pub url: String,
    pub title_key: String,
    #[serde(rename = "loop")]
    pub looped: bool,
}

pub struct StoryWorld {
    pub story: Story,
    /// Buildings and props of the places (towns, graveyards, harbours).
    pub props: Vec<super::settlements::WorldProp>,
    /// Where new characters appear, when a datadisk defines a spawn point.
    pub spawn: Option<[f64; 3]>,
    /// Persisted sites of the places, by qualified place id.
    pub sites: HashMap<String, Placed>,
    pub npcs: Vec<NpcDescriptor>,
    /// Places with background music.
    pub areas: Vec<AreaDescriptor>,
    /// Landscape patches of the places (ADR 0013), announced to clients within their reach.
    pub terrains: Vec<TerrainDescriptor>,
}

type Painted = Option<(String, Option<Arc<crate::environment::BiomeMap>>)>;
static PAINTED: Mutex<Painted> = Mutex::const_new(None);

/// The land biomes the world's datadisk paints, with a key naming the disks they come from (part of cache
/// keys, so a changed disk is not served from an old environment). `None` while no disk paints biomes.
///
/// It reads the applied disks itself instead of using [`world`]: placing the places needs the terrain,
/// the terrain needs these biomes, and the story is not built yet at that point.
pub async fn painted_biomes(
    state: &AppState,
) -> Option<(Arc<crate::environment::BiomeMap>, String)> {
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT disk_id, version, sha256 FROM world_datadisks WHERE world_id = $1 ORDER BY position",
    )
    .bind(state.world_id)
    .fetch_all(&state.pool)
    .await
    .ok()?;
    if rows.is_empty() {
        return None;
    }
    let key = format!(
        "{}:{}",
        state.world_id,
        rows.iter()
            .map(|(id, version, sha)| format!("{id}@{version}:{}", sha.as_deref().unwrap_or("")))
            .collect::<Vec<_>>()
            .join(",")
    );
    let mut cache = PAINTED.lock().await;
    if let Some((stored, map)) = cache.as_ref() {
        if stored == &key {
            return map.clone().map(|map| (map, key));
        }
    }
    let dir = disk_dir();
    let ids: Vec<String> = rows.into_iter().map(|(id, _, _)| id).collect();
    let map = tokio::task::spawn_blocking(move || {
        ids.iter().find_map(|id| {
            let disk = super::disk::Disk::load(&dir.join(id), false).ok()?;
            disk.world_biome_map().ok().flatten().map(Arc::new)
        })
    })
    .await
    .ok()?;
    *cache = Some((key.clone(), map.clone()));
    map.map(|map| (map, key))
}

/// Path clients use to fetch a media file of a disk.
pub fn media_url(media: &str) -> String {
    format!("/story/media/{media}")
}

type Cached = Option<(Instant, String, Option<Arc<StoryWorld>>)>;
static WORLD: Mutex<Cached> = Mutex::const_new(None);

fn unavailable() -> Error {
    Error::Status(StatusCode::SERVICE_UNAVAILABLE, "story unavailable")
}

/// A rebuild of the story world is running in the background.
static REFRESHING: AtomicBool = AtomicBool::new(false);

/// The world's story, or `None` when no datadisk was applied to it.
///
/// Building the story (reading and hashing the disks, parsing their models, placing buildings and loading
/// the landscape patches) takes seconds, more than a client waits for. So a story that is merely
/// stale is returned at once and rebuilt in the background; only the very first build, or one after a
/// failure, makes the request wait.
pub async fn world(state: &AppState) -> Result<Option<Arc<StoryWorld>>, Error> {
    let mut cache = WORLD.lock().await;
    let key = state.world_id.to_string();
    if let Some((at, stored, world)) = cache.as_ref() {
        if stored == &key {
            if at.elapsed() < REFRESH {
                return Ok(world.clone());
            }
            if world.is_some() {
                if !REFRESHING.swap(true, Ordering::AcqRel) {
                    let state = state.clone();
                    tokio::spawn(async move {
                        let built = compute(&state).await;
                        let mut cache = WORLD.lock().await;
                        // A failed rebuild keeps the story that works, and tries again after the next period.
                        let kept = cache.as_ref().and_then(|(_, _, world)| world.clone());
                        *cache = Some((
                            Instant::now(),
                            state.world_id.to_string(),
                            built.ok().flatten().or(kept),
                        ));
                        REFRESHING.store(false, Ordering::Release);
                    });
                }
                return Ok(world.clone());
            }
        }
    }
    match compute(state).await {
        Ok(built) => {
            *cache = Some((Instant::now(), key, built.clone()));
            Ok(built)
        }
        Err(error) => {
            eprintln!("story unavailable: {error:#}");
            // Do not rebuild on every request: remember the failure for a while.
            *cache = Some((Instant::now(), key, None));
            Err(unavailable())
        }
    }
}

/// Reads the disk selection of the world and builds its story.
async fn compute(state: &AppState) -> anyhow::Result<Option<Arc<StoryWorld>>> {
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT disk_id, version, sha256 FROM world_datadisks WHERE world_id = $1 ORDER BY position",
    )
    .bind(state.world_id)
    .fetch_all(&state.pool)
    .await?;
    if rows.is_empty() && super::settlements::count() == 0 {
        return Ok(None);
    }
    Ok(Some(Arc::new(build(state, rows).await?)))
}

async fn build(
    state: &AppState,
    rows: Vec<(String, String, Option<String>)>,
) -> anyhow::Result<StoryWorld> {
    let dir = disk_dir();
    let paths: Vec<PathBuf> = rows.iter().map(|(id, _, _)| dir.join(id)).collect();
    let terrain = movement::terrain(state)
        .await
        .map_err(|_| anyhow::anyhow!("terrain unavailable"))?;
    let heightmap = terrain.heightmap_sha256().to_owned();
    let seed = terrain.seed().to_owned();
    let mut extra = Vec::new();
    if super::settlements::count() > 0 {
        extra.push(super::settlements::world_disk(
            &seed,
            super::settlements::count(),
        ));
    }
    let mut story =
        tokio::task::spawn_blocking(move || Story::load_with(&paths, extra, false)).await??;
    let loaded: Vec<(String, String, String)> = story.disks.clone();
    for (id, version, sha256) in &rows {
        let found = loaded
            .iter()
            .find(|(loaded_id, _, _)| loaded_id == id)
            .filter(|(_, v, _)| v == version);
        let Some((_, _, actual)) = found else {
            anyhow::bail!("datadisk {id} differs from the one the world was generated with");
        };
        match sha256 {
            Some(pinned) => anyhow::ensure!(
                pinned == actual,
                "datadisk {id} changed after the world was generated"
            ),
            // The admin tool only records which disks to use; the first load pins their content.
            None => {
                sqlx::query("UPDATE world_datadisks SET sha256 = $3 WHERE world_id = $1 AND disk_id = $2 AND sha256 IS NULL")
                    .bind(state.world_id)
                    .bind(id)
                    .bind(actual)
                    .execute(&state.pool)
                    .await?;
            }
        }
    }
    let stored: Vec<(String, String, f64, f64, f64, f64)> = sqlx::query_as(
        "SELECT anchor_id, heightmap_sha256, direction_x, direction_y, direction_z, height_m FROM story_anchors WHERE world_id = $1",
    )
    .bind(state.world_id)
    .fetch_all(&state.pool)
    .await?;
    anyhow::ensure!(
        stored.iter().all(|row| row.1 == heightmap),
        "story places were placed on a different heightmap"
    );
    let mut sites: HashMap<String, Placed> = stored
        .into_iter()
        .map(|(id, _, x, y, z, height)| {
            (
                id.clone(),
                Placed {
                    id,
                    direction: [x, y, z],
                    height_m: height,
                },
            )
        })
        .collect();
    let existing: Vec<Placed> = sites.values().cloned().collect();
    let (placing_story, placing_terrain, placing_seed) =
        (story.clone(), terrain.clone(), seed.clone());
    let fresh = tokio::task::spawn_blocking(move || {
        placement::place_anchors(
            &placing_story,
            placing_terrain.as_ref(),
            &placing_seed,
            &existing,
        )
    })
    .await??;
    if !fresh.is_empty() {
        let mut transaction = state.pool.begin().await?;
        for site in &fresh {
            sqlx::query("INSERT INTO story_anchors (world_id, anchor_id, heightmap_sha256, direction_x, direction_y, direction_z, height_m) VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT DO NOTHING")
                .bind(state.world_id)
                .bind(&site.id)
                .bind(&heightmap)
                .bind(site.direction[0])
                .bind(site.direction[1])
                .bind(site.direction[2])
                .bind(site.height_m)
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        // Another process may have won the race; the stored rows are the truth.
        let rows: Vec<(String, f64, f64, f64, f64)> = sqlx::query_as(
            "SELECT anchor_id, direction_x, direction_y, direction_z, height_m FROM story_anchors WHERE world_id = $1",
        )
        .bind(state.world_id)
        .fetch_all(&state.pool)
        .await?;
        sites = rows
            .into_iter()
            .map(|(id, x, y, z, height)| {
                (
                    id.clone(),
                    Placed {
                        id,
                        direction: [x, y, z],
                        height_m: height,
                    },
                )
            })
            .collect();
    }
    // The landscape patches shape the ground before anything is put on it: structures, NPCs and the spawn stand on
    // the patched terrain, not on the base map below it.
    let (patch_story, patch_sites) = (story.clone(), sites.clone());
    let (patches, terrains) =
        tokio::task::spawn_blocking(move || load_patches(&patch_story, &patch_sites))
            .await
            .map_err(|_| anyhow::anyhow!("terrain patch loading panicked"))??;
    movement::set_patches(format!("{seed}:{heightmap}"), patches);
    let built = super::settlements::finish(&mut story, &mut sites, terrain.as_ref(), &seed)?;
    let props = built.props;
    let colliders = built.colliders;
    let built_bridges = built.bridges;
    let farms = built.farms;
    let npcs = npc_descriptors(&story, &sites, terrain.as_ref());
    let spawn = find_spawn(&story, &sites, &colliders, &npcs, terrain.as_ref(), &seed);
    let areas = area_descriptors(&story, &sites);
    movement::set_wildlife(
        format!("{seed}:{heightmap}"),
        wildlife_zones(&story, &sites),
    );
    movement::set_statics(format!("{seed}:{heightmap}"), colliders);
    movement::set_bridges(built_bridges);
    movement::set_farms(format!("{seed}:{heightmap}"), farms);
    Ok(StoryWorld {
        spawn,
        story,
        props,
        sites,
        npcs,
        areas,
        terrains,
    })
}

/// A free spot near the place a datadisk made the spawn point: inside its fence, clear of walls,
/// gravestones and characters. The first spawn place in the order of the disks wins.
pub fn find_spawn(
    story: &Story,
    sites: &HashMap<String, Placed>,
    colliders: &[movement::WorldObject],
    npcs: &[NpcDescriptor],
    ground: &dyn placement::Ground,
    seed: &str,
) -> Option<[f64; 3]> {
    let anchor = story
        .anchors
        .iter()
        .find(|anchor| anchor.place.spawn && sites.contains_key(&anchor.id))?;
    let site = &sites[&anchor.id];
    let reach = (anchor.place.radius_m.unwrap_or(20.0) * 0.4).clamp(3.0, 12.0);
    let mut random = crate::scenery::Rng::new(seed, &format!("spawn:{}", anchor.id));
    let mut fallback = None;
    let mut wet = None;
    for attempt in 0..400 {
        // Closer spots first, spreading outwards as the nearest ones turn out to be taken.
        let distance = 2.5 + (reach - 2.5) * f64::from(attempt) / 400.0 + random.between(0.0, 1.0);
        let angle = random.between(0.0, std::f64::consts::TAU);
        let (direction, position) = crate::scenery::world_from_local(
            site.direction,
            distance * angle.cos(),
            distance * angle.sin(),
            |direction| ground.height(direction).max(0.0),
            movement::RADIUS,
        );
        fallback.get_or_insert(position);
        let clear = |other: [f64; 3], radius: f64| {
            length(std::array::from_fn(|axis| position[axis] - other[axis])) > radius
        };
        if colliders
            .iter()
            .all(|c| clear(c.position, c.collision_radius_m + 1.2))
            && npcs.iter().all(|npc| clear(npc.position, 2.0))
        {
            // Dry ground first: a character put into water has to swim, and drowns.
            if ground.land_site(direction).is_some() {
                return Some(position);
            }
            wet.get_or_insert(position);
        }
    }
    wet.or(fallback)
}

/// Historical flora/fauna overlays of placed story sites.
pub fn wildlife_zones(
    story: &Story,
    sites: &HashMap<String, Placed>,
) -> Vec<movement::WildlifeZone> {
    story
        .anchors
        .iter()
        .filter_map(|anchor| {
            let wildlife = anchor.place.wildlife.as_ref()?;
            let site = sites.get(&anchor.id)?;
            Some(movement::WildlifeZone {
                direction: site.direction,
                radius_m: wildlife.radius_m.or(anchor.place.radius_m).unwrap_or(500.0),
                density: wildlife.density.unwrap_or(1.0),
                flora: wildlife
                    .flora
                    .iter()
                    .map(|entry| (entry.id.clone(), entry.weight, entry.waterside))
                    .collect(),
                fauna: wildlife
                    .fauna
                    .iter()
                    .map(|entry| (entry.id.clone(), entry.weight, entry.waterside))
                    .collect(),
            })
        })
        .collect()
}

/// NPCs stand on a ring around their place; the spot comes from a hash of the NPC id.
/// The places of the story that have background music.
pub fn area_descriptors(story: &Story, sites: &HashMap<String, Placed>) -> Vec<AreaDescriptor> {
    story
        .anchors
        .iter()
        .filter_map(|anchor| {
            let track = story.music.get(anchor.place.music.as_ref()?)?;
            let site = sites.get(&anchor.id)?;
            Some(AreaDescriptor {
                id: anchor.id.clone(),
                position: site_position(site),
                radius_m: anchor
                    .place
                    .music_radius_m
                    .or(anchor.place.radius_m)
                    .unwrap_or(40.0),
                music: AreaMusic {
                    url: media_url(&track.0),
                    title_key: track.1.clone(),
                    looped: track.2,
                },
            })
        })
        .collect()
}

pub fn npc_descriptors(
    story: &Story,
    sites: &HashMap<String, Placed>,
    ground: &dyn placement::Ground,
) -> Vec<NpcDescriptor> {
    let mut npcs = Vec::new();
    for npc in story.npcs.values() {
        let (Some(site), Some(anchor)) = (
            sites.get(&npc.place),
            story.anchors.iter().find(|anchor| anchor.id == npc.place),
        ) else {
            continue;
        };
        let reach = (anchor.place.radius_m.unwrap_or(20.0) * 0.4).clamp(3.0, 12.0);
        // The first offset is the character's own place; if it is in water, a few more are tried
        // so that a surveyed point on a river bank does not stand its people in the stream.
        let mut chosen = None;
        for attempt in 0..16 {
            let key = if attempt == 0 {
                format!("ishtaria-npc:{}", npc.id)
            } else {
                format!("ishtaria-npc:{}:{attempt}", npc.id)
            };
            let hash = Sha256::digest(key.as_bytes());
            let sample = |index: usize| {
                f64::from(u32::from_le_bytes(
                    hash[index * 4..index * 4 + 4].try_into().unwrap(),
                )) / 4_294_967_296.0
            };
            let direction = placement::offset(
                site.direction,
                2.0 + sample(0) * (reach - 2.0),
                sample(1) * std::f64::consts::TAU,
            );
            let candidate = (direction, sample(2));
            let dry = ground.land_site(direction).is_some();
            chosen.get_or_insert(candidate);
            if dry {
                chosen = Some(candidate);
                break;
            }
        }
        let (direction, yaw_sample) = chosen.expect("at least one attempt");
        let height = ground.height(direction).max(0.0);
        npcs.push(NpcDescriptor {
            id: npc.id.clone(),
            name_key: npc.name_key.clone(),
            character: format!("{}/{}", npc.character.pack, npc.character.skin),
            model: npc.character.model.as_deref().map(media_url),
            position: direction.map(|axis| axis * (RADIUS + height)),
            yaw: yaw_sample * std::f64::consts::TAU,
            scale_m: MAN_HEIGHT_M * npc.size.unwrap_or(1.0),
            portrait: npc.portrait.as_deref().map(media_url),
            shop: npc
                .tags
                .iter()
                .find_map(|tag| tag.strip_prefix("shop:"))
                .map(str::to_owned),
            movement: npc.movement.clone(),
            site: site.direction,
            wander: None,
            unlocked_eggs: Vec::new(),
            health: None,
        });
    }
    npcs
}

/// The character as it is at `now_ms`: a walking one gets its route and stands where the route puts it.
/// `home` is where it was put, unit direction `position / |position|`. While it talks to a player (`pause`, migration 0040)
/// the time of its walk stands still: it stands where it is, and its route goes on from there afterwards, shifted by the time it stood.
pub fn walking(
    ground: &dyn placement::Ground,
    npc: &NpcDescriptor,
    now_ms: i64,
    pause: Option<&Pause>,
) -> NpcDescriptor {
    let Some(movement) = &npc.movement else {
        return npc.clone();
    };
    let (clock, standing) = pause.map_or((now_ms, false), |pause| pause.clock(now_ms));
    let offset = pause.map_or(0, |pause| pause.offset_ms(now_ms)) as f64;
    let home = crate::movement::unit(npc.position);
    let route = super::walking::route(ground, &npc.id, home, npc.site, movement, clock);
    let mut here = npc.clone();
    if route.is_empty() {
        return here;
    }
    here.position = super::walking::position_at(&route, npc.position, clock);
    here.wander = Some(if standing {
        // Two waypoints one place: clients stand it there until they ask again.
        let [x, y, z] = here.position;
        vec![
            [now_ms as f64, x, y, z],
            [(now_ms + STANDING_ROUTE_MS) as f64, x, y, z],
        ]
    } else {
        route
            .into_iter()
            .map(|[time, x, y, z]| [time + offset, x, y, z])
            .collect()
    });
    here
}

/// How far ahead the route of a character that stands in a conversation reaches.
const STANDING_ROUTE_MS: i64 = 90_000;
/// How long an answer keeps a conversation's stop open (leaving the conversation ends it at once).
pub const KEEP_STANDING_MS: i64 = 5 * 60_000;

/// The stops of the characters in conversation (migration 0040), by character; empty when they cannot be read, so that the
/// characters simply walk on.
async fn pauses(state: &AppState) -> HashMap<String, Pause> {
    let rows: Result<Vec<(String, i64, i64, i64)>, sqlx::Error> = sqlx::query_as(
        "SELECT npc_id, shift_ms, pause_from_ms, pause_to_ms FROM npc_pauses WHERE world_id = $1",
    )
    .bind(state.world_id)
    .fetch_all(&state.pool)
    .await;
    rows.map(|rows| {
        rows.into_iter()
            .map(|(id, shift_ms, from_ms, to_ms)| {
                (
                    id,
                    Pause {
                        shift_ms,
                        from_ms,
                        to_ms,
                    },
                )
            })
            .collect()
    })
    .unwrap_or_default()
}

/// A character talks to a player: it stands still from `now_ms` for a few minutes more (every answer renews this). A stop
/// that has already ended is added to the time the character has stood, and a new one begins.
pub async fn keep_standing(
    executor: impl sqlx::PgExecutor<'_>,
    world_id: i64,
    npc_id: &str,
    now_ms: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO npc_pauses (world_id, npc_id, shift_ms, pause_from_ms, pause_to_ms) VALUES ($1, $2, 0, $3, $4) \
         ON CONFLICT (world_id, npc_id) DO UPDATE SET \
         shift_ms = CASE WHEN npc_pauses.pause_to_ms > $3 THEN npc_pauses.shift_ms ELSE npc_pauses.shift_ms + (npc_pauses.pause_to_ms - npc_pauses.pause_from_ms) END, \
         pause_from_ms = CASE WHEN npc_pauses.pause_to_ms > $3 THEN npc_pauses.pause_from_ms ELSE $3 END, \
         pause_to_ms = $4",
    )
    .bind(world_id)
    .bind(npc_id)
    .bind(now_ms)
    .bind(now_ms + KEEP_STANDING_MS)
    .execute(executor)
    .await?;
    Ok(())
}

/// The conversation is over: the character walks on from where it stands.
pub async fn let_walk(
    executor: impl sqlx::PgExecutor<'_>,
    world_id: i64,
    npc_id: &str,
    now_ms: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE npc_pauses SET pause_to_ms = greatest(pause_from_ms, least(pause_to_ms, $3)) WHERE world_id = $1 AND npc_id = $2",
    )
    .bind(world_id)
    .bind(npc_id)
    .bind(now_ms)
    .execute(executor)
    .await?;
    Ok(())
}

/// Where the character is now (its place for one that stands).
pub async fn position_now(state: &AppState, npc: &NpcDescriptor) -> NpcDescriptor {
    if npc.movement.is_none() {
        return npc.clone();
    }
    match movement::terrain(state).await {
        Ok(terrain) => {
            let pauses = pauses(state).await;
            walking(&*terrain, npc, movement::unix_now_ms(), pauses.get(&npc.id))
        }
        Err(_) => npc.clone(),
    }
}

/// Props of places near a point (buildings, graveyard fences, ships). Like NPCs, they never
/// disturb the rest of the world when the story is unavailable.
pub async fn props_near(state: &AppState, point: [f64; 3]) -> Vec<super::settlements::WorldProp> {
    const PROP_RADIUS_M: f64 = 450.0;
    const MAX_PROPS: usize = 2500;
    // Buildings of datadisks are few and large, so they are announced from farther away.
    match world(state).await {
        Ok(Some(world)) => world
            .props
            .iter()
            .filter(|prop| {
                length(std::array::from_fn(|axis| {
                    prop.position[axis] - point[axis]
                })) <= prop.visible_m.unwrap_or(PROP_RADIUS_M)
            })
            .take(MAX_PROPS)
            .cloned()
            .collect(),
        _ => Vec::new(),
    }
}

/// Where new characters appear, if a datadisk defines a spawn point.
pub async fn spawn_position(state: &AppState) -> Option<[f64; 3]> {
    world(state)
        .await
        .ok()
        .flatten()
        .and_then(|world| world.spawn)
}

/// The character a datadisk gives every new player, if one does; the first disk in the order of
/// application wins.
pub async fn player_character(state: &AppState) -> Option<String> {
    world(state).await.ok().flatten().and_then(|world| {
        world
            .story
            .infos
            .iter()
            .find_map(|info| info.player_character.clone())
    })
}

/// Grants disk `starting_items` and begins `auto_start_quests` for a newly created character.
/// Safe to call when the world has no story (then it does nothing). Never regrants on login.
pub async fn apply_new_player(
    state: &AppState,
    player_id: i64,
) -> Result<(), crate::players::Error> {
    let Some(world) = world(state).await? else {
        return Ok(());
    };
    if world.story.starting_items.is_empty() && world.story.auto_start_quests.is_empty() {
        return Ok(());
    }
    let mut transaction = state.pool.begin().await?;
    if !world.story.starting_items.is_empty() {
        crate::gathering::add_items(&mut transaction, player_id, &world.story.starting_items)
            .await?;
    }
    for (quest, stage) in &world.story.auto_start_quests {
        sqlx::query(
            "INSERT INTO player_quests (player_id, quest_id, stage) VALUES ($1, $2, $3) ON CONFLICT (player_id, quest_id) DO NOTHING",
        )
        .bind(player_id)
        .bind(quest)
        .bind(stage)
        .execute(&mut *transaction)
        .await?;
    }
    transaction.commit().await?;
    Ok(())
}

/// The body the datadisk of the world gives the players, as (character, body); the first disk that names a
/// character decides. A character of another look is an ordinary person.
pub async fn player_body(state: &AppState) -> Option<(String, super::disk::PlayerBody)> {
    let world = world(state).await.ok().flatten()?;
    let info = world
        .story
        .infos
        .iter()
        .find(|info| info.player_character.is_some())?;
    Some((info.player_character.clone()?, info.player_body?))
}

/// NPCs near a point. A broken or missing story never disturbs the rest of the world.
/// When `player_id` is set, each NPC carries Easter-egg clip names unlocked for that player.
pub async fn npcs_near(
    state: &AppState,
    point: [f64; 3],
    player_id: Option<i64>,
) -> Vec<NpcDescriptor> {
    let Ok(Some(world)) = world(state).await else {
        return Vec::new();
    };
    // A walking character may be far from where it was put.
    let reach = |npc: &NpcDescriptor| {
        NPC_RADIUS_M
            + npc.movement.as_ref().map_or(0.0, |movement| {
                movement
                    .waypoints
                    .iter()
                    .fold(movement.radius_m, |far, spot| {
                        far.max(spot[0].hypot(spot[1]))
                    })
                    * 2.0
            })
    };
    let near: Vec<&NpcDescriptor> = world
        .npcs
        .iter()
        .filter(|npc| {
            length(std::array::from_fn(|axis| npc.position[axis] - point[axis])) <= reach(npc)
        })
        .collect();
    let terrain = if near.iter().any(|npc| npc.movement.is_some()) {
        movement::terrain(state).await.ok()
    } else {
        None
    };
    let now = movement::unix_now_ms();
    let pauses = if terrain.is_some() {
        pauses(state).await
    } else {
        HashMap::new()
    };
    let egg_ctx = match player_id {
        Some(id) if !world.story.easter_eggs.is_empty() => {
            super::eggs::EggContext::load(state, id).await.ok()
        }
        _ => None,
    };
    let hidden_role: Option<String> = match player_id {
        Some(id) => sqlx::query_scalar("SELECT story_role FROM players WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.pool)
            .await
            .ok()
            .flatten(),
        None => None,
    };
    // Escorted companion (Šamḫat) follows the player even far from her temple site.
    let escort = match player_id {
        Some(id) => crate::escort::active_for(state, id).await.ok().flatten(),
        None => None,
    };
    // Love minigame: wild Enkidu weaves at the water-hole (server position).
    let love = match player_id {
        Some(id) => crate::love::active_for(state, id).await.ok().flatten(),
        None => None,
    };
    let player_pos: Option<[f64; 3]> = match player_id {
        Some(id) if escort.is_some() => sqlx::query_as(
            "SELECT position_x, position_y, position_z FROM players WHERE id = $1 AND position_x IS NOT NULL",
        )
        .bind(id)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten()
        .map(|(x, y, z): (f64, f64, f64)| [x, y, z]),
        _ => None,
    };
    let escort_companion = escort.as_ref().map(|row| row.companion_id.as_str());
    let mut listed: Vec<NpcDescriptor> = near
        .into_iter()
        .filter(|npc| hidden_role.as_deref() != Some(npc.id.as_str()))
        .map(|npc| {
            let mut described = match &terrain {
                Some(terrain) => walking(&**terrain, npc, now, pauses.get(&npc.id)),
                None => npc.clone(),
            };
            if let Some(ctx) = &egg_ctx {
                described.unlocked_eggs = super::eggs::unlocked_clips(
                    &world.story.easter_eggs,
                    &described.id,
                    described.position,
                    ctx,
                );
            }
            described
        })
        .collect();
    if let (Some(companion_id), Some(player)) = (escort_companion, player_pos) {
        if hidden_role.as_deref() != Some(companion_id) {
            let follow = crate::escort::follow_position(player);
            if let Some(index) = listed.iter().position(|npc| npc.id == companion_id) {
                listed[index].position = follow;
                listed[index].wander = None;
                listed[index].movement = None;
            } else if let Some(home) = world.npcs.iter().find(|npc| npc.id == companion_id) {
                let mut described = home.clone();
                described.position = follow;
                described.wander = None;
                described.movement = None;
                if let Some(ctx) = &egg_ctx {
                    described.unlocked_eggs = super::eggs::unlocked_clips(
                        &world.story.easter_eggs,
                        &described.id,
                        described.position,
                        ctx,
                    );
                }
                listed.push(described);
            }
        }
    }
    if let Some(love) = love {
        if let Some(index) = listed
            .iter()
            .position(|npc| crate::love::is_wild_enkidu(&npc.id))
        {
            listed[index].position = love.enkidu;
            listed[index].wander = None;
            listed[index].movement = None;
        } else if let Some(home) = world
            .npcs
            .iter()
            .find(|npc| crate::love::is_wild_enkidu(&npc.id))
        {
            if hidden_role.as_deref() != Some(home.id.as_str()) {
                let mut described = home.clone();
                described.position = love.enkidu;
                described.wander = None;
                described.movement = None;
                listed.push(described);
            }
        }
    }
    if let Some(id) = player_id {
        for npc in &mut listed {
            if let Ok(Some((at, yaw))) =
                crate::npc_combat::chase_pose(state, id, &npc.id, player_pos).await
            {
                npc.position = at;
                npc.yaw = yaw;
                npc.wander = None;
                npc.movement = None;
            }
            if let Ok(Some(bar)) = crate::npc_combat::bar_for(state, id, &npc.id).await {
                npc.health = Some(bar);
            }
        }
    }
    // Wayfarers carrying a find divert along the road toward their merchant.
    let errands = crate::loot::errands(state).await;
    if !errands.is_empty() {
        for npc in &mut listed {
            let Some(merchant_id) = errands.get(&npc.id) else {
                continue;
            };
            let Some(merchant) = world
                .npcs
                .iter()
                .find(|candidate| candidate.id == *merchant_id)
            else {
                continue;
            };
            let dest = position_now(state, merchant).await;
            let route = crate::loot::errand_wander(npc, &dest, now);
            npc.position = super::walking::position_at(&route, npc.position, now);
            npc.wander = Some(route);
        }
    }
    listed
}

/// Music areas whose region is near a point (the client works out the distance itself).
pub async fn areas_near(state: &AppState, point: [f64; 3]) -> Vec<AreaDescriptor> {
    match world(state).await {
        Ok(Some(world)) => world
            .areas
            .iter()
            .filter(|area| {
                length(std::array::from_fn(|axis| {
                    area.position[axis] - point[axis]
                })) <= area.radius_m + NPC_RADIUS_M
            })
            .cloned()
            .collect(),
        _ => Vec::new(),
    }
}

/// Position of a place in world metres.
pub fn site_position(site: &Placed) -> [f64; 3] {
    site.direction.map(|axis| axis * (RADIUS + site.height_m))
}

/// Reads the landscape patches of the placed places from the disks' media (ADR 0013): the
/// heightmap and the water mask (0 = dry land), both 16-bit greyscale PNGs whose 0x8000 is the
/// anchor's height and whose sha256 must still match the composed story.
fn load_patches(
    story: &Story,
    sites: &HashMap<String, Placed>,
) -> anyhow::Result<(Vec<movement::TerrainPatch>, Vec<TerrainDescriptor>)> {
    let mut patches = Vec::new();
    let mut descriptors = Vec::new();
    for anchor in &story.anchors {
        let Some(terrain) = &anchor.place.terrain else {
            continue;
        };
        let Some(site) = sites.get(&anchor.id) else {
            eprintln!("terrain patch {}: its place has no anchor", anchor.id);
            continue;
        };
        let Some((disk, _)) = anchor.id.split_once(':') else {
            continue;
        };
        let read = |path: &str| -> anyhow::Result<(Vec<u16>, usize, usize)> {
            let (file, sha256) = story
                .media
                .get(&format!("{disk}/{path}"))
                .ok_or_else(|| anyhow::anyhow!("{path} is not a media file of the disk"))?;
            let bytes = std::fs::read(file)?;
            anyhow::ensure!(
                format!("{:x}", Sha256::digest(&bytes)) == *sha256,
                "{path} changed on disk"
            );
            let image = image::load_from_memory(&bytes)
                .map_err(|error| anyhow::anyhow!("{path}: {error:#}"))?
                .into_luma16();
            let (width, height) = image.dimensions();
            anyhow::ensure!(
                width >= 16 && height >= 16 && width * height <= 4 << 20,
                "{path} has invalid dimensions {width}x{height}"
            );
            Ok((image.into_raw(), width as usize, height as usize))
        };
        let (elevation, width, height) = match read(&terrain.heightmap) {
            Ok(loaded) => loaded,
            Err(error) => {
                eprintln!("terrain patch {}: {error:#}", anchor.id);
                continue;
            }
        };
        let elevation: Vec<i16> = elevation
            .into_iter()
            .map(|value| (i32::from(value) - 32768) as i16)
            .collect();
        let water = match &terrain.water {
            Some(path) => match read(path) {
                Ok((water, w, h)) if (w, h) == (width, height) => Some(
                    water
                        .into_iter()
                        .map(|value| (i32::from(value) - 32768) as i16)
                        .collect(),
                ),
                Ok(_) => {
                    eprintln!(
                        "terrain patch {}: water mask has a different size",
                        anchor.id
                    );
                    None
                }
                Err(error) => {
                    eprintln!("terrain patch {}: {error:#}", anchor.id);
                    None
                }
            },
            None => None,
        };
        // The mask of cleared ground has a size of its own (a street is a few metres wide).
        let clear = match &terrain.clear {
            Some(path) => match read(path) {
                Ok((pixels, w, h)) => Some(movement::ClearMask {
                    width: w,
                    height: h,
                    pixels: pixels.into_iter().map(|value| value > 0).collect(),
                }),
                Err(error) => {
                    eprintln!("terrain patch {}: {error:#}", anchor.id);
                    None
                }
            },
            None => None,
        };
        let (extent, feather) = (terrain.extent_m, terrain.feather_m.unwrap_or(100.0));
        let offset = terrain.offset_m.unwrap_or([0.0, 0.0]);
        let visible = terrain.visible_m.unwrap_or(1200.0);
        patches.push(movement::TerrainPatch {
            anchor: site.direction,
            anchor_height_m: site.height_m,
            extent_m: extent,
            offset_m: offset,
            feather_m: feather,
            width,
            height,
            elevation,
            water: water.clone(),
            clear,
        });
        descriptors.push(TerrainDescriptor {
            place: anchor.id.clone(),
            position: site_position(site),
            height_m: site.height_m,
            extent_m: extent,
            offset_m: offset,
            feather_m: feather,
            visible_m: visible,
            heightmap: media_url(&format!("{disk}/{}", terrain.heightmap)),
            water: terrain
                .water
                .as_ref()
                .map(|path| media_url(&format!("{disk}/{path}"))),
            clear: terrain
                .clear
                .as_ref()
                .map(|path| media_url(&format!("{disk}/{path}"))),
        });
    }
    Ok((patches, descriptors))
}

/// Landscape patches whose rectangle lies within reach of a point, for the client.
pub async fn terrains_near(state: &AppState, point: [f64; 3]) -> Vec<TerrainDescriptor> {
    match world(state).await {
        Ok(Some(world)) => world
            .terrains
            .iter()
            .filter(|terrain| {
                let reach = 0.5 * (terrain.extent_m[0].hypot(terrain.extent_m[1]))
                    + terrain.feather_m
                    + terrain.visible_m;
                length(std::array::from_fn(|axis| {
                    terrain.position[axis] - point[axis]
                })) <= reach
            })
            .cloned()
            .collect(),
        _ => Vec::new(),
    }
}

/// Distance in metres between a player and an NPC.
pub fn distance(player: [f64; 3], npc: &NpcDescriptor) -> f64 {
    length(std::array::from_fn(|axis| {
        player[axis] - npc.position[axis]
    }))
}

/// Forgets the cached world (tests use a fresh database per test).
#[cfg(test)]
pub async fn reset_cache() {
    *WORLD.lock().await = None;
}
