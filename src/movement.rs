//! Server-authoritative movement: walking, running and jumping projected onto the terrain,
//! collision with generated objects, and the generated objects, animals and their routes that
//! `GET /world/objects` sends to clients.

use super::{environment, players, AppState};
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use parry2d_f64::{
    na::{Isometry2, Vector2},
    query::{cast_shapes, ShapeCastOptions},
    shape::Ball,
};
use parry3d_f64::{
    na::{Isometry3, Point3, Translation3, UnitQuaternion, Vector3},
    query::{cast_shapes as cast_shapes3, contact, ShapeCastOptions as ShapeCastOptions3},
    shape::{Capsule, TriMesh},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::FromRow;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, OnceLock},
};
use tokio::sync::Mutex;

#[cfg(test)]
use parry3d_f64::query::{Ray, RayCast};

pub(super) const RADIUS: f64 = 6_371_000.0;
const WALK_SPEED: f64 = 4.0;
const RUN_SPEED: f64 = 6.0;
/// A character in water swims at this share of the walking speed and cannot run or jump.
const SWIM_SPEED_FACTOR: f64 = 0.4;
/// Water this deep (metres) reaches a character's chest: up to it they wade on the bed, beyond it they swim and drown.
pub(super) const WADE_DEPTH_M: f64 = 1.2;
/// Wading is slowed by the resistance of the water; swimming is slower still.
const WADE_SPEED_FACTOR: f64 = 0.6;
/// Nobody runs in water deeper than this (metres).
pub(super) const RUN_DEPTH_M: f64 = 0.3;
pub(super) const OBJECT_GRID: i32 = 600_000;
/// Animals live on a coarser grid: about one cell per 170 metres.
const FAUNA_GRID: i32 = 60_000;
const FAUNA_PRESENCE: f64 = 0.35;
/// How far from a patch's water (metres) a wildlife overlay still gathers game and lush growth.
const WATERSIDE_REACH_M: f64 = 80.0;
/// Land animals this close to patch water join the drink/graze cycle instead of milling in place.
const WATERHOLE_REACH_M: f64 = 130.0;
/// How far from the bank a waterhole animal grazes among the grass (metres).
const WATERHOLE_GRAZE_MIN_M: f64 = 28.0;
const WATERHOLE_GRAZE_MAX_M: f64 = 78.0;
/// Walking speed of a waterhole drink cycle, metres per second.
const WATERHOLE_SPEED_M_S: f64 = 1.4;
/// How long it stands at the bank drinking, and how long it grazes before the next walk to water.
const WATERHOLE_DRINK_MS: i64 = 22_000;
const WATERHOLE_GRAZE_MS: i64 = 95_000;
/// Waypoints of a waterhole route: one every step, covering about a minute and a half.
const WATERHOLE_STEP_MS: i64 = 6_000;
const WATERHOLE_POINTS: i64 = 16;
/// Steepest bank (m/m, rise over run) that still grows lush plants; steeper is stony and bare.
const LUSH_BANK_SLOPE: f64 = 0.18;
/// Fish are only placed where the water is at least this deep.
const MIN_WATER_DEPTH_M: f64 = 3.0;
/// Reeds, papyrus and cattails stand on the bed only in this depth band (metres).
const REED_MIN_DEPTH_M: f64 = 0.15;
const REED_MAX_DEPTH_M: f64 = WADE_DEPTH_M;
/// Animals are sent to clients only within this distance of the requested point.
const FAUNA_RADIUS_M: f64 = 280.0;
/// Land animals walk between waypoints that depend only on their id and the time: a slot lasts
/// 30 seconds, the first 21 of which are spent walking to the next waypoint.
const WANDER_SLOT_MS: i64 = 30_000;
const WANDER_MOVE_MS: i64 = 21_000;
const WANDER_PARTS: i64 = 3;
/// Slots of the route sent to clients; they ask again before it runs out.
const WANDER_SLOTS: i64 = 3;
/// How far a free animal strays from where the seed put it, and a farm animal from its pasture.
const WILD_WANDER_M: f64 = 40.0;
/// A struck animal that does not fight back runs this far from the hunter, in this time, rests where it
/// stopped and then walks back into its usual wandering.
const FLEE_DISTANCE_M: f64 = 45.0;
const FLEE_RUN_MS: i64 = 6_000;
const FLEE_REST_MS: i64 = 60_000;
const FLEE_RETURN_MS: i64 = 40_000;

/// Animals fleeing from a hunter: id -> (start, where it was struck, where it runs to). In memory only:
/// a flight lasts under two minutes and a restart just ends it.
#[allow(clippy::type_complexity)]
static FLEES: std::sync::Mutex<Option<HashMap<String, (i64, [f64; 3], [f64; 3])>>> =
    std::sync::Mutex::new(None);
/// Birds circle farther than herds; they stay above the same seed cell.
const FLY_WANDER_M: f64 = 70.0;
const FARM_WANDER_M: f64 = 14.0;
/// Animals sent around each settlement and the room reserved for them in a reply.
const FARM_ANIMALS_MAX: usize = 64;
const FARM_RANGE_M: f64 = 400.0;
/// Share of goats in a flock; the rest are sheep.
const GOAT_SHARE: f64 = 0.3;
/// How far a flock animal keeps from the centre of its herd, metres.
const FLOCK_SPREAD_M: f64 = 9.0;
/// Waypoints of a flock animal's route: one every step, covering two minutes.
const FLOCK_STEP_MS: i64 = 10_000;
const FLOCK_POINTS: i64 = 13;
/// Whether a catalogued model is an animal that walks, grazes or flies over land: the Quaternius animals and the
/// ones of 0 A.D. (whose fish swims and is looked up as a fish).
pub(super) fn is_land_animal(model: &str) -> bool {
    model.starts_with("animal.") || (model.starts_with("zeroad.") && model != "zeroad.fish_generic")
}

/// Models of the animals of a pasture, with their relative frequency.
const FARM_SPECIES: [(&str, f64); 19] = [
    ("animal.chicken", 4.0),
    ("animal.sheep", 4.0),
    ("animal.pig", 3.0),
    ("animal.cow", 5.0),
    ("animal.bull", 1.0),
    ("animal.horse", 2.0),
    ("animal.white_horse", 1.0),
    ("animal.donkey", 2.0),
    ("animal.alpaca", 1.0),
    ("animal.shiba_inu", 1.0),
    // The domestic animals of 0 A.D. (CC BY-SA 3.0), for variety beside the Quaternius ones.
    ("zeroad.chicken", 3.0),
    ("zeroad.pig", 2.0),
    ("zeroad.cow", 3.0),
    ("zeroad.bull", 1.0),
    ("zeroad.zebu", 1.0),
    ("zeroad.donkey", 1.0),
    ("zeroad.horse_persian", 1.0),
    ("zeroad.camel", 1.0),
    ("zeroad.mastiff", 1.0),
];
const PLAYER_RADIUS: f64 = 0.35;
const JUMP_SPEED: f64 = 6.5;
const RUN_JUMP_SPEED: f64 = 8.5;
const GRAVITY: f64 = 9.81;
/// Landing speed up to which a fall does no harm: a run-jump lands at 8.5 m/s, a drop of three metres at 7.7.
const SAFE_IMPACT_SPEED: f64 = 8.0;
/// Landing speed that kills a character outright (a drop of about 22 m).
const LETHAL_IMPACT_SPEED: f64 = 21.0;

/// The health a fall takes from a character that lands at `speed` metres per second: nothing up to the safe
/// speed, then the kinetic energy beyond it (it grows with the square of the speed) until it takes all of 100.
pub(super) fn fall_damage(speed: f64) -> i32 {
    fall_damage_of(speed, 1.0, 1.0)
}

/// The same for a body `size` times as tall as a man's (a drop is as hard as it is long compared with the body, so
/// the speeds that count grow with the square root of the size) that endures `toughness` times as much.
pub(super) fn fall_damage_of(speed: f64, size: f64, toughness: f64) -> i32 {
    let speed = speed / size.max(0.1).sqrt();
    if !speed.is_finite() || speed <= SAFE_IMPACT_SPEED {
        return 0;
    }
    let share = (speed * speed - SAFE_IMPACT_SPEED * SAFE_IMPACT_SPEED)
        / (LETHAL_IMPACT_SPEED * LETHAL_IMPACT_SPEED - SAFE_IMPACT_SPEED * SAFE_IMPACT_SPEED);
    ((share * 100.0) / toughness.max(1.0))
        .round()
        .clamp(0.0, 100.0) as i32
}
const COLLISION_SKIN: f64 = 0.001;
const WALKABLE_NORMAL: f64 = 0.8192319205190405;
/// The highest step a walking character climbs without jumping (the risers of stairs are 0.3 m).
const STEP_HEIGHT: f64 = 0.45;
/// The step near a datadisk building: stairs are 0.3 m high with treads narrower than the capsule, so the
/// capsule must clear two risers at once.
const STAIR_STEP_HEIGHT: f64 = 0.7;

/// How high a walking character climbs in one step among these obstacles.
fn step_lift(objects: &[WorldObject]) -> f64 {
    if objects
        .iter()
        .any(|object| object.model.starts_with(STRUCTURE_PREFIX))
    {
        STAIR_STEP_HEIGHT
    } else {
        STEP_HEIGHT
    }
}
/// Steepest contact of a datadisk building a character stands on (the round capsule meets stairs of 45 degrees
/// on their edges with a normal of 0.5 to 0.7); terrain and the natural props keep `WALKABLE_NORMAL`.
const STAIR_NORMAL: f64 = 0.4;

/// Whether a surface with this normal carries a character standing on it.
fn walkable(normal: [f64; 3], up: [f64; 3], structure: bool) -> bool {
    dot(normal, up)
        >= if structure {
            STAIR_NORMAL
        } else {
            WALKABLE_NORMAL
        }
}
/// Model names of buildings that datadisks bring: `disk:<disk>:<media path>`.
pub(super) const STRUCTURE_PREFIX: &str = "disk:";

fn flight_step(height: f64, velocity: f64, seconds: f64) -> (f64, f64) {
    (
        height + velocity * seconds - 0.5 * GRAVITY * seconds * seconds,
        velocity - GRAVITY * seconds,
    )
}
static TERRAIN: Mutex<Option<(String, Arc<WalkingTerrain>)>> = Mutex::const_new(None);
/// Obstacles that places add to the world (walls, fences, gravestones), for the terrain with
/// the given key `<seed>:<heightmap hash>`.
static STATICS: std::sync::RwLock<(String, Vec<WorldObject>)> =
    std::sync::RwLock::new((String::new(), Vec::new()));

/// A settlement with a pasture beside it, for the key `<seed>:<heightmap hash>`.
#[derive(Clone, Debug)]
pub(super) struct FarmSite {
    pub id: String,
    pub direction: [f64; 3],
    /// Radius of the built-up area in metres; the pasture lies beyond it.
    pub radius_m: f64,
    /// Where a shepherd drives the flock of sheep and goats from the fold (unit direction); none when the
    /// land beyond the fold does not allow it, and the flock then keeps to the fold.
    pub pasture: Option<[f64; 3]>,
    /// Shift of this flock's cycle, so that the flocks of a world are not all out at the same time.
    pub phase_ms: i64,
}

static FARMS: std::sync::RwLock<(String, Vec<FarmSite>)> =
    std::sync::RwLock::new((String::new(), Vec::new()));

/// A landscape patch of a datadisk place (ADR 0013): real-world relief around the site with its
/// river at the historical course, blended over the base terrain within its feathered rectangle.
#[derive(Clone, Debug)]
pub struct TerrainPatch {
    /// Unit vector of the placed anchor.
    pub anchor: [f64; 3],
    /// Ground height of the anchor on the base terrain; the patch's 0x8000 means exactly this.
    pub anchor_height_m: f64,
    /// Size of the rectangle in the local x/z frame, centred `offset_m` from the anchor.
    pub extent_m: [f64; 2],
    pub offset_m: [f64; 2],
    pub feather_m: f64,
    /// Pixels of the 16-bit heightmap: decimetres above the anchor, row-major, `width` per row.
    pub width: usize,
    pub height: usize,
    pub elevation: Vec<i16>,
    /// The water mask (0 = dry land in the file, `i16::MIN` after the offset): other pixels the
    /// surface in decimetres above the anchor, row-major like `elevation`.
    pub water: Option<Vec<i16>>,
    /// Where a datadisk has built or tilled the ground: no natural scenery stands on these pixels
    /// (streets, houses, squares, fields). Its own size, nearest pixel.
    pub clear: Option<ClearMask>,
}

/// A mask of cleared ground over the rectangle of a landscape patch (a pixel above zero is cleared).
#[derive(Clone, Debug)]
pub struct ClearMask {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<bool>,
}

/// Landscape patches of the datadisk places, for the key `<seed>:<heightmap hash>`.
static PATCHES: std::sync::RwLock<(String, Vec<TerrainPatch>)> =
    std::sync::RwLock::new((String::new(), Vec::new()));

/// A place's historical flora/fauna overlay: weighted catalog ids inside a radius.
/// Each entry is `(catalog id, weight, waterside)` — waterside entries only spawn near water.
#[derive(Clone, Debug)]
pub struct WildlifeZone {
    pub direction: [f64; 3],
    pub radius_m: f64,
    pub density: f64,
    pub flora: Vec<(String, f64, bool)>,
    pub fauna: Vec<(String, f64, bool)>,
}

static WILDLIFE: std::sync::RwLock<(String, Vec<WildlifeZone>)> =
    std::sync::RwLock::new((String::new(), Vec::new()));

impl TerrainPatch {
    /// The patch rectangle around `direction`: local metres from its centre and the blend weight
    /// (1.0 inside the feathered core, easing to 0.0 at the rectangle edge), when inside at all.
    fn project(&self, direction: [f64; 3]) -> Option<(f64, f64, f64)> {
        let (axis_x, axis_z) = crate::scenery::client_axes(self.anchor);
        let x = RADIUS * dot(direction, axis_x) - self.offset_m[0];
        let z = RADIUS * dot(direction, axis_z) - self.offset_m[1];
        let half = [self.extent_m[0] / 2.0, self.extent_m[1] / 2.0];
        if x.abs() > half[0] || z.abs() > half[1] {
            return None;
        }
        let ease = |distance: f64, edge: f64| {
            let weight = ((edge - distance) / self.feather_m).clamp(0.0, 1.0);
            weight * weight * (3.0 - 2.0 * weight)
        };
        Some((x, z, ease(x.abs(), half[0]) * ease(z.abs(), half[1])))
    }

    /// Bilinear metres above the anchor at local patch metres.
    fn sample(&self, x: f64, z: f64) -> f64 {
        let grid = &self.elevation;
        let unit = 0.1;
        let columns = self.width as f64;
        let rows = self.height as f64;
        let horizontal = ((x + self.extent_m[0] / 2.0) / self.extent_m[0] * columns - 0.5)
            .clamp(0.0, columns - 1.0);
        let vertical =
            ((z + self.extent_m[1] / 2.0) / self.extent_m[1] * rows - 0.5).clamp(0.0, rows - 1.0);
        let column = horizontal.floor() as usize;
        let row = vertical.floor() as usize;
        let corner = |column: usize, row: usize| {
            f64::from(grid.get(row * self.width + column).copied().unwrap_or(0))
        };
        let west = corner(column, row);
        let east = corner((column + 1).min(self.width - 1), row);
        let north = corner(column, (row + 1).min(self.height - 1));
        let south = corner(
            (column + 1).min(self.width - 1),
            (row + 1).min(self.height - 1),
        );
        let upper = west + (east - west) * (horizontal - column as f64);
        let lower = north + (south - north) * (horizontal - column as f64);
        (upper + (lower - upper) * (vertical - row as f64)) * unit
    }

    /// Whether the clearing mask of the patch covers the local patch metres.
    fn cleared(&self, x: f64, z: f64) -> bool {
        let Some(mask) = self.clear.as_ref() else {
            return false;
        };
        let column = (((x + self.extent_m[0] / 2.0) / self.extent_m[0] * mask.width as f64)
            .clamp(0.0, mask.width as f64 - 1.0)) as usize;
        let row = (((z + self.extent_m[1] / 2.0) / self.extent_m[1] * mask.height as f64)
            .clamp(0.0, mask.height as f64 - 1.0)) as usize;
        mask.pixels
            .get(row * mask.width + column)
            .copied()
            .unwrap_or(false)
    }

    /// The water surface in metres above the anchor at local patch metres, or nothing on dry
    /// land. Nearest pixel, so the mask keeps sharp banks instead of bleeding across them.
    fn surface(&self, x: f64, z: f64) -> Option<f64> {
        let mask = self.water.as_ref()?;
        let column = (((x + self.extent_m[0] / 2.0) / self.extent_m[0] * self.width as f64)
            .clamp(0.0, self.width as f64 - 1.0)) as usize;
        let row = (((z + self.extent_m[1] / 2.0) / self.extent_m[1] * self.height as f64)
            .clamp(0.0, self.height as f64 - 1.0)) as usize;
        let level = *mask.get(row * self.width + column)?;
        (level != i16::MIN).then_some(f64::from(level) * 0.1)
    }
}

/// Ground height in a direction with every patch applied, blended over the base height.
fn patch_height(direction: [f64; 3], base: f64) -> Option<f64> {
    let stored = PATCHES.read().ok()?;
    stored.1.iter().find_map(|patch| {
        let (x, z, weight) = patch.project(direction)?;
        let height = patch.anchor_height_m + patch.sample(x, z);
        Some(base + (height - base) * weight)
    })
}

/// The water surface of a patch in a direction (absolute metres), when its mask says water.
fn patch_water(direction: [f64; 3]) -> Option<f64> {
    let stored = PATCHES.read().ok()?;
    stored.1.iter().find_map(|patch| {
        let (x, z, _) = patch.project(direction)?;
        let surface = patch.surface(x, z)?;
        Some(patch.anchor_height_m + surface)
    })
}

/// Distance in metres (to within a few metres) from a dry point to the nearest water of a patch,
/// searched up to `reach_m`; `None` when the point is wet, outside every patch or far from water.
fn patch_water_distance(direction: [f64; 3], reach_m: f64) -> Option<f64> {
    nearest_water_bank(direction, reach_m).map(|(distance, _)| distance)
}

/// Dry bank beside the nearest patch water within `reach_m`: metres to the water and the unit
/// direction of a standable spot a couple of metres inland. `None` when wet, outside every patch
/// or farther than `reach_m` from water.
fn nearest_water_bank(direction: [f64; 3], reach_m: f64) -> Option<(f64, [f64; 3])> {
    let stored = PATCHES.read().ok()?;
    let mut best: Option<(f64, [f64; 3])> = None;
    for patch in &stored.1 {
        patch.water.as_ref()?;
        let (x, z, _) = patch.project(direction)?;
        if patch.surface(x, z).is_some() {
            continue;
        }
        let mut radius = 4.0;
        while radius <= reach_m {
            for step in 0..16 {
                let angle = f64::from(step) * std::f64::consts::TAU / 16.0;
                let wx = x + radius * angle.cos();
                let wz = z + radius * angle.sin();
                if patch.surface(wx, wz).is_none() {
                    continue;
                }
                // Stand on the dry side of the bank, a couple of metres inland from the water pixel.
                let bank_r = (radius - 2.5).max(1.0);
                let bx = x + bank_r * angle.cos();
                let bz = z + bank_r * angle.sin();
                if patch.surface(bx, bz).is_some() {
                    continue;
                }
                let (bank, _) = crate::scenery::world_from_local(
                    patch.anchor,
                    bx + patch.offset_m[0],
                    bz + patch.offset_m[1],
                    |_| 0.0,
                    RADIUS,
                );
                if best.map_or(true, |(distance, _)| radius < distance) {
                    best = Some((radius, bank));
                }
                break;
            }
            radius += 4.0;
        }
    }
    best
}

/// Whether a dry cell is close enough to patch water, the sea or standing water for a
/// `waterside: true` overlay entry.
fn near_waterside(terrain: &WalkingTerrain, direction: [f64; 3]) -> bool {
    if patch_water_distance(direction, WATERSIDE_REACH_M).is_some() {
        return true;
    }
    for step in 0..8 {
        let angle = f64::from(step) * std::f64::consts::TAU / 8.0;
        for &meters in &[WATERSIDE_REACH_M * 0.5, WATERSIDE_REACH_M] {
            let (probe, _) = crate::scenery::world_from_local(
                direction,
                angle.cos() * meters,
                angle.sin() * meters,
                |_| 0.0,
                RADIUS,
            );
            if terrain.is_sea(probe)
                || patch_water(probe).is_some()
                || terrain.water_depth(probe) > 0.5
            {
                return true;
            }
        }
    }
    false
}

/// Whether a landscape patch covers this direction with dry ground (not its river mask).
/// Early Dynastic Uruk sits on a coarse Earth cell that is sea or river; the patch is the real tell.
fn patch_dry(direction: [f64; 3]) -> bool {
    let Ok(stored) = PATCHES.read() else {
        return false;
    };
    stored.1.iter().any(|patch| {
        patch
            .project(direction)
            .is_some_and(|(x, z, weight)| weight > 0.0 && patch.surface(x, z).is_none())
    })
}

/// Whether the clearing mask of a patch (streets, houses, fields of a datadisk) covers a direction.
fn patch_cleared(direction: [f64; 3]) -> bool {
    let Ok(stored) = PATCHES.read() else {
        return false;
    };
    stored.1.iter().any(|patch| {
        patch
            .project(direction)
            .is_some_and(|(x, z, _)| patch.cleared(x, z))
    })
}

pub(super) fn set_patches(key: String, patches: Vec<TerrainPatch>) {
    if let Ok(mut stored) = PATCHES.write() {
        *stored = (key, patches);
    }
}

pub(super) fn set_wildlife(key: String, zones: Vec<WildlifeZone>) {
    if let Ok(mut stored) = WILDLIFE.write() {
        *stored = (key, zones);
    }
}

/// The wildlife overlay covering a direction, if any (nearest / first match).
fn wildlife_at(direction: [f64; 3]) -> Option<WildlifeZone> {
    let stored = WILDLIFE.read().ok()?;
    stored.1.iter().find_map(|zone| {
        let distance = length(std::array::from_fn(|axis| {
            (zone.direction[axis] - direction[axis]) * RADIUS
        }));
        (distance <= zone.radius_m).then(|| zone.clone())
    })
}

pub(super) fn set_farms(key: String, sites: Vec<FarmSite>) {
    if let Ok(mut farms) = FARMS.write() {
        *farms = (key, sites);
    }
}

/// A bridge over deep water: its deck is ground to walk on. Directions are unit vectors of the two banks.
#[derive(Clone, Debug, PartialEq)]
pub struct BridgeDeck {
    pub from: [f64; 3],
    pub to: [f64; 3],
    pub half_width_m: f64,
    /// Deck height at the banks, in metres.
    pub from_height_m: f64,
    pub to_height_m: f64,
    /// How far the middle of the deck hangs below the line between the banks (a rope bridge sags).
    pub sag_m: f64,
}

static BRIDGES: std::sync::RwLock<Vec<BridgeDeck>> = std::sync::RwLock::new(Vec::new());

pub(super) fn set_bridges(decks: Vec<BridgeDeck>) {
    if let Ok(mut stored) = BRIDGES.write() {
        *stored = decks;
    }
}

/// The height of the deck of a bridge under a direction, when it stands on one.
fn bridge_height(direction: [f64; 3]) -> Option<f64> {
    let stored = BRIDGES.read().ok()?;
    stored.iter().find_map(|deck| {
        let point = direction.map(|axis| axis * RADIUS);
        let (from, to) = (
            deck.from.map(|axis| axis * RADIUS),
            deck.to.map(|axis| axis * RADIUS),
        );
        let along: [f64; 3] = std::array::from_fn(|axis| to[axis] - from[axis]);
        let length_squared = dot(along, along);
        if length_squared < 1e-6 {
            return None;
        }
        let offset: [f64; 3] = std::array::from_fn(|axis| point[axis] - from[axis]);
        let t = dot(offset, along) / length_squared;
        if !(0.0..=1.0).contains(&t) {
            return None;
        }
        let aside: [f64; 3] = std::array::from_fn(|axis| offset[axis] - along[axis] * t);
        (length(aside) <= deck.half_width_m).then_some(
            deck.from_height_m + (deck.to_height_m - deck.from_height_m) * t
                - deck.sag_m * 4.0 * t * (1.0 - t),
        )
    })
}

pub(super) fn set_statics(key: String, obstacles: Vec<WorldObject>) {
    if let Ok(mut statics) = STATICS.write() {
        *statics = (key, obstacles);
    }
}
static COLLIDERS: OnceLock<HashMap<String, TriMesh>> = OnceLock::new();

#[derive(Deserialize)]
struct CollisionMesh {
    vertices: Vec<[f64; 3]>,
    triangles: Vec<[u32; 3]>,
}

fn colliders() -> &'static HashMap<String, TriMesh> {
    COLLIDERS.get_or_init(|| {
        let source: HashMap<String, CollisionMesh> =
            serde_json::from_str(include_str!("../etc/world_colliders.json"))
                .expect("bundled collision geometry must be valid");
        source
            .into_iter()
            .map(|(id, mesh)| {
                (
                    id,
                    TriMesh::new(
                        mesh.vertices.into_iter().map(Point3::from).collect(),
                        mesh.triangles,
                    ),
                )
            })
            .collect()
    })
}

type StructureMeshes = std::sync::RwLock<HashMap<String, (String, &'static TriMesh)>>;
static STRUCTURE_MESHES: OnceLock<StructureMeshes> = OnceLock::new();

/// Registers the collision mesh of a datadisk building under its model name. A mesh lives as long
/// as the process; it is replaced (and the old one leaked) only when the file's hash changes.
pub(super) fn register_structure(
    model: &str,
    sha256: &str,
    vertices: Vec<[f64; 3]>,
    triangles: Vec<[u32; 3]>,
) {
    let meshes = STRUCTURE_MESHES.get_or_init(Default::default);
    if let Ok(mut meshes) = meshes.write() {
        if meshes.get(model).is_some_and(|(hash, _)| hash == sha256) {
            return;
        }
        let mesh = TriMesh::new(vertices.into_iter().map(Point3::from).collect(), triangles);
        meshes.insert(
            model.to_owned(),
            (sha256.to_owned(), Box::leak(Box::new(mesh))),
        );
    }
}

fn structure_mesh(model: &str) -> Option<&'static TriMesh> {
    let meshes = STRUCTURE_MESHES.get()?.read().ok()?;
    meshes.get(model).map(|(_, mesh)| *mesh)
}

fn object_shape(object: &WorldObject) -> Option<(Isometry3<f64>, &'static TriMesh)> {
    let up = Vector3::from(unit(object.position));
    let rotation = UnitQuaternion::rotation_between(&Vector3::y(), &up)?
        * UnitQuaternion::from_axis_angle(&Vector3::y_axis(), object.yaw);
    Some((
        Isometry3::from_parts(Translation3::from(Vector3::from(object.position)), rotation),
        colliders()
            .get(&object.model)
            .or_else(|| structure_mesh(&object.model))?,
    ))
}

#[cfg(test)]
fn object_floor(point: [f64; 3], object: &WorldObject) -> Option<f64> {
    if object.collision_radius_m <= 0.0 {
        return None;
    }
    let (transform, mesh) = object_shape(object)?;
    let up = unit(point);
    let ray = Ray::new(
        transform.inverse_transform_point(&Point3::from(point)) / object.scale_m,
        transform.inverse_transform_vector(&-Vector3::from(up)),
    );
    let distance = mesh.cast_local_ray(&ray, 100.0 / object.scale_m, false)?;
    Some(length(point) - distance * object.scale_m)
}

fn object_capsule(
    point: [f64; 3],
    object_transform: &Isometry3<f64>,
    scale: f64,
) -> (Isometry3<f64>, Capsule) {
    let up = Vector3::from(unit(point));
    let rotation = UnitQuaternion::rotation_between(&Vector3::y(), &up).unwrap_or_default();
    let transform = Isometry3::from_parts(Translation3::from(Vector3::from(point)), rotation);
    let mut local_transform = object_transform.inverse() * transform;
    local_transform.translation.vector /= scale;
    (
        local_transform,
        Capsule::new(
            Point3::new(0.0, PLAYER_RADIUS / scale, 0.0),
            Point3::new(0.0, 1.45 / scale, 0.0),
            PLAYER_RADIUS / scale,
        ),
    )
}

fn capsule_hit(
    point: [f64; 3],
    motion: [f64; 3],
    objects: &[WorldObject],
) -> Option<(f64, [f64; 3])> {
    capsule_hit_kind(point, motion, objects).map(|(time, normal, _)| (time, normal))
}

/// Like `capsule_hit`, and tells whether the nearest thing hit is a building of a datadisk.
fn capsule_hit_kind(
    point: [f64; 3],
    motion: [f64; 3],
    objects: &[WorldObject],
) -> Option<(f64, [f64; 3], bool)> {
    let mut closest: Option<(f64, [f64; 3], bool)> = None;
    for object in objects
        .iter()
        .filter(|object| object.collision_radius_m > 0.0)
    {
        let Some((object_transform, mesh)) = object_shape(object) else {
            // A round obstacle without a mesh (fences, graves, lamps of places) blocks sideways
            // only; it must not stop a jump that goes straight up.
            if let Some(hit) = cylinder_hit(point, motion, object) {
                if closest.map_or(true, |(time, _, _)| hit.0 < time) {
                    closest = Some((hit.0, hit.1, false));
                }
            }
            continue;
        };
        let (local_transform, body) = object_capsule(point, &object_transform, object.scale_m);
        let local_velocity =
            object_transform.inverse_transform_vector(&Vector3::from(motion)) / object.scale_m;
        match cast_shapes3(
            &local_transform,
            &local_velocity,
            &body,
            &Isometry3::identity(),
            &Vector3::zeros(),
            mesh,
            ShapeCastOptions3 {
                max_time_of_impact: 1.0,
                target_distance: COLLISION_SKIN / object.scale_m,
                stop_at_penetration: false,
                ..ShapeCastOptions3::default()
            },
        ) {
            Ok(Some(hit)) => {
                let normal: [f64; 3] =
                    (object_transform.rotation * hit.normal2.into_inner()).into();
                if dot(motion, normal) < -1e-10
                    && closest.map_or(true, |(time, _, _)| hit.time_of_impact < time)
                {
                    closest = Some((
                        hit.time_of_impact.clamp(0.0, 1.0),
                        normal,
                        object.model.starts_with(STRUCTURE_PREFIX),
                    ));
                }
            }
            Ok(None) => {}
            Err(_) => return Some((0.0, unit(motion).map(|value| -value), false)),
        }
    }
    closest
}

/// Where a moving player first touches a mesh-less round obstacle (a vertical cylinder of the
/// object's collision radius), and the horizontal normal there. Moving away is never blocked.
fn cylinder_hit(
    point: [f64; 3],
    motion: [f64; 3],
    object: &WorldObject,
) -> Option<(f64, [f64; 3])> {
    let up = unit(point);
    let flat = |vector: [f64; 3]| -> [f64; 3] {
        let lift = dot(vector, up);
        std::array::from_fn(|axis| vector[axis] - up[axis] * lift)
    };
    let offset = flat(std::array::from_fn(|axis| {
        point[axis] - object.position[axis]
    }));
    let sideways = flat(motion);
    let reach = object.collision_radius_m + PLAYER_RADIUS;
    let a = dot(sideways, sideways);
    let b = dot(offset, sideways);
    if a < 1e-12 || b >= 0.0 {
        return None;
    }
    let c = dot(offset, offset) - reach * reach;
    // Already inside: only motion deeper in counts, and it is stopped at once.
    let time = if c <= 0.0 {
        0.0
    } else {
        let discriminant = b * b - a * c;
        if discriminant < 0.0 {
            return None;
        }
        ((-b - discriminant.sqrt()) / a).max(0.0)
    };
    if time > 1.0 {
        return None;
    }
    let touch: [f64; 3] = std::array::from_fn(|axis| offset[axis] + sideways[axis] * time);
    (length(touch) > 1e-9).then(|| (time, unit(touch)))
}

/// A walking capsule that a low edge (a stair riser) blocks: lifts it by one step, moves it on and
/// sets it down on the walkable surface it reaches. `None` when the edge is too high, the way is
/// blocked above it, or nothing walkable lies beyond.
fn step_up(
    start: [f64; 3],
    motion: [f64; 3],
    slid: [f64; 3],
    lift: f64,
    objects: &[WorldObject],
) -> Option<[f64; 3]> {
    let up = unit(start);
    let along = |from: [f64; 3]| {
        let offset: [f64; 3] = std::array::from_fn(|axis| from[axis] - start[axis]);
        let lift = dot(offset, up);
        length(std::array::from_fn(|axis| offset[axis] - up[axis] * lift))
    };
    let wanted = along(std::array::from_fn(|axis| start[axis] + motion[axis]));
    if wanted < 1e-9 || capsule_hit(start, up.map(|value| value * lift), objects).is_some() {
        return None;
    }
    let lifted: [f64; 3] = std::array::from_fn(|axis| start[axis] + up[axis] * lift);
    let (over, _) = slide_capsule(lifted, motion, objects);
    if along(over) < 0.6 * wanted || along(over) <= along(slid) + 1e-4 {
        return None;
    }
    let drop = lift + 0.05;
    let (time, normal, structure) = capsule_hit_kind(over, up.map(|value| -value * drop), objects)?;
    if !walkable(normal, up, structure) {
        return None;
    }
    let landed: [f64; 3] = std::array::from_fn(|axis| over[axis] - up[axis] * drop * time);
    (dot(std::array::from_fn(|axis| landed[axis] - start[axis]), up) > 0.01).then_some(landed)
}

fn recover_capsule(mut point: [f64; 3], objects: &[WorldObject]) -> [f64; 3] {
    for _ in 0..8 {
        let mut recovered = false;
        for object in objects
            .iter()
            .filter(|object| object.collision_radius_m > 0.0)
        {
            let Some((object_transform, mesh)) = object_shape(object) else {
                continue;
            };
            let (transform, body) = object_capsule(point, &object_transform, object.scale_m);
            if let Ok(Some(hit)) = contact(&transform, &body, &Isometry3::identity(), mesh, 0.0) {
                if hit.dist < -1e-7 / object.scale_m {
                    let normal: [f64; 3] =
                        (object_transform.rotation * hit.normal2.into_inner()).into();
                    let distance = (COLLISION_SKIN - hit.dist * object.scale_m).min(0.5);
                    point = std::array::from_fn(|axis| point[axis] + normal[axis] * distance);
                    recovered = true;
                }
            }
        }
        if !recovered {
            break;
        }
    }
    point
}

fn slide_capsule(
    mut point: [f64; 3],
    mut motion: [f64; 3],
    objects: &[WorldObject],
) -> ([f64; 3], Vec<[f64; 3]>) {
    let mut normals = Vec::new();
    for _ in 0..4 {
        if length(motion) < 1e-8 {
            break;
        }
        let Some((time, normal)) = capsule_hit(point, motion, objects) else {
            point = std::array::from_fn(|axis| point[axis] + motion[axis]);
            break;
        };
        point = std::array::from_fn(|axis| point[axis] + motion[axis] * time + normal[axis] * 1e-6);
        motion = motion.map(|value| value * (1.0 - time));
        let incoming = dot(motion, normal).min(0.0);
        motion = std::array::from_fn(|axis| motion[axis] - normal[axis] * incoming);
        normals.push(normal);
    }
    (point, normals)
}

#[cfg(test)]
fn clear_jump_path(point: [f64; 3], destination: [f64; 3], objects: &[WorldObject]) -> bool {
    capsule_hit(
        point,
        std::array::from_fn(|axis| destination[axis] - point[axis]),
        objects,
    )
    .is_none()
}

#[derive(Default)]
struct Flight {
    airborne: bool,
    vertical_speed: f64,
    horizontal: [f64; 3],
    /// The downward speed (m/s) of the hardest landing since it was last read; the caller turns it into damage.
    impact: f64,
}

#[derive(Serialize, FromRow)]
pub(super) struct Position {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub sequence: String,
    pub airborne: bool,
    pub on_object: bool,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Intent {
    direction: [f64; 3],
    sequence: String,
    #[serde(default)]
    jump: bool,
    #[serde(default)]
    run: bool,
    /// How tall the character is next to a man (set by the server from the datadisk's player body, never by a client):
    /// water reaches the chest of a giant at a greater depth. 0 stands for 1.
    #[serde(skip)]
    body_scale: f64,
}

impl Intent {
    fn scale(&self) -> f64 {
        if self.body_scale > 0.0 {
            self.body_scale
        } else {
            1.0
        }
    }
}

#[derive(Serialize)]
pub(super) struct Reply {
    position: Position,
    moving: bool,
    stats: players::Stats,
    /// True while the character sits on a world rock.
    sitting: bool,
    /// True while the character is in water, swimming and losing health until a dry shore.
    swimming: bool,
}

pub(super) struct WalkingTerrain {
    size: usize,
    pixels: Vec<u16>,
    environment: environment::Environment,
    catalog: Vec<ObjectKind>,
    removed: Removed,
}

/// Objects that players have harvested, with the unix second at which they grow back.
/// They are absent from rendering and collision until then.
#[derive(Default)]
struct Removed(std::sync::RwLock<HashMap<String, Remains>>);

/// When an object grows back and what, if anything, stands in its place meanwhile.
type Remains = (i64, Option<(String, f64)>);

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// How many sheep and goats a shepherd drives from a settlement of this size.
pub(super) fn flock_size(radius_m: f64) -> usize {
    if radius_m < 40.0 {
        5
    } else if radius_m < 60.0 {
        8
    } else {
        12
    }
}

/// How far beyond the built-up area of a settlement its fold (the pasture beside it) lies.
pub(super) const FOLD_M: f64 = 30.0;

/// The direction in which a settlement's fold and pasture lie, radians in the local frame of the settlement.
pub(super) fn fold_heading(seed: &str, site_id: &str) -> f64 {
    let place = Sha256::digest(format!("{seed}:pasture:{site_id}"));
    f64::from(u32::from_le_bytes(place[0..4].try_into().unwrap())) / 4_294_967_296.0
        * std::f64::consts::TAU
}

/// How many animals graze beside a settlement of this size.
fn farm_herd(radius_m: f64) -> usize {
    if radius_m < 40.0 {
        3
    } else if radius_m < 60.0 {
        6
    } else {
        10
    }
}

pub(super) fn unix_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

/// Where a walking animal stands at `time_ms`, from the waypoints sent to clients.
pub(super) fn route_position(route: &[[f64; 4]], time_ms: i64) -> [f64; 3] {
    let time = time_ms as f64;
    let point = |entry: &[f64; 4]| [entry[1], entry[2], entry[3]];
    let (Some(first), Some(last)) = (route.first(), route.last()) else {
        return [0.0; 3];
    };
    if time <= first[0] {
        return point(first);
    }
    for pair in route.windows(2) {
        if time <= pair[1][0] {
            let span = pair[1][0] - pair[0][0];
            let fraction = if span > 0.0 {
                (time - pair[0][0]) / span
            } else {
                1.0
            };
            return std::array::from_fn(|axis| {
                pair[0][axis + 1] + (pair[1][axis + 1] - pair[0][axis + 1]) * fraction
            });
        }
    }
    point(last)
}

#[derive(Deserialize)]
struct ObjectCatalog {
    version: u32,
    objects: Vec<ObjectKind>,
}

#[derive(Deserialize)]
struct ObjectKind {
    id: String,
    biomes: Vec<u8>,
    weight: f64,
    scale_m: f64,
    max_slope: f64,
    collision_radius: f64,
    /// Animals form their own layer with its own grid and ids, so adding them
    /// does not change where trees and rocks stand.
    #[serde(default)]
    fauna: bool,
    /// Swims below the surface of oceans, lakes and rivers instead of standing on land.
    #[serde(default)]
    aquatic: bool,
    /// Deepest the animal swims below the surface, in metres.
    #[serde(default = "default_depth")]
    depth_m: f64,
    /// Flies above the ground (Idle = glide, Walk = flight on the client).
    #[serde(default)]
    flying: bool,
    /// Typical height above the terrain when `flying`, in metres.
    #[serde(default = "default_altitude")]
    altitude_m: f64,
    /// Vertical placement offset in metres (negative sinks the model into the
    /// ground). 0 A.D. trees often have a few root-tip vertices at Y=0 while the
    /// solid root flare sits one to two metres higher, so a shallow offset still
    /// leaves them floating (figs, willows, olives).
    #[serde(default)]
    ground_offset_m: f64,
    /// Reeds, papyrus and cattails: stand in shallow water or on its bank (within
    /// [`WATERSIDE_REACH_M`]), never on dry steppe far from water.
    #[serde(default)]
    shallow_water: bool,
}

fn default_depth() -> f64 {
    8.0
}

fn default_altitude() -> f64 {
    15.0
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(super) struct WorldObject {
    pub id: String,
    pub model: String,
    pub position: [f64; 3],
    pub scale_m: f64,
    pub yaw: f64,
    pub collision_radius_m: f64,
    pub biome: u8,
    #[serde(default)]
    pub ground_offset_m: f64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RegionQuery {
    x: f64,
    y: f64,
    z: f64,
}

/// A world object as sent to clients, with what it can be harvested for.
#[derive(Serialize)]
struct ObjectDescriptor {
    #[serde(flatten)]
    object: WorldObject,
    /// `tree` or `rock` when the object can be harvested, with the tool it needs.
    #[serde(skip_serializing_if = "Option::is_none")]
    harvest: Option<super::gathering::HarvestInfo>,
    /// Waypoints `[unix milliseconds, x, y, z]` of a walking animal; its `position` is where it
    /// stands at `server_time_ms`, and it moves in straight lines between the waypoints.
    #[serde(skip_serializing_if = "Option::is_none")]
    wander: Option<Vec<[f64; 4]>>,
}

#[derive(Serialize)]
pub(super) struct ObjectRegion {
    version: u32,
    heightmap_sha256: String,
    seed: String,
    /// The server's clock when the region was computed, in unix milliseconds.
    server_time_ms: i64,
    objects: Vec<ObjectDescriptor>,
    /// Characters of the story datadisks standing nearby (passable for now).
    npcs: Vec<super::story::world::NpcDescriptor>,
    /// Places with background music near the point.
    areas: Vec<super::story::world::AreaDescriptor>,
    /// Buildings and props of towns, graveyards and harbours nearby.
    props: Vec<super::story::settlements::WorldProp>,
    /// Landscape patches of datadisk places nearby.
    terrains: Vec<super::story::world::TerrainDescriptor>,
    memorials: Vec<super::survival::Memorial>,
    /// Items dropped on the ground nearby.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    loot: Vec<super::loot::GroundItem>,
}

fn object_catalog() -> Option<Vec<ObjectKind>> {
    let catalog: ObjectCatalog =
        serde_json::from_str(include_str!("../etc/world_objects.json")).ok()?;
    (catalog.version == 1).then_some(catalog.objects)
}

fn fauna_catalog() -> Option<Vec<ObjectKind>> {
    let catalog: ObjectCatalog =
        serde_json::from_str(include_str!("../etc/world_fauna.json")).ok()?;
    (catalog.version == 1).then(|| {
        catalog
            .objects
            .into_iter()
            .map(|kind| ObjectKind {
                fauna: true,
                ..kind
            })
            .collect()
    })
}

/// Per-instance size from a catalog base and a unit sample in `[0, 1)`.
/// Neighbours of the same model span roughly half to one and a half times the base height,
/// so trees, rocks and plants are not identical.
fn varied_scale(base_m: f64, unit: f64) -> f64 {
    base_m * (0.55 + unit * 0.9)
}

pub(super) fn unit(point: [f64; 3]) -> [f64; 3] {
    let radius = length(point);
    point.map(|value| value / radius)
}

pub(super) fn cross(first: [f64; 3], second: [f64; 3]) -> [f64; 3] {
    [
        first[1] * second[2] - first[2] * second[1],
        first[2] * second[0] - first[0] * second[2],
        first[0] * second[1] - first[1] * second[0],
    ]
}

pub(super) fn dot(first: [f64; 3], second: [f64; 3]) -> f64 {
    first
        .into_iter()
        .zip(second)
        .map(|(first, second)| first * second)
        .sum()
}

fn cube_point(face: usize, horizontal: f64, vertical: f64) -> [f64; 3] {
    let cube: [f64; 3] = match face {
        0 => [1.0, vertical, -horizontal],
        1 => [-1.0, vertical, horizontal],
        2 => [horizontal, 1.0, -vertical],
        3 => [horizontal, -1.0, vertical],
        4 => [horizontal, vertical, 1.0],
        _ => [-horizontal, vertical, -1.0],
    };
    unit(std::array::from_fn(|axis| {
        let first = cube[(axis + 1) % 3].powi(2);
        let second = cube[(axis + 2) % 3].powi(2);
        cube[axis] * (1.0 - first / 2.0 - second / 2.0 + first * second / 3.0).sqrt()
    }))
}

fn clear_path(point: [f64; 3], destination: [f64; 3], objects: &[WorldObject]) -> bool {
    let up = unit(point);
    let reference = if up[1].abs() < 0.9 {
        [0.0, 1.0, 0.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    let sideways = unit(cross(up, reference));
    let forward = cross(up, sideways);
    let delta = std::array::from_fn(|axis| destination[axis] - point[axis]);
    let velocity = Vector2::new(dot(delta, sideways), dot(delta, forward));
    objects
        .iter()
        .filter(|object| {
            object.collision_radius_m > 0.0 && !object.model.starts_with(STRUCTURE_PREFIX)
        })
        .all(|object| {
            let offset = std::array::from_fn(|axis| object.position[axis] - point[axis]);
            let center = Vector2::new(dot(offset, sideways), dot(offset, forward));
            let radius = PLAYER_RADIUS + object.collision_radius_m;
            if center.norm_squared() < radius * radius && (-center).dot(&velocity) > 0.0 {
                return true;
            }
            cast_shapes(
                &Isometry2::identity(),
                &velocity,
                &Ball::new(PLAYER_RADIUS),
                &Isometry2::translation(center.x, center.y),
                &Vector2::zeros(),
                &Ball::new(object.collision_radius_m),
                ShapeCastOptions {
                    max_time_of_impact: 1.0,
                    ..ShapeCastOptions::default()
                },
            )
            .is_ok_and(|hit| hit.is_none())
        })
}

pub(super) fn length(point: [f64; 3]) -> f64 {
    point.iter().map(|value| value * value).sum::<f64>().sqrt()
}

/// The map tile (face, column, row) of the object grid that contains a position.
pub(super) fn tile_of(point: [f64; 3]) -> (i32, i32, i32) {
    let (face, horizontal, vertical) = coordinates(unit(point));
    (
        face as i32,
        ((horizontal * f64::from(OBJECT_GRID)) as i32).clamp(0, OBJECT_GRID - 1),
        ((vertical * f64::from(OBJECT_GRID)) as i32).clamp(0, OBJECT_GRID - 1),
    )
}

fn coordinates(point: [f64; 3]) -> (usize, f64, f64) {
    let [horizontal, vertical, depth] = point;
    let magnitude = point.map(f64::abs);
    let (face, pair) = if magnitude[0] >= magnitude[1] && magnitude[0] >= magnitude[2] {
        if horizontal >= 0.0 {
            (0, [-depth, vertical])
        } else {
            (1, [depth, vertical])
        }
    } else if magnitude[1] >= magnitude[2] {
        if vertical >= 0.0 {
            (2, [horizontal, -depth])
        } else {
            (3, [horizontal, depth])
        }
    } else if depth >= 0.0 {
        (4, [horizontal, vertical])
    } else {
        (5, [-horizontal, vertical])
    };
    let squared = pair.map(|value| value * value);
    let result: [f64; 2] = std::array::from_fn(|axis| {
        let coefficient = 3.0 + 2.0 * (squared[axis] - squared[1 - axis]);
        pair[axis].signum()
            * (0.5
                * (coefficient
                    - (coefficient * coefficient - 24.0 * squared[axis])
                        .max(0.0)
                        .sqrt()))
            .max(0.0)
            .sqrt()
    });
    (
        face,
        ((result[0] + 1.0) * 0.5).clamp(0.0, 1.0),
        ((1.0 - result[1]) * 0.5).clamp(0.0, 1.0),
    )
}

/// The unit direction of a point on a cube face; both fractions run from 0 to 1.
pub(super) fn direction_at(face: usize, horizontal: f64, vertical: f64) -> [f64; 3] {
    cube_point(face, horizontal * 2.0 - 1.0, 1.0 - vertical * 2.0)
}

impl WalkingTerrain {
    /// For story placement: the biome and height of dry land in this direction, or
    /// nothing over water.
    pub(super) fn land_site(&self, direction: [f64; 3]) -> Option<(u8, f64)> {
        let height = self.surface(direction)?;
        let (face, horizontal, vertical) = coordinates(direction);
        let size = self.environment.face_size;
        let index = face * size * size
            + ((vertical * size as f64) as usize).min(size - 1) * size
            + ((horizontal * size as f64) as usize).min(size - 1);
        let mut biome = self.environment.biomes[index];
        // Dry ground inside a coarse sea/river cell (landscape patch or floodplain above the
        // water surface) is grassland for placement and wildlife.
        if biome < 3 {
            biome = 4;
        }
        Some((biome, height))
    }

    /// The water within `reach_m` metres of a point on the surface (position in metres from the
    /// planet centre): fresh water (lake or river) wins over the sea.
    pub(super) fn water_near(&self, position: [f64; 3], reach_m: f64) -> WaterNear {
        let distance = length(position);
        if distance < 1.0 {
            return WaterNear::None;
        }
        let up = position.map(|value| value / distance);
        let helper = if up[0].abs() < 0.9 {
            [1.0, 0.0, 0.0]
        } else {
            [0.0, 1.0, 0.0]
        };
        let cross = |a: [f64; 3], b: [f64; 3]| {
            [
                a[1] * b[2] - a[2] * b[1],
                a[2] * b[0] - a[0] * b[2],
                a[0] * b[1] - a[1] * b[0],
            ]
        };
        let east = cross(up, helper);
        let east = east.map(|value| value / length(east));
        let north = cross(up, east);
        let mut found = WaterNear::None;
        let mut samples = vec![(0.0, 0.0)];
        for step in 0..8 {
            let angle = f64::from(step) * std::f64::consts::FRAC_PI_4;
            samples.push((angle.cos() * reach_m, angle.sin() * reach_m));
            samples.push((angle.cos() * reach_m * 0.5, angle.sin() * reach_m * 0.5));
        }
        for (dx, dz) in samples {
            let point: [f64; 3] =
                std::array::from_fn(|axis| position[axis] + east[axis] * dx + north[axis] * dz);
            let direction = point.map(|value| value / length(point));
            if patch_water(direction).is_some() {
                return WaterNear::Fresh;
            }
            // Standing on a dry tell/patch: the coarse sea or river under it is not drinkable here.
            if patch_dry(direction) {
                continue;
            }
            let (face, horizontal, vertical) = coordinates(direction);
            let size = self.environment.face_size;
            let index = face * size * size
                + ((vertical * size as f64) as usize).min(size - 1) * size
                + ((horizontal * size as f64) as usize).min(size - 1);
            match self.environment.biomes[index] {
                1 | 2 => return WaterNear::Fresh,
                0 => found = WaterNear::Salt,
                _ => {}
            }
        }
        found
    }

    /// True where the coarse environment map says open sea.
    pub(super) fn is_sea(&self, direction: [f64; 3]) -> bool {
        let (face, horizontal, vertical) = coordinates(direction);
        let size = self.environment.face_size;
        let index = face * size * size
            + ((vertical * size as f64) as usize).min(size - 1) * size
            + ((horizontal * size as f64) as usize).min(size - 1);
        self.environment.biomes[index] == 0
    }

    /// The surface of fresh water that is shallow enough to cross on foot, and not just wade: under three
    /// quarters of a man's chest. `None` on dry land, in the sea and where it is deeper.
    pub(super) fn wade_level(&self, direction: [f64; 3]) -> Option<f64> {
        if self.is_sea(direction) {
            return None;
        }
        let depth = self.water_depth(direction);
        (depth > 0.0 && depth <= WADE_DEPTH_M * 0.75).then(|| self.height(direction) + depth)
    }

    pub(super) fn heightmap_sha256(&self) -> &str {
        &self.environment.heightmap_sha256
    }

    pub(super) fn seed(&self) -> &str {
        &self.environment.seed
    }

    /// Height of the ground (or sea level surface) in this direction, including local relief.
    pub(super) fn ground_height(&self, direction: [f64; 3]) -> f64 {
        self.height(direction)
    }

    /// Finds a generated object by its id (`seed:face:column:row`) unless it is currently harvested.
    pub(super) fn object_by_id(&self, id: &str) -> Option<WorldObject> {
        if self.is_removed(id) {
            return None;
        }
        self.object_cell_by_id(id)
    }

    /// The generated object with this id, ignoring harvesting.
    pub(super) fn object_cell_by_id(&self, id: &str) -> Option<WorldObject> {
        let mut parts = id.rsplitn(4, ':');
        let row: i32 = parts.next()?.parse().ok()?;
        let column: i32 = parts.next()?.parse().ok()?;
        let face: usize = parts.next()?.parse().ok()?;
        let seed = parts.next()?;
        if seed != self.environment.seed
            || face > 5
            || !(0..OBJECT_GRID).contains(&column)
            || !(0..OBJECT_GRID).contains(&row)
        {
            return None;
        }
        self.object_cell(face, column, row)
            .filter(|object| object.id == id)
    }

    /// Hides an object until `until_unix`; a `leaves` model (and its size relative to the
    /// original) is shown in its place, for example a stump where a tree stood.
    /// Where a portal may be built: dry land. Returns the map position (face, column, row).
    pub(super) fn build_site(&self, point: [f64; 3]) -> Option<(i32, i32, i32)> {
        let direction = unit(point);
        self.surface(direction)?;
        let (face, horizontal, vertical) = coordinates(direction);
        let size = self.environment.face_size;
        let column = ((horizontal * size as f64) as usize).min(size - 1);
        let row = ((vertical * size as f64) as usize).min(size - 1);
        Some((face as i32, column as i32, row as i32))
    }

    pub(super) fn mark_removed(&self, id: &str, until_unix: i64, leaves: Option<(String, f64)>) {
        if let Ok(mut removed) = self.removed.0.write() {
            let now = unix_now();
            removed.retain(|_, (until, _)| *until > now);
            removed.insert(id.to_owned(), (until_unix, leaves));
        }
    }

    fn is_removed(&self, id: &str) -> bool {
        self.removed.0.read().is_ok_and(|removed| {
            removed
                .get(id)
                .is_some_and(|(until, _)| *until > unix_now())
        })
    }

    /// Whether the point lies within reach of a datadisk building, whose mesh must be walked by
    /// the capsule instead of the terrain-only step.
    fn near_structure(&self, point: [f64; 3]) -> bool {
        let key = format!(
            "{}:{}",
            self.environment.seed, self.environment.heightmap_sha256
        );
        STATICS.read().is_ok_and(|statics| {
            statics.0 == key
                && statics.1.iter().any(|obstacle| {
                    obstacle.model.starts_with(STRUCTURE_PREFIX)
                        && length(std::array::from_fn(|axis| {
                            obstacle.position[axis] - point[axis]
                        })) < obstacle.collision_radius_m + 4.0
                })
        })
    }

    fn advance(
        &self,
        mut point: [f64; 3],
        intent: &Intent,
        seconds: f64,
        flight: &mut Flight,
        on_object: bool,
    ) -> ([f64; 3], bool) {
        let speed = if intent.run { RUN_SPEED } else { WALK_SPEED };
        if !flight.airborne && !intent.jump && !on_object && !self.near_structure(point) {
            return (
                self.step_scaled(point, intent.direction, seconds, speed, intent.scale())
                    .unwrap_or(point),
                false,
            );
        }
        if !flight.airborne && !on_object {
            point = unit(point).map(|value| value * (RADIUS + self.height(unit(point))));
        }
        let up = unit(point);
        let tangent = std::array::from_fn(|axis| {
            (intent.direction[axis] - up[axis] * dot(intent.direction, up)) * speed
        });
        if intent.jump && !flight.airborne {
            flight.airborne = true;
            flight.vertical_speed = if intent.run && length(tangent) > 1e-6 {
                RUN_JUMP_SPEED
            } else {
                JUMP_SPEED
            };
            flight.horizontal = tangent;
        } else if !flight.airborne {
            flight.horizontal = tangent;
        }
        let objects = self.walking_objects(point, 2);
        point = self.advance_over_objects(point, seconds, flight, &objects);
        (
            point,
            !flight.airborne && length(point) > RADIUS + self.height(unit(point)) + 0.02,
        )
    }

    fn advance_over_objects(
        &self,
        mut point: [f64; 3],
        seconds: f64,
        flight: &mut Flight,
        objects: &[WorldObject],
    ) -> [f64; 3] {
        let seconds = seconds.clamp(0.0, 0.25);
        let steps = (seconds / 0.016).ceil().max(1.0) as usize;
        let delta = seconds / steps as f64;
        point = recover_capsule(point, objects);
        let lift = step_lift(objects);
        for _ in 0..steps {
            let flying = flight.airborne;
            let radius = length(point);
            let up = unit(point);
            let radial = dot(flight.horizontal, up);
            let lateral: [f64; 3] =
                std::array::from_fn(|axis| flight.horizontal[axis] - up[axis] * radial);
            let direction = unit(std::array::from_fn(|axis| {
                point[axis] + lateral[axis] * delta
            }));
            let lateral = if self.surface(direction).is_none() {
                flight.horizontal = [0.0; 3];
                [0.0; 3]
            } else {
                lateral
            };
            let (height, speed) = if flight.airborne {
                flight_step(radius, flight.vertical_speed, delta)
            } else {
                (radius, 0.0)
            };
            let motion =
                std::array::from_fn(|axis| lateral[axis] * delta + up[axis] * (height - radius));
            let before = point;
            let (slid, mut normals) = slide_capsule(point, motion, objects);
            point = slid;
            if !flight.airborne && !normals.is_empty() {
                if let Some(stepped) = step_up(before, motion, point, lift, objects) {
                    point = stepped;
                    normals.clear();
                }
            }
            let up = unit(point);
            let mut velocity =
                std::array::from_fn(|axis| lateral[axis] + up[axis] * speed.max(-100.0));
            for normal in normals {
                let incoming = dot(velocity, normal).min(0.0);
                velocity = std::array::from_fn(|axis| velocity[axis] - normal[axis] * incoming);
            }
            let ground = RADIUS + self.surface(up).unwrap_or_else(|| self.height(up));
            let mut grounded = length(point) <= ground + COLLISION_SKIN && speed <= 0.0;
            if grounded {
                point = up.map(|value| value * ground);
            }
            if speed <= 0.0 {
                if let Some((time, normal, structure)) =
                    capsule_hit_kind(point, up.map(|value| -value * 0.05), objects)
                {
                    if walkable(normal, up, structure) {
                        point = std::array::from_fn(|axis| point[axis] - up[axis] * 0.05 * time);
                        grounded = true;
                    }
                }
            }
            if !grounded && !flight.airborne {
                // Walking down stairs: follow the surface instead of dropping off every tread.
                if let Some((time, normal, structure)) =
                    capsule_hit_kind(point, up.map(|value| -value * lift), objects)
                {
                    if walkable(normal, up, structure) {
                        point = std::array::from_fn(|axis| point[axis] - up[axis] * lift * time);
                        grounded = true;
                    }
                }
            }
            if grounded && flying {
                flight.impact = flight.impact.max(-speed);
            }
            flight.airborne = !grounded;
            flight.vertical_speed = if grounded {
                0.0
            } else {
                dot(velocity, up).clamp(-100.0, RUN_JUMP_SPEED)
            };
            flight.horizontal =
                std::array::from_fn(|axis| velocity[axis] - up[axis] * dot(velocity, up));
            let horizontal_speed = length(flight.horizontal);
            if horizontal_speed > RUN_SPEED {
                flight.horizontal = flight
                    .horizontal
                    .map(|value| value * RUN_SPEED / horizontal_speed);
            }
        }
        point
    }

    #[cfg(test)]
    /// Whether a character standing at `direction` is out of their depth in water (the sea, a lake, a river, a pond):
    /// swimming, slowly, and drowning until they reach a shore where the water is shallow again.
    pub(super) fn in_water(&self, direction: [f64; 3]) -> bool {
        self.in_water_for(direction, 1.0)
    }

    /// [`Self::in_water`] for a character `scale` times the height of a man: water reaches their chest that much deeper.
    pub(super) fn in_water_for(&self, direction: [f64; 3], scale: f64) -> bool {
        self.water_depth(direction) > WADE_DEPTH_M * scale
    }

    /// Metres of water above the ground in a direction, 0 on dry land. The same rules as [`Self::surface`] decide
    /// what is water; this says how much.
    pub(super) fn water_depth(&self, direction: [f64; 3]) -> f64 {
        let height = self.height(direction);
        if let Some(level) = patch_water(direction) {
            return (level - height).max(0.0);
        }
        // A dry patch is dry even over a coarse sea or river cell, down to sea level.
        if patch_dry(direction) {
            return (-height).max(0.0);
        }
        // The same rule as the client's `_detail_water_km`, so there is no water the player cannot see:
        // ground below sea level is not sea by itself (the strip is interpolated between its samples, and
        // land next to a water sample dips below zero far from any coast), only the water of its cell is.
        let (face, horizontal, vertical) = coordinates(direction);
        let size = self.environment.face_size;
        let index = face * size * size
            + ((vertical * size as f64) as usize).min(size - 1) * size
            + ((horizontal * size as f64) as usize).min(size - 1);
        let water = self.environment.water_m[index];
        if water == environment::DRY {
            return 0.0;
        }
        let depth = f64::from(water) - height;
        if environment::is_open_water(self.environment.biomes[index]) {
            return depth.max(0.0);
        }
        if water > self.environment.elevation_m[index] && depth >= 0.5 {
            return depth;
        }
        0.0
    }

    /// The ground height where a character can stand or wade: dry land and water up to their chest.
    fn walkable(&self, direction: [f64; 3], scale: f64) -> Option<f64> {
        (self.water_depth(direction) <= WADE_DEPTH_M * scale).then(|| self.height(direction))
    }

    fn surface(&self, direction: [f64; 3]) -> Option<f64> {
        // A river of a landscape patch is no ground to stand scenery on.
        if patch_water(direction).is_some() {
            return None;
        }
        // Dry patch land is walkable even when the coarse planet cell is sea or river (Uruk on
        // the Early Dynastic Gulf). Without this, near a city wall the capsule walker zeroes
        // every horizontal step and only jumping still moves the character.
        if patch_dry(direction) {
            let height = self.height(direction);
            return (height > 0.0).then_some(height);
        }
        let (face, horizontal, vertical) = coordinates(direction);
        let size = self.environment.face_size;
        let index = face * size * size
            + ((vertical * size as f64) as usize).min(size - 1) * size
            + ((horizontal * size as f64) as usize).min(size - 1);
        let height = self.height(direction);
        // The heightmap (and earth water mask) decide what is dry: a positive height is land
        // even when the coarse 150 km cell is ocean or river (Uruk / hunter water-hole on the
        // Early Dynastic strip). Coarse biomes alone would leave the character only able to jump.
        if height <= 0.0 {
            return None;
        }
        let water = self.environment.water_m[index];
        let elevation = self.environment.elevation_m[index];
        // Standing water only where fill rose above the coarse ground (lakes, carved channels).
        // Flow-marked rivers on a flat plain set water_m == elevation; local relief of a few
        // metres must not count as drowning (Mesopotamian floodplain at the hunter spawn).
        if water != environment::DRY && water > elevation && f64::from(water) - height >= 0.5 {
            return None;
        }
        Some(height)
    }

    fn height(&self, direction: [f64; 3]) -> f64 {
        let (face, horizontal, vertical) = coordinates(direction);
        let horizontal = (horizontal * self.size as f64 - 0.5).clamp(0.0, (self.size - 1) as f64);
        let vertical = (vertical * self.size as f64 - 0.5).clamp(0.0, (self.size - 1) as f64);
        let column = horizontal as usize;
        let row = vertical as usize;
        let pixel = |column: usize, row: usize| {
            crate::terrain::metres(self.pixels[row * self.size * 6 + face * self.size + column])
        };
        let interpolate =
            |first: f64, second: f64, fraction: f64| first + (second - first) * fraction;
        let upper = interpolate(
            pixel(column, row),
            pixel((column + 1).min(self.size - 1), row),
            horizontal.fract(),
        );
        let lower = interpolate(
            pixel(column, (row + 1).min(self.size - 1)),
            pixel(
                (column + 1).min(self.size - 1),
                (row + 1).min(self.size - 1),
            ),
            horizontal.fract(),
        );
        let base = interpolate(upper, lower, vertical.fract());
        bridge_height(direction).unwrap_or_else(|| patch_height(direction, base).unwrap_or(base))
    }

    fn object_cell(&self, face: usize, column: i32, row: i32) -> Option<WorldObject> {
        let id = format!("{}:{face}:{column}:{row}", self.environment.seed);
        self.place(id, face, column, row, OBJECT_GRID, false)
    }

    fn fauna_cell(&self, face: usize, column: i32, row: i32) -> Option<WorldObject> {
        let id = format!("{}:fauna:{face}:{column}:{row}", self.environment.seed);
        let animal = self.place(id, face, column, row, FAUNA_GRID, true)?;
        // Wild land animals keep away from settlements, including where they stray to.
        (!is_land_animal(&animal.model) || !self.near_settlement(animal.position)).then_some(animal)
    }

    /// Whether a wild animal put here could wander into a settlement's built-up area.
    fn near_settlement(&self, position: [f64; 3]) -> bool {
        let key = format!(
            "{}:{}",
            self.environment.seed, self.environment.heightmap_sha256
        );
        let Ok(farms) = FARMS.read() else {
            return false;
        };
        if farms.0 != key {
            return false;
        }
        let direction = unit(position);
        farms.1.iter().any(|site| {
            length(std::array::from_fn(|axis| {
                (site.direction[axis] - direction[axis]) * RADIUS
            })) <= site.radius_m + WILD_WANDER_M + 20.0
        })
    }

    /// Water animals: placed below the water surface where the water is deep enough.
    fn place_aquatic(
        &self,
        id: String,
        point: [f64; 3],
        index: usize,
        biome: u8,
        sample: &dyn Fn(usize) -> f64,
    ) -> Option<WorldObject> {
        if sample(2) > FAUNA_PRESENCE {
            return None;
        }
        // The water of a landscape patch is a river, whatever the coarse map says about the land below it.
        let (level, biome) = match patch_water(point) {
            Some(surface) => (surface, 2),
            None => (f64::from(self.environment.water_m[index]), biome),
        };
        let depth = level - self.height(point);
        if depth < MIN_WATER_DEPTH_M {
            return None;
        }
        let candidates: Vec<_> = self
            .catalog
            .iter()
            .filter(|kind| kind.fauna && kind.aquatic && kind.biomes.contains(&biome))
            .collect();
        let total: f64 = candidates.iter().map(|kind| kind.weight).sum();
        let mut choice = sample(3) * total;
        let kind = candidates.into_iter().find(|kind| {
            choice -= kind.weight;
            choice <= 0.0
        })?;
        let deepest = kind.depth_m.min(depth - 1.5).max(1.0);
        let below = 0.8 + sample(6) * (deepest - 0.8).max(0.0);
        let scale_m = varied_scale(kind.scale_m, sample(5));
        Some(WorldObject {
            id,
            model: kind.id.clone(),
            position: point.map(|value| value * (RADIUS + level - below)),
            scale_m,
            yaw: sample(4) * std::f64::consts::TAU,
            collision_radius_m: 0.0,
            biome,
            ground_offset_m: kind.ground_offset_m,
        })
    }

    /// Reeds and similar plants standing on the bed of shallow water.
    fn place_reed_bed(
        &self,
        id: String,
        point: [f64; 3],
        mut biome: u8,
        sample: &dyn Fn(usize) -> f64,
    ) -> Option<WorldObject> {
        let overlay = wildlife_at(point);
        let bed = self.height(point);
        // Dense stand in a water-hole; thinner without an overlay.
        let presence = if overlay.is_some() { 0.55 } else { 0.22 };
        if sample(2) > presence {
            return None;
        }
        let overlay_weights = overlay.as_ref().map(|zone| zone.flora.as_slice());
        let from_overlay = overlay_weights.is_some_and(|w| !w.is_empty());
        let candidates: Vec<_> = if let Some(weights) = overlay_weights.filter(|w| !w.is_empty()) {
            weights
                .iter()
                .filter_map(|(model, weight, _)| {
                    self.catalog
                        .iter()
                        .find(|kind| kind.id == *model && !kind.fauna && kind.shallow_water)
                        .map(|kind| (kind, *weight))
                })
                .collect()
        } else {
            self.catalog
                .iter()
                // An empty biome list marks an overlay-only model; only a datadisk overlay grows it.
                .filter(|kind| !kind.fauna && kind.shallow_water && kind.biomes.contains(&biome))
                .map(|kind| (kind, kind.weight))
                .collect()
        };
        let total: f64 = candidates.iter().map(|(_, weight)| *weight).sum();
        if total <= 0.0 {
            return None;
        }
        let mut choice = sample(3) * total;
        let (kind, _) = candidates.into_iter().find(|(_, weight)| {
            choice -= *weight;
            choice <= 0.0
        })?;
        if from_overlay && !kind.biomes.is_empty() && !kind.biomes.contains(&biome) {
            biome = kind.biomes[0];
        } else if biome > 2 {
            biome = 2;
        }
        let scale_m = varied_scale(kind.scale_m, sample(5));
        Some(WorldObject {
            id,
            model: kind.id.clone(),
            position: point.map(|value| value * (RADIUS + bed)),
            scale_m,
            yaw: sample(4) * std::f64::consts::TAU,
            collision_radius_m: 0.0,
            biome,
            ground_offset_m: kind.ground_offset_m,
        })
    }

    /// Chooses the object of one grid cell from the hash of its identifier.
    fn place(
        &self,
        id: String,
        face: usize,
        column: i32,
        row: i32,
        grid: i32,
        fauna: bool,
    ) -> Option<WorldObject> {
        let hash = Sha256::digest(id.as_bytes());
        let sample = |index: usize| {
            f64::from(u32::from_le_bytes(
                hash[index * 4..index * 4 + 4].try_into().unwrap(),
            )) / 4_294_967_296.0
        };
        let point = cube_point(
            face,
            (f64::from(column) + 0.15 + sample(0) * 0.7) * 2.0 / f64::from(grid) - 1.0,
            1.0 - (f64::from(row) + 0.15 + sample(1) * 0.7) * 2.0 / f64::from(grid),
        );
        let (face, horizontal, vertical) = coordinates(point);
        let size = self.environment.face_size;
        let index = face * size * size
            + ((vertical * size as f64) as usize).min(size - 1) * size
            + ((horizontal * size as f64) as usize).min(size - 1);
        let mut biome = self.environment.biomes[index];
        // Dry tell/patch over drowned coarse cells: place and announce as grassland.
        if patch_dry(point) && biome < 3 {
            biome = 4;
        }
        // A datadisk's wildlife overlay lives on land: where it covers dry ground that the coarse map calls
        // river or sea (a floodplain, the ground above the water), game and plants stand there, not fish.
        if biome < 3 && wildlife_at(point).is_some() && self.surface(point).is_some() {
            biome = 4;
        }
        if fauna && (biome < 3 || patch_water(point).is_some()) {
            return self.place_aquatic(id, point, index, biome, &sample);
        }
        // Streets, houses and fields of a datadisk's city carry no wild trees, rocks, flowers or animals.
        if patch_cleared(point) {
            return None;
        }
        // Reeds grow from the bed of shallow water (and on the bank — handled below).
        let water_depth = self.water_depth(point);
        if !fauna && (REED_MIN_DEPTH_M..=REED_MAX_DEPTH_M).contains(&water_depth) {
            return self.place_reed_bed(id, point, biome, &sample);
        }
        let height = self.surface(point)?;
        let overlay = wildlife_at(point);
        let reference = if point[1].abs() < 0.9 {
            [0.0, 1.0, 0.0]
        } else {
            [1.0, 0.0, 0.0]
        };
        let tangent = unit(cross(point, reference));
        let second = cross(point, tangent);
        let slope = [tangent, second]
            .into_iter()
            .map(|tangent| {
                (self.height(unit(std::array::from_fn(|axis| {
                    point[axis] + tangent[axis] * 0.00001
                }))) - height)
                    .abs()
                    / (RADIUS * 0.00001)
            })
            .fold(0.0, f64::max);
        let mut density = overlay.as_ref().map(|zone| zone.density).unwrap_or(1.0);
        // A water-hole draws game and lush growth: around the water of a patch under a wildlife overlay
        // animals gather (up to three times as many), and gently sloping banks grow thicker than the
        // steppe. A steep, stony bank stays bare.
        if overlay.is_some() {
            if let Some(distance) = patch_water_distance(point, WATERSIDE_REACH_M) {
                let nearness = 1.0 - distance / WATERSIDE_REACH_M;
                if fauna {
                    // A water-hole draws a herd: up to five times as many animals on the bank.
                    density *= 1.0 + 4.0 * nearness;
                } else if slope <= LUSH_BANK_SLOPE {
                    density *= 1.0 + 2.5 * nearness;
                }
            }
        }
        let presence = if fauna {
            FAUNA_PRESENCE
        } else if biome == 5 {
            0.7
        } else {
            0.18
        };
        // Higher density lowers the random threshold so more cells spawn.
        let threshold = (presence * density).clamp(0.01, 0.95);
        if sample(2) > threshold {
            return None;
        }
        let overlay_weights: Option<&[(String, f64, bool)]> = overlay.as_ref().map(|zone| {
            if fauna {
                zone.fauna.as_slice()
            } else {
                zone.flora.as_slice()
            }
        });
        let from_overlay = overlay_weights.is_some_and(|w| !w.is_empty());
        // Probing the surroundings is costly, so only cells that ask about water pay for it.
        let near_water = std::cell::LazyCell::new(|| near_waterside(self, point));
        let waterside_here = from_overlay && *near_water;
        let candidates: Vec<_> = if let Some(weights) = overlay_weights.filter(|w| !w.is_empty()) {
            weights
                .iter()
                .filter_map(|(model, weight, waterside)| {
                    if *waterside && !waterside_here {
                        return None;
                    }
                    self.catalog
                        .iter()
                        .find(|kind| {
                            kind.id == *model
                                && kind.fauna == fauna
                                && !kind.aquatic
                                && (!kind.shallow_water || *near_water)
                                && slope <= kind.max_slope
                        })
                        .map(|kind| (kind, *weight))
                })
                .collect()
        } else {
            self.catalog
                .iter()
                .filter(|kind| {
                    kind.fauna == fauna
                        && !kind.aquatic
                        && (!kind.shallow_water || *near_water)
                        && kind.biomes.contains(&biome)
                        && slope <= kind.max_slope
                })
                .map(|kind| (kind, kind.weight))
                .collect()
        };
        let total: f64 = candidates.iter().map(|(_, weight)| *weight).sum();
        if total <= 0.0 {
            return None;
        }
        let mut choice = sample(3) * total;
        let (kind, _) = candidates.into_iter().find(|(_, weight)| {
            choice -= weight;
            choice <= 0.0
        })?;
        // Overlay flora/fauna may sit on a remapped grassland cell while the model is
        // beach-only; announce a biome the client catalog accepts.
        if from_overlay && !kind.biomes.is_empty() && !kind.biomes.contains(&biome) {
            biome = kind.biomes[0];
        }
        let scale_m = varied_scale(kind.scale_m, sample(5));
        let clearance = if kind.flying {
            kind.altitude_m * (0.85 + sample(6) * 0.3)
        } else {
            0.0
        };
        Some(WorldObject {
            id,
            model: kind.id.clone(),
            position: point.map(|value| value * (RADIUS + height + clearance)),
            scale_m,
            yaw: sample(4) * std::f64::consts::TAU,
            collision_radius_m: kind.collision_radius * scale_m,
            biome,
            ground_offset_m: kind.ground_offset_m,
        })
    }

    /// Objects of the cells around a point; `cell` picks the object of one cell.
    fn collect(
        &self,
        point: [f64; 3],
        span: i32,
        grid: i32,
        cell: impl Fn(usize, i32, i32) -> Option<WorldObject>,
    ) -> Vec<WorldObject> {
        let (face, horizontal, vertical) = coordinates(unit(point));
        let center_column = (horizontal * f64::from(grid)) as i32;
        let center_row = (vertical * f64::from(grid)) as i32;
        let mut seen = HashSet::new();
        let mut objects = Vec::new();
        for row in center_row - span..=center_row + span {
            for column in center_column - span..=center_column + span {
                let (target, horizontal, vertical) = coordinates(cube_point(
                    face,
                    (f64::from(column) + 0.5) * 2.0 / f64::from(grid) - 1.0,
                    1.0 - (f64::from(row) + 0.5) * 2.0 / f64::from(grid),
                ));
                let column = ((horizontal * f64::from(grid)) as i32).min(grid - 1);
                let row = ((vertical * f64::from(grid)) as i32).min(grid - 1);
                if seen.insert((target, column, row)) {
                    if let Some(object) = cell(target, column, row) {
                        objects.push(object);
                    }
                }
            }
        }
        objects
    }

    /// Animals near a point. They are passable scenery for now: they stand where the
    /// seed puts them and do not move or collide.
    pub(super) fn fauna(&self, point: [f64; 3]) -> Vec<WorldObject> {
        let mut animals = self.collect(point, 2, FAUNA_GRID, |face, column, row| {
            self.fauna_cell(face, column, row)
        });
        animals.retain(|animal| {
            length(std::array::from_fn(|axis| {
                animal.position[axis] - point[axis]
            })) <= FAUNA_RADIUS_M
                && !self.is_removed(&animal.id)
        });
        animals.sort_by(|first, second| first.id.cmp(&second.id));
        animals
    }

    /// The biome of the map cell in this direction. Dry ground inside a coarse sea/river
    /// cell (landscape patch or floodplain above the water) is grassland, matching
    /// [`Self::land_site`].
    fn biome_at(&self, direction: [f64; 3]) -> u8 {
        let (face, horizontal, vertical) = coordinates(direction);
        let size = self.environment.face_size;
        let mut biome = self.environment.biomes[face * size * size
            + ((vertical * size as f64) as usize).min(size - 1) * size
            + ((horizontal * size as f64) as usize).min(size - 1)];
        if biome < 3 && self.surface(direction).is_some() {
            biome = 4;
        }
        biome
    }

    /// The animals of the pastures beside the settlements near a point, as the seed puts them.
    pub(super) fn farm_animals(&self, point: [f64; 3]) -> Vec<WorldObject> {
        let key = format!(
            "{}:{}",
            self.environment.seed, self.environment.heightmap_sha256
        );
        let Ok(farms) = FARMS.read() else {
            return Vec::new();
        };
        if farms.0 != key {
            return Vec::new();
        }
        let direction = unit(point);
        let mut animals = Vec::new();
        for site in &farms.1 {
            let near = length(std::array::from_fn(|axis| {
                (site.direction[axis] - direction[axis]) * RADIUS
            })) <= FARM_RANGE_M + site.radius_m * 2.0;
            if near {
                animals.extend(
                    (0..farm_herd(site.radius_m) + flock_size(site.radius_m))
                        .filter_map(|number| self.farm_animal(site, number))
                        .filter(|animal| !self.is_removed(&animal.id)),
                );
            }
        }
        animals.sort_by(|first, second| first.id.cmp(&second.id));
        animals
    }

    /// The fold of a settlement: the middle of the pasture beside it, where its flock rests.
    fn fold_of(&self, site: &FarmSite) -> [f64; 3] {
        let heading = fold_heading(&self.environment.seed, &site.id);
        let distance = site.radius_m + FOLD_M;
        crate::scenery::world_from_local(
            site.direction,
            heading.cos() * distance,
            heading.sin() * distance,
            |_| 0.0,
            RADIUS,
        )
        .0
    }

    /// One animal of a settlement's pasture: a herd that grazes together, beyond the buildings. The numbers
    /// beyond the village's own animals are its flock of sheep and goats, driven out by the shepherd.
    fn farm_animal(&self, site: &FarmSite, number: usize) -> Option<WorldObject> {
        let id = format!("{}:farm:{number}:{}", self.environment.seed, site.id);
        let word = |hash: &[u8], index: usize| {
            f64::from(u32::from_le_bytes(
                hash[index * 4..index * 4 + 4].try_into().unwrap(),
            )) / 4_294_967_296.0
        };
        // The pasture of the whole settlement and the spot of this animal in it.
        let hash = Sha256::digest(id.as_bytes());
        let heading = fold_heading(&self.environment.seed, &site.id);
        let distance = site.radius_m + FOLD_M;
        let flock = number >= farm_herd(site.radius_m);
        let spread = word(&hash, 1).sqrt() * if flock { FLOCK_SPREAD_M } else { 12.0 };
        let angle = word(&hash, 2) * std::f64::consts::TAU;
        let x = heading.cos() * distance + angle.cos() * spread;
        let z = heading.sin() * distance + angle.sin() * spread;
        let (direction, _) =
            crate::scenery::world_from_local(site.direction, x, z, |_| 0.0, RADIUS);
        let height = self.surface(direction)?;
        let model = if flock {
            if word(&hash, 5) < GOAT_SHARE {
                "animal.goat"
            } else {
                "animal.sheep"
            }
        } else {
            let mut choice =
                word(&hash, 0) * FARM_SPECIES.iter().map(|(_, weight)| weight).sum::<f64>();
            FARM_SPECIES
                .iter()
                .find(|(_, weight)| {
                    choice -= weight;
                    choice <= 0.0
                })
                .map_or(FARM_SPECIES[0].0, |(model, _)| model)
        };
        let kind = self.catalog.iter().find(|kind| kind.id == model)?;
        let scale_m = varied_scale(kind.scale_m, word(&hash, 3));
        Some(WorldObject {
            id,
            model: model.to_owned(),
            position: direction.map(|value| value * (RADIUS + height)),
            scale_m,
            yaw: word(&hash, 4) * std::f64::consts::TAU,
            collision_radius_m: 0.0,
            biome: self.biome_at(direction),
            ground_offset_m: kind.ground_offset_m,
        })
    }

    /// The route of an animal of a settlement's flock, from the start of the current time step: it walks with the
    /// herd between fold and pasture and mills about its centre. `None` for any other animal.
    fn flock_route(&self, animal: &WorldObject, now_ms: i64) -> Option<Vec<[f64; 4]>> {
        let rest = animal
            .id
            .strip_prefix(&format!("{}:farm:", self.environment.seed))?;
        let (number, site_id) = rest.split_once(':')?;
        let number: usize = number.parse().ok()?;
        let key = format!(
            "{}:{}",
            self.environment.seed, self.environment.heightmap_sha256
        );
        let farms = FARMS.read().ok()?;
        let site = (farms.0 == key)
            .then(|| farms.1.iter().find(|candidate| candidate.id == site_id))??;
        if number < farm_herd(site.radius_m) {
            return None;
        }
        let fold = self.fold_of(site);
        let hash = Sha256::digest(animal.id.as_bytes());
        let word = |index: usize| {
            f64::from(u32::from_le_bytes(
                hash[index * 4..index * 4 + 4].try_into().unwrap(),
            )) / 4_294_967_296.0
        };
        // Its own place in the herd: a fixed spot about the centre, and a slow milling about that.
        let angle = word(6) * std::f64::consts::TAU;
        let (own_x, own_z) = (
            angle.cos() * word(7).sqrt() * FLOCK_SPREAD_M,
            angle.sin() * word(7).sqrt() * FLOCK_SPREAD_M,
        );
        let phase = word(0) * std::f64::consts::TAU;
        let first = now_ms.div_euclid(FLOCK_STEP_MS) * FLOCK_STEP_MS;
        Some(
            (0..=FLOCK_POINTS)
                .map(|step| {
                    let time = first + step * FLOCK_STEP_MS;
                    let centre = crate::herd::centre(fold, site.pasture, time + site.phase_ms);
                    let sway = time as f64 / 1000.0 / 37.0 + phase;
                    let (direction, _) = crate::scenery::world_from_local(
                        centre,
                        own_x + sway.sin() * 2.5,
                        own_z + sway.cos() * 2.5,
                        |_| 0.0,
                        RADIUS,
                    );
                    let metres = RADIUS + self.height(direction).max(0.0);
                    let round = |value: f64| (value * 100.0).round() / 100.0;
                    let point = direction.map(|value| round(value * metres));
                    [time as f64, point[0], point[1], point[2]]
                })
                .collect(),
        )
    }

    /// Where an animal heads to at the start of a slot: a spot near where it was put, on land.
    fn wander_waypoint(&self, id: &str, anchor: [f64; 3], slot: i64, radius: f64) -> [f64; 3] {
        let hash = Sha256::digest(format!("{id}:wander:{slot}").as_bytes());
        let word = |index: usize| {
            f64::from(u32::from_le_bytes(
                hash[index * 4..index * 4 + 4].try_into().unwrap(),
            )) / 4_294_967_296.0
        };
        let reference = if anchor[1].abs() < 0.9 {
            [0.0, 1.0, 0.0]
        } else {
            [1.0, 0.0, 0.0]
        };
        let tangent = unit(cross(anchor, reference));
        let second = cross(anchor, tangent);
        let angle = word(0) * std::f64::consts::TAU;
        let distance = word(1).sqrt() * radius;
        let direction = unit(std::array::from_fn(|axis| {
            anchor[axis] * RADIUS
                + (tangent[axis] * angle.cos() + second[axis] * angle.sin()) * distance
        }));
        if self.surface(direction).is_some() {
            direction
        } else {
            anchor
        }
    }

    fn fauna_kind(&self, model: &str) -> Option<&ObjectKind> {
        self.catalog.iter().find(|kind| kind.id == model)
    }

    /// Clearance above the terrain for a flying animal at one route slot (metres).
    fn flight_clearance(&self, animal: &WorldObject, slot: i64) -> f64 {
        let altitude = self
            .fauna_kind(&animal.model)
            .map(|kind| kind.altitude_m)
            .unwrap_or(default_altitude());
        let hash = Sha256::digest(format!("{}:fly:{slot}", animal.id).as_bytes());
        let sample =
            f64::from(u32::from_le_bytes(hash[0..4].try_into().unwrap())) / 4_294_967_296.0;
        altitude * (0.85 + sample * 0.3)
    }

    /// Graze spot of a waterhole animal: among the grass away from the bank, seeded from its id.
    fn waterhole_graze(&self, animal: &WorldObject, drink: [f64; 3]) -> [f64; 3] {
        let hash = Sha256::digest(format!("{}:waterhole-graze", animal.id).as_bytes());
        let word = |index: usize| {
            f64::from(u32::from_le_bytes(
                hash[index * 4..index * 4 + 4].try_into().unwrap(),
            )) / 4_294_967_296.0
        };
        let distance =
            WATERHOLE_GRAZE_MIN_M + word(1) * (WATERHOLE_GRAZE_MAX_M - WATERHOLE_GRAZE_MIN_M);
        // Prefer dry land farther from water; a single random bearing often points into the pond.
        let base = word(0) * std::f64::consts::TAU;
        for step in 0..8 {
            let angle = base + f64::from(step) * std::f64::consts::TAU / 8.0;
            let (candidate, _) = crate::scenery::world_from_local(
                drink,
                angle.cos() * distance,
                angle.sin() * distance,
                |_| 0.0,
                RADIUS,
            );
            if self.surface(candidate).is_none() || patch_water(candidate).is_some() {
                continue;
            }
            if nearest_water_bank(candidate, WATERHOLE_REACH_M)
                .is_some_and(|(metres, _)| metres >= WATERHOLE_GRAZE_MIN_M * 0.6)
            {
                return candidate;
            }
        }
        // Last resort: walk inland from the bank along the outward radial of the seed place.
        let anchor = unit(animal.position);
        let (outward, _) = crate::scenery::world_from_local(
            drink,
            // A short push away from the drink spot toward the seed, then beyond it.
            0.0,
            WATERHOLE_GRAZE_MIN_M + 10.0,
            |_| 0.0,
            RADIUS,
        );
        if self.surface(outward).is_some() && patch_water(outward).is_none() {
            outward
        } else if self.surface(anchor).is_some() {
            anchor
        } else {
            drink
        }
    }

    /// Drink → graze → return cycle for land animals near a patch water-hole. No state: only id and time.
    /// `None` for birds, farm animals, or anyone farther than [`WATERHOLE_REACH_M`] from patch water.
    fn waterhole_route(&self, animal: &WorldObject, now_ms: i64) -> Option<Vec<[f64; 4]>> {
        if animal.id.contains(":farm:") {
            return None;
        }
        if self
            .fauna_kind(&animal.model)
            .is_some_and(|kind| kind.flying || kind.aquatic)
        {
            return None;
        }
        let anchor = unit(animal.position);
        // Only animals of a wildlife overlay join the cycle: open-world fauna keep the short wander.
        wildlife_at(anchor)?;
        let (_, drink) = nearest_water_bank(anchor, WATERHOLE_REACH_M)?;
        let graze = self.waterhole_graze(animal, drink);
        let walk_m = length(std::array::from_fn(|axis| {
            (drink[axis] - graze[axis]) * RADIUS
        }))
        .clamp(8.0, WATERHOLE_GRAZE_MAX_M + 10.0);
        let walk_ms = ((walk_m / WATERHOLE_SPEED_M_S) * 1000.0).round() as i64;
        let cycle_ms = WATERHOLE_GRAZE_MS + walk_ms + WATERHOLE_DRINK_MS + walk_ms;
        let hash = Sha256::digest(format!("{}:waterhole-phase", animal.id).as_bytes());
        let phase_ms = (f64::from(u32::from_le_bytes(hash[0..4].try_into().unwrap()))
            / 4_294_967_296.0
            * cycle_ms as f64) as i64;
        let sway_phase = f64::from(u32::from_le_bytes(hash[4..8].try_into().unwrap()))
            / 4_294_967_296.0
            * std::f64::consts::TAU;
        let along = |time_ms: i64| -> [f64; 3] {
            let into = (time_ms + phase_ms).rem_euclid(cycle_ms);
            let centre = if into < WATERHOLE_GRAZE_MS {
                graze
            } else if into < WATERHOLE_GRAZE_MS + walk_ms {
                let fraction = (into - WATERHOLE_GRAZE_MS) as f64 / walk_ms as f64;
                unit(std::array::from_fn(|axis| {
                    graze[axis] + (drink[axis] - graze[axis]) * fraction
                }))
            } else if into < WATERHOLE_GRAZE_MS + walk_ms + WATERHOLE_DRINK_MS {
                drink
            } else {
                let fraction = (into - WATERHOLE_GRAZE_MS - walk_ms - WATERHOLE_DRINK_MS) as f64
                    / walk_ms as f64;
                unit(std::array::from_fn(|axis| {
                    drink[axis] + (graze[axis] - drink[axis]) * fraction
                }))
            };
            // Mill a little while grazing or drinking so the herd is not frozen in place.
            let standing = into < WATERHOLE_GRAZE_MS
                || (into >= WATERHOLE_GRAZE_MS + walk_ms
                    && into < WATERHOLE_GRAZE_MS + walk_ms + WATERHOLE_DRINK_MS);
            if standing {
                let sway = time_ms as f64 / 1000.0 / 29.0 + sway_phase;
                let (milled, _) = crate::scenery::world_from_local(
                    centre,
                    sway.sin() * 2.2,
                    sway.cos() * 2.2,
                    |_| 0.0,
                    RADIUS,
                );
                if self.surface(milled).is_some() && patch_water(milled).is_none() {
                    milled
                } else {
                    centre
                }
            } else {
                centre
            }
        };
        let first = now_ms.div_euclid(WATERHOLE_STEP_MS) * WATERHOLE_STEP_MS;
        let round = |value: f64| (value * 100.0).round() / 100.0;
        Some(
            (0..=WATERHOLE_POINTS)
                .map(|step| {
                    let time = first + step * WATERHOLE_STEP_MS;
                    let direction = along(time);
                    let metres = RADIUS + self.height(direction).max(0.0);
                    let point = direction.map(|value| round(value * metres));
                    [time as f64, point[0], point[1], point[2]]
                })
                .collect(),
        )
    }

    /// Waypoints of a land animal (or bird) from the start of the current slot onwards.
    pub(super) fn animal_route(&self, animal: &WorldObject, now_ms: i64) -> Vec<[f64; 4]> {
        self.flee_route(animal, now_ms)
            .unwrap_or_else(|| self.usual_route(animal, now_ms))
    }

    /// Sends a struck animal running away from `hunter` (metres from the centre).
    pub(super) fn start_flee(&self, animal: &WorldObject, hunter: [f64; 3], now_ms: i64) {
        let here = unit(animal.position);
        let away: [f64; 3] = std::array::from_fn(|axis| animal.position[axis] - hunter[axis]);
        let along = away.iter().zip(here).map(|(a, b)| a * b).sum::<f64>();
        let mut tangent: [f64; 3] = std::array::from_fn(|axis| away[axis] - here[axis] * along);
        if length(tangent) < 1e-6 {
            // Struck from straight above or below: any way along the ground will do.
            tangent = [-here[1], here[0], 0.0];
        }
        let tangent = unit(tangent);
        let target = unit(std::array::from_fn(|axis| {
            here[axis] + tangent[axis] * FLEE_DISTANCE_M / RADIUS
        }));
        let metres = RADIUS + self.height(target).max(0.0);
        let to = target.map(|value| value * metres);
        let mut flees = FLEES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let flees = flees.get_or_insert_with(HashMap::new);
        flees.retain(|_, (start, _, _)| {
            now_ms - *start < FLEE_RUN_MS + FLEE_REST_MS + FLEE_RETURN_MS
        });
        flees.insert(animal.id.clone(), (now_ms, animal.position, to));
    }

    /// The run, the rest and the walk back of a fleeing animal, while it lasts.
    fn flee_route(&self, animal: &WorldObject, now_ms: i64) -> Option<Vec<[f64; 4]>> {
        let (start, from, to) = *FLEES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()?
            .get(&animal.id)?;
        let end = start + FLEE_RUN_MS + FLEE_REST_MS + FLEE_RETURN_MS;
        if now_ms >= end {
            return None;
        }
        let back = route_position(&self.usual_route(animal, end), end);
        let entry = |time: i64, point: [f64; 3]| [time as f64, point[0], point[1], point[2]];
        Some(vec![
            entry(start, from),
            entry(start + FLEE_RUN_MS, to),
            entry(start + FLEE_RUN_MS + FLEE_REST_MS, to),
            entry(end, back),
        ])
    }

    fn usual_route(&self, animal: &WorldObject, now_ms: i64) -> Vec<[f64; 4]> {
        if let Some(route) = self.flock_route(animal, now_ms) {
            return route;
        }
        if let Some(route) = self.waterhole_route(animal, now_ms) {
            return route;
        }
        let flying = self
            .fauna_kind(&animal.model)
            .is_some_and(|kind| kind.flying);
        let radius = if animal.id.contains(":farm:") {
            FARM_WANDER_M
        } else if flying {
            FLY_WANDER_M
        } else {
            WILD_WANDER_M
        };
        let anchor = unit(animal.position);
        let first = now_ms.div_euclid(WANDER_SLOT_MS);
        let entry = |time: i64, direction: [f64; 3], slot: i64| {
            let ground = self.height(direction).max(0.0);
            let clearance = if flying {
                self.flight_clearance(animal, slot)
            } else {
                0.0
            };
            let metres = RADIUS + ground + clearance;
            let round = |value: f64| (value * 100.0).round() / 100.0;
            let position = direction.map(|value| round(value * metres));
            [time as f64, position[0], position[1], position[2]]
        };
        let mut route = Vec::new();
        for slot in first..first + WANDER_SLOTS {
            let from = self.wander_waypoint(&animal.id, anchor, slot, radius);
            let to = self.wander_waypoint(&animal.id, anchor, slot + 1, radius);
            for part in 0..=WANDER_PARTS {
                let fraction = part as f64 / WANDER_PARTS as f64;
                let direction = unit(std::array::from_fn(|axis| {
                    from[axis] + (to[axis] - from[axis]) * fraction
                }));
                route.push(entry(
                    slot * WANDER_SLOT_MS + (WANDER_MOVE_MS as f64 * fraction) as i64,
                    direction,
                    slot,
                ));
            }
        }
        let end = first + WANDER_SLOTS;
        route.push(entry(
            end * WANDER_SLOT_MS,
            self.wander_waypoint(&animal.id, anchor, end, radius),
            end,
        ));
        route
    }

    /// Land animals and birds near a point with where they are at `now_ms` and where they go next;
    /// fish are only drawn and keep their place.
    pub(super) fn moving_animals(
        &self,
        point: [f64; 3],
        now_ms: i64,
    ) -> Vec<(WorldObject, Option<Vec<[f64; 4]>>)> {
        let mut wild = self.fauna(point);
        // A water-hole herd is denser than the open steppe; keep room for the drinkers.
        wild.truncate(96);
        let mut farm = self.farm_animals(point);
        farm.truncate(FARM_ANIMALS_MAX);
        wild.into_iter()
            .chain(farm)
            .map(|mut animal| {
                if self
                    .fauna_kind(&animal.model)
                    .is_some_and(|kind| kind.aquatic)
                {
                    return (animal, None);
                }
                let route = self.animal_route(&animal, now_ms);
                animal.position = route_position(&route, now_ms);
                (animal, Some(route))
            })
            .collect()
    }

    /// A land animal by its id (`<seed>:fauna:…` or `<seed>:farm:…`) standing where it is at
    /// `now_ms`, unless it was butchered and has not returned yet.
    pub(super) fn animal_by_id(&self, id: &str, now_ms: i64) -> Option<WorldObject> {
        if self.is_removed(id) {
            return None;
        }
        let seed = &self.environment.seed;
        let mut animal = if let Some(rest) = id.strip_prefix(&format!("{seed}:fauna:")) {
            let mut parts = rest.split(':');
            let face: usize = parts.next()?.parse().ok()?;
            let column: i32 = parts.next()?.parse().ok()?;
            let row: i32 = parts.next()?.parse().ok()?;
            if parts.next().is_some()
                || face > 5
                || !(0..FAUNA_GRID).contains(&column)
                || !(0..FAUNA_GRID).contains(&row)
            {
                return None;
            }
            self.fauna_cell(face, column, row)?
        } else if let Some(rest) = id.strip_prefix(&format!("{seed}:farm:")) {
            let (number, site) = rest.split_once(':')?;
            let number: usize = number.parse().ok()?;
            let key = format!("{seed}:{}", self.environment.heightmap_sha256);
            let farms = FARMS.read().ok()?;
            let site = (farms.0 == key)
                .then(|| farms.1.iter().find(|candidate| candidate.id == site))??;
            if number >= farm_herd(site.radius_m) + flock_size(site.radius_m) {
                return None;
            }
            self.farm_animal(site, number)?
        } else {
            return None;
        };
        if animal.id != id || !is_land_animal(&animal.model) {
            return None;
        }
        let route = self.animal_route(&animal, now_ms);
        animal.position = route_position(&route, now_ms);
        Some(animal)
    }

    /// What blocks a step: harvestable scenery plus the walls and fences of places.
    pub(super) fn walking_objects(&self, point: [f64; 3], span: i32) -> Vec<WorldObject> {
        let mut objects = self.objects(point, span);
        let key = format!(
            "{}:{}",
            self.environment.seed, self.environment.heightmap_sha256
        );
        if let Ok(statics) = STATICS.read() {
            if statics.0 == key {
                // Only what is near the walker matters; steps are a few metres long.
                objects.extend(
                    statics
                        .1
                        .iter()
                        .filter(|obstacle| {
                            (0..3).all(|axis| {
                                (obstacle.position[axis] - point[axis]).abs()
                                    < 60.0 + obstacle.collision_radius_m
                            })
                        })
                        .cloned(),
                );
            }
        }
        objects
    }

    pub(super) fn objects(&self, point: [f64; 3], span: i32) -> Vec<WorldObject> {
        let mut objects = self.collect(point, span, OBJECT_GRID, |face, column, row| {
            self.object_cell(face, column, row)
        });
        if let Ok(removed) = self.removed.0.read() {
            let now = unix_now();
            objects = objects
                .into_iter()
                .filter_map(|mut object| match removed.get(&object.id) {
                    Some((until, leaves)) if *until > now => {
                        leaves.as_ref().map(|(model, scale)| {
                            // The remains are small and can be walked over.
                            object.model.clone_from(model);
                            object.scale_m *= scale;
                            object.collision_radius_m = 0.0;
                            object
                        })
                    }
                    _ => Some(object),
                })
                .collect();
        }
        objects.sort_by(|first, second| {
            let squared = |object: &WorldObject| {
                object
                    .position
                    .into_iter()
                    .zip(point)
                    .map(|(first, second)| (first - second).powi(2))
                    .sum::<f64>()
            };
            squared(first)
                .total_cmp(&squared(second))
                .then_with(|| first.id.cmp(&second.id))
        });
        objects
    }

    #[cfg(test)]
    fn step(&self, point: [f64; 3], intent: [f64; 3], seconds: f64) -> Option<[f64; 3]> {
        self.step_at_speed(point, intent, seconds, WALK_SPEED)
    }

    /// Whether a character walking the straight line between two directions would meet a wall of a
    /// building, a fence, a tree or a rock on the way (a step of 0.7 m is climbed, as by players).
    pub(super) fn walk_blocked(&self, from: [f64; 3], to: [f64; 3]) -> bool {
        let (from, to) = (unit(from), unit(to));
        let ground =
            |direction: [f64; 3]| direction.map(|v| v * (RADIUS + self.height(direction).max(0.0)));
        let middle = unit(std::array::from_fn(|axis| (from[axis] + to[axis]) / 2.0));
        let objects = self.walking_objects(ground(middle), 4);
        let metres = RADIUS * dot(from, to).clamp(-1.0, 1.0).acos();
        let steps = (metres / 2.0).ceil().max(1.0) as usize;
        let mut previous = ground(from);
        for index in 1..=steps {
            let fraction = index as f64 / steps as f64;
            let next = ground(unit(std::array::from_fn(|axis| {
                from[axis] + (to[axis] - from[axis]) * fraction
            })));
            let up = unit(previous);
            let lifted: [f64; 3] =
                std::array::from_fn(|axis| previous[axis] + up[axis] * STAIR_STEP_HEIGHT);
            let motion: [f64; 3] = std::array::from_fn(|axis| next[axis] - previous[axis]);
            if !clear_path(previous, next, &objects)
                || capsule_hit(lifted, motion, &objects).is_some()
            {
                return true;
            }
            previous = next;
        }
        false
    }

    #[cfg(test)]
    fn step_at_speed(
        &self,
        point: [f64; 3],
        intent: [f64; 3],
        seconds: f64,
        speed: f64,
    ) -> Option<[f64; 3]> {
        self.step_scaled(point, intent, seconds, speed, 1.0)
    }

    /// A step of a character `scale` times the height of a man (see [`Self::in_water_for`]).
    fn step_scaled(
        &self,
        point: [f64; 3],
        intent: [f64; 3],
        seconds: f64,
        speed: f64,
        scale: f64,
    ) -> Option<[f64; 3]> {
        let radius = length(point);
        if !(RADIUS - 8000.0..=RADIUS + 8012.0).contains(&radius) {
            return None;
        }
        let up = point.map(|value| value / radius);
        // Wading through water that is not yet deep enough to swim in is slower than walking.
        let depth = self.water_depth(up);
        let wade_depth = WADE_DEPTH_M * scale;
        let speed = if depth > 0.0 && depth <= wade_depth {
            speed * WADE_SPEED_FACTOR
        } else {
            speed
        };
        let radial = intent
            .iter()
            .zip(up)
            .map(|(value, axis)| value * axis)
            .sum::<f64>();
        let tangent: [f64; 3] = std::array::from_fn(|axis| intent[axis] - up[axis] * radial);
        let distance = length(tangent) * speed * seconds.clamp(0.0, 0.25);
        if distance < 0.000001 {
            return None;
        }
        let intermediate: [f64; 3] = std::array::from_fn(|axis| {
            point[axis] + tangent[axis] * speed * seconds.clamp(0.0, 0.25)
        });
        let unit = intermediate.map(|value| value / length(intermediate));
        let height = if self.in_water_for(up, scale) || self.in_water_for(unit, scale) {
            // Swimming (or about to): no slope limit, the swimmer floats at the ground level (sea level at
            // the least) and may leave the water wherever it is shallow enough to wade.
            // The step was already slowed to a wade when the swimmer is only about to leave the shallows.
            let swim_factor = if depth > 0.0 && depth <= wade_depth {
                SWIM_SPEED_FACTOR / WADE_SPEED_FACTOR
            } else {
                SWIM_SPEED_FACTOR
            };
            let swim_point: [f64; 3] = std::array::from_fn(|axis| {
                point[axis] + (intermediate[axis] - point[axis]) * swim_factor
            });
            let swim_unit = swim_point.map(|value| value / length(swim_point));
            let height = self
                .surface(swim_unit)
                .unwrap_or_else(|| self.height(swim_unit).max(0.0));
            let destination = swim_unit.map(|value| value * (RADIUS + height));
            return clear_path(point, destination, &self.walking_objects(point, 2))
                .then_some(destination);
        } else {
            self.walkable(unit, scale)?
        };
        let previous = self.walkable(up, scale)?;
        if (height - previous).abs() > distance * 0.7 {
            return None;
        }
        let destination = unit.map(|value| value * (RADIUS + height));
        clear_path(point, destination, &self.walking_objects(point, 2)).then_some(destination)
    }
}

/// What kind of water a character stands next to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WaterNear {
    None,
    Salt,
    Fresh,
}

pub(super) async fn terrain(state: &AppState) -> Result<Arc<WalkingTerrain>, players::Error> {
    let unavailable = || {
        players::Error::Status(
            StatusCode::SERVICE_UNAVAILABLE,
            "walking terrain unavailable",
        )
    };
    let (size, seed, sha256): (i32, String, String) =
        sqlx::query_as("SELECT face_size, seed, sha256 FROM heightmaps WHERE world_id = $1")
            .bind(state.world_id)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(unavailable)?;
    let painted = super::story::world::painted_biomes(state).await;
    let key = format!(
        "{}:{sha256}:{seed}:{size}:{}",
        state.world_id,
        painted.as_ref().map_or("", |(_, disks)| disks.as_str())
    );
    let mut cache = TERRAIN.lock().await;
    if let Some((stored, terrain)) = cache.as_ref() {
        if stored == &key {
            return Ok(terrain.clone());
        }
    }
    let pixels: Vec<u8> =
        sqlx::query_scalar("SELECT pixels FROM heightmaps WHERE world_id = $1 AND sha256 = $2")
            .bind(state.world_id)
            .bind(sha256.clone())
            .fetch_one(&state.pool)
            .await?;
    let pixels = crate::terrain::samples_from_bytes(&pixels);
    let terrain = tokio::task::spawn_blocking(move || {
        let mut catalog = object_catalog()?;
        catalog.extend(fauna_catalog()?);
        let biomes = painted.as_ref().map(|(map, _)| map.as_ref());
        environment::generate_with(size as usize, &pixels, &seed, &sha256, biomes).map(
            |environment| {
                Arc::new(WalkingTerrain {
                    size: size as usize,
                    pixels,
                    environment,
                    catalog,
                    removed: Removed::default(),
                })
            },
        )
    })
    .await
    .map_err(|_| unavailable())?
    .ok_or_else(unavailable)?;
    let depleted: Vec<(String, i64)> = sqlx::query_as("SELECT object_id, extract(epoch FROM depleted_until)::bigint FROM world_object_state WHERE world_id = $1 AND depleted_until > now()")
        .bind(state.world_id)
        .fetch_all(&state.pool)
        .await?;
    for (id, until) in depleted {
        let leaves = terrain
            .object_cell_by_id(&id)
            .and_then(|object| super::gathering::leaves_for(&object.model));
        terrain.mark_removed(&id, until, leaves);
    }
    *cache = Some((key, terrain.clone()));
    Ok(terrain)
}

pub(super) async fn world_objects(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<RegionQuery>,
) -> Result<Json<ObjectRegion>, players::Error> {
    let point = [query.x, query.y, query.z];
    if point.iter().any(|value| !value.is_finite())
        || !(RADIUS - 8000.0..=RADIUS + 8000.0).contains(&length(point))
    {
        return Err(players::Error::Status(
            StatusCode::BAD_REQUEST,
            "invalid object region",
        ));
    }
    let player_id = match players::token_hash(&headers) {
        Ok(hash) => players::player_id(&state, &hash).await.ok(),
        Err(_) => None,
    };
    let terrain = terrain(&state).await?;
    let worker = terrain.clone();
    let now_ms = unix_now_ms();
    let objects = tokio::task::spawn_blocking(move || {
        let mut objects = worker.objects(point, 16);
        objects.truncate(512 - 64 - FARM_ANIMALS_MAX);
        let objects: Vec<_> = objects.into_iter().map(|object| (object, None)).collect();
        let animals = worker.moving_animals(point, now_ms);
        objects.into_iter().chain(animals).collect::<Vec<_>>()
    })
    .await
    .map_err(|_| {
        players::Error::Status(StatusCode::SERVICE_UNAVAILABLE, "object region unavailable")
    })?;
    Ok(Json(ObjectRegion {
        memorials: super::survival::memorials(&state, unit(point)).await?,
        npcs: super::story::world::npcs_near(&state, point, player_id).await,
        areas: super::story::world::areas_near(&state, point).await,
        props: super::story::world::props_near(&state, point).await,
        terrains: super::story::world::terrains_near(&state, point).await,
        loot: super::loot::near(&state, point).await.unwrap_or_default(),
        version: 1,
        heightmap_sha256: terrain.environment.heightmap_sha256.clone(),
        seed: terrain.environment.seed.clone(),
        server_time_ms: now_ms,
        objects: objects
            .into_iter()
            .map(|(object, wander)| ObjectDescriptor {
                harvest: super::gathering::harvest_info(&object.model),
                object,
                wander,
            })
            .collect(),
    }))
}

pub(super) async fn world_memorials(
    State(state): State<AppState>,
    Query(query): Query<RegionQuery>,
) -> Result<Json<Vec<super::survival::Memorial>>, players::Error> {
    let point = [query.x, query.y, query.z];
    if point.iter().any(|value| !value.is_finite())
        || !(RADIUS - 8000.0..=RADIUS + 8000.0).contains(&length(point))
    {
        return Err(players::Error::Status(
            StatusCode::BAD_REQUEST,
            "invalid memorial region",
        ));
    }
    Ok(Json(super::survival::memorials(&state, unit(point)).await?))
}

pub(super) async fn walk(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut intent): Json<Intent>,
) -> Result<Json<Reply>, players::Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let sequence = intent
        .sequence
        .parse::<i64>()
        .ok()
        .filter(|value| *value > 0 && value.to_string() == intent.sequence);
    if sequence.is_none()
        || intent.direction.iter().any(|value| !value.is_finite())
        || length(intent.direction) > 1.000001
    {
        return Err(players::Error::Status(
            StatusCode::BAD_REQUEST,
            "invalid movement intent",
        ));
    }
    let sequence = sequence.unwrap();
    let terrain = terrain(&state).await?;
    let _ = super::story::world::world(&state).await; // loads the walls and fences that block steps
    let mut transaction = state.pool.begin().await?;
    if !super::survival::lock_alive(&state, &mut transaction, player_id).await? {
        let notice = super::survival::obituary(&mut transaction, player_id).await?;
        transaction.commit().await?;
        return Err(players::Error::Obituary(Box::new(notice)));
    }
    let mut position: Position = sqlx::query_as("SELECT position_x AS x, position_y AS y, position_z AS z, movement_sequence::text AS sequence, movement_airborne AS airborne, movement_support AS on_object FROM players WHERE id = $1 AND position_x IS NOT NULL FOR UPDATE")
        .bind(player_id).fetch_optional(&mut *transaction).await?
        .ok_or(players::Error::Status(StatusCode::CONFLICT, "player position unavailable"))?;
    let mut moving = false;
    if sequence > position.sequence.parse::<i64>().unwrap_or(i64::MAX) {
        let stamina: i32 = sqlx::query_scalar("SELECT stamina FROM players WHERE id = $1")
            .bind(player_id)
            .fetch_one(&mut *transaction)
            .await?;
        intent.run &= stamina > 0;
        let here = unit([position.x, position.y, position.z]);
        intent.body_scale = body_scale(&state, &mut transaction, player_id).await?;
        let swimming = terrain.in_water_for(here, intent.scale());
        intent.run &= terrain.water_depth(here) <= RUN_DEPTH_M * intent.scale();
        // A jump costs stamina up front and is refused when too little is left.
        intent.jump &=
            !position.airborne && !swimming && f64::from(stamina) >= super::survival::JUMP_STAMINA;
        let wants_motion = intent.jump || length(intent.direction) > 0.000001;
        if wants_motion {
            // Standing up from a rock before walking or jumping.
            sqlx::query("UPDATE players SET sitting_object_id = NULL WHERE id = $1 AND sitting_object_id IS NOT NULL")
                .bind(player_id)
                .execute(&mut *transaction)
                .await?;
        }
        let takes_off = intent.jump;
        let seconds: f64 = sqlx::query_scalar("SELECT greatest(0.0, least(0.25, extract(epoch FROM (clock_timestamp() - last_moved_at))))::float8 FROM players WHERE id = $1")
            .bind(player_id).fetch_one(&mut *transaction).await?;
        let (vertical_speed, jump_x, jump_y, jump_z): (f64, f64, f64, f64) = sqlx::query_as(
            "SELECT jump_vertical_speed, jump_x, jump_y, jump_z FROM players WHERE id = $1",
        )
        .bind(player_id)
        .fetch_one(&mut *transaction)
        .await?;
        let mut flight = Flight {
            airborne: position.airborne,
            vertical_speed,
            horizontal: [jump_x, jump_y, jump_z],
            impact: 0.0,
        };
        let previous = [position.x, position.y, position.z];
        let (point, on_object) =
            terrain.advance(previous, &intent, seconds, &mut flight, position.on_object);
        [position.x, position.y, position.z] = point;
        position.airborne = flight.airborne;
        position.on_object = on_object;
        moving = length(std::array::from_fn(|axis| point[axis] - previous[axis])) > 0.000001;
        position.sequence = sequence.to_string();
        sqlx::query("UPDATE players SET position_x = $2, position_y = $3, position_z = $4, movement_sequence = $5, last_moved_at = clock_timestamp(), movement_airborne = $6, movement_support = $7, jump_vertical_speed = $8, jump_x = $9, jump_y = $10, jump_z = $11 WHERE id = $1")
            .bind(player_id).bind(position.x).bind(position.y).bind(position.z).bind(sequence)
            .bind(position.airborne).bind(on_object).bind(flight.vertical_speed)
            .bind(flight.horizontal[0]).bind(flight.horizontal[1]).bind(flight.horizontal[2])
            .execute(&mut *transaction).await?;
        if takes_off {
            super::survival::jump(&mut transaction, player_id, intent.run).await?;
        }
        // Landing hurts by the speed it comes with: a fall from a high ziggurat leaves a hero at death's door.
        let mut damage = fall_damage(flight.impact);
        if damage > 0 {
            // A tall, tough character (a demigod of a datadisk) is hurt less than a man.
            if let Some((character, body)) = super::story::world::player_body(&state).await {
                let looks: String =
                    sqlx::query_scalar("SELECT character FROM players WHERE id = $1")
                        .bind(player_id)
                        .fetch_one(&mut *transaction)
                        .await?;
                if looks == character {
                    damage = fall_damage_of(
                        flight.impact,
                        body.height_m / super::story::disk::MAN_HEIGHT_M,
                        body.toughness,
                    );
                }
            }
        }
        if damage > 0
            && super::survival::hurt(
                &state,
                &mut transaction,
                player_id,
                damage,
                super::survival::DamageCause::Fall,
            )
            .await?
        {
            let notice = super::survival::obituary(&mut transaction, player_id).await?;
            transaction.commit().await?;
            return Err(players::Error::Obituary(Box::new(notice)));
        }
        // Whoever is in water, moving or not, is drowning until they reach a dry shore.
        if !position.airborne
            && terrain.in_water_for(unit([position.x, position.y, position.z]), intent.scale())
            && super::survival::drown(&state, &mut transaction, player_id, seconds).await?
        {
            let notice = super::survival::obituary(&mut transaction, player_id).await?;
            transaction.commit().await?;
            return Err(players::Error::Obituary(Box::new(notice)));
        }
        if moving
            && !super::survival::activity(&state, &mut transaction, player_id, seconds, intent.run)
                .await?
        {
            let notice = super::survival::obituary(&mut transaction, player_id).await?;
            transaction.commit().await?;
            return Err(players::Error::Obituary(Box::new(notice)));
        }
    }
    let stats = players::stats(&mut transaction, player_id).await?;
    let sitting: bool =
        sqlx::query_scalar("SELECT sitting_object_id IS NOT NULL FROM players WHERE id = $1")
            .bind(player_id)
            .fetch_one(&mut *transaction)
            .await?;
    transaction.commit().await?;
    let swimming = !position.airborne
        && terrain.in_water_for(unit([position.x, position.y, position.z]), intent.scale());
    Ok(Json(Reply {
        position,
        moving,
        stats,
        sitting,
        swimming,
    }))
}

/// How tall the player is next to a man: a datadisk's demigod stands taller than the people of the creator, and the
/// water must be deeper before it reaches their chest. 1 for everybody else.
async fn body_scale(
    state: &AppState,
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    player_id: i64,
) -> Result<f64, players::Error> {
    let Some((character, body)) = super::story::world::player_body(state).await else {
        return Ok(1.0);
    };
    let looks: String = sqlx::query_scalar("SELECT character FROM players WHERE id = $1")
        .bind(player_id)
        .fetch_one(&mut **transaction)
        .await?;
    Ok(if looks == character {
        (body.height_m / super::story::disk::MAN_HEIGHT_M).clamp(0.5, 6.0)
    } else {
        1.0
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terrain::legacy_sample;

    /// The walking terrain reads process-wide statics (landscape patches, wildlife overlays,
    /// farms, bridges and structures). Tests that set or read them would see each other's
    /// worlds when they run in parallel, so every test of this module runs alone.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn assert_capsule_clear(point: [f64; 3], object: &WorldObject) {
        let (transform, mesh) = object_shape(object).unwrap();
        let (body_transform, body) = object_capsule(point, &transform, object.scale_m);
        let hit = contact(&body_transform, &body, &Isometry3::identity(), mesh, 0.0).unwrap();
        assert!(
            hit.map_or(true, |hit| hit.dist * object.scale_m >= -0.00001),
            "capsule penetrates {} at {point:?}: {hit:?}",
            object.model
        );
    }

    fn assert_supported(point: [f64; 3], object: &WorldObject) {
        assert_capsule_clear(point, object);
        let up = unit(point);
        let (time, normal) = capsule_hit(
            point,
            up.map(|value| -value * 0.05),
            std::slice::from_ref(object),
        )
        .unwrap();
        assert!(time < 0.01 && dot(normal, up) >= WALKABLE_NORMAL);
    }

    #[test]
    fn jump_has_a_bounded_apex_and_returns_to_the_ground() {
        let _serial = serial();
        let apex_time = JUMP_SPEED / GRAVITY;
        let (height, velocity) = flight_step(0.0, JUMP_SPEED, apex_time);
        assert!((height - JUMP_SPEED * JUMP_SPEED / (2.0 * GRAVITY)).abs() < 1e-10);
        assert!(height > 2.0 && height < 2.2);
        assert!(velocity.abs() < 1e-10);
        let (height, velocity) = flight_step(height, velocity, apex_time);
        assert!(height.abs() < 1e-10);
        assert!((velocity + JUMP_SPEED).abs() < 1e-10);
    }

    #[test]
    fn jump_lands_on_original_rock_geometry() {
        let _serial = serial();
        let pixels = vec![legacy_sample(144); 16 * 16 * 6];
        let mut terrain = WalkingTerrain {
            size: 16,
            environment: environment::generate(16, &pixels, "42", "test").unwrap(),
            pixels,
            catalog: Vec::new(),
            removed: Default::default(),
        };
        terrain.environment.water_m.fill(environment::DRY);
        terrain.environment.biomes.fill(4);
        let height = terrain.height([1.0, 0.0, 0.0]);
        let object = WorldObject {
            id: "rock".into(),
            model: "nature.rock".into(),
            position: [RADIUS + height, 0.0, 0.0],
            scale_m: 1.0,
            yaw: 0.0,
            collision_radius_m: 1.0,
            biome: 4,
            ground_offset_m: 0.0,
        };
        let floor = object_floor([RADIUS + height + 10.0, 0.0, 0.0], &object).unwrap();
        assert!(floor > RADIUS + height + 0.1);
        let mut point = [floor + 1.0, 0.0, 0.0];
        let mut flight = Flight {
            impact: 0.0,
            airborne: true,
            vertical_speed: -1.0,
            horizontal: [0.0; 3],
        };
        for _ in 0..20 {
            point = terrain.advance_over_objects(
                point,
                0.1,
                &mut flight,
                std::slice::from_ref(&object),
            );
        }
        assert!(!flight.airborne);
        assert!(length(point) >= floor);
        assert_supported(point, &object);
        let previous = point;
        point = terrain.advance_over_objects(point, 0.25, &mut flight, &[object]);
        assert!(length(std::array::from_fn(|axis| point[axis] - previous[axis])) < 0.00001);
    }

    /// A flat test terrain with a model of a staircase of the given riser height standing at
    /// the sphere point `[RADIUS + height, 0, 0]`; model +Z is world +Z, model +Y is up.
    fn staircase(riser: f64, steps: usize) -> (WalkingTerrain, WorldObject, f64) {
        staircase_with(riser, 0.8, steps)
    }

    fn staircase_with(riser: f64, tread: f64, steps: usize) -> (WalkingTerrain, WorldObject, f64) {
        let pixels = vec![legacy_sample(144); 16 * 16 * 6];
        let mut terrain = WalkingTerrain {
            size: 16,
            environment: environment::generate(16, &pixels, "42", "test").unwrap(),
            pixels,
            catalog: Vec::new(),
            removed: Default::default(),
        };
        terrain.environment.water_m.fill(environment::DRY);
        terrain.environment.biomes.fill(4);
        let height = terrain.height([1.0, 0.0, 0.0]);
        let half = 2.0;
        let mut vertices: Vec<[f64; 3]> = Vec::new();
        let mut triangles: Vec<[u32; 3]> = Vec::new();
        let mut quad = |a: [f64; 3], b: [f64; 3], c: [f64; 3], d: [f64; 3]| {
            let base = vertices.len() as u32;
            vertices.extend([a, b, c, d]);
            triangles.extend([[base, base + 1, base + 2], [base, base + 2, base + 3]]);
        };
        for k in 0..steps {
            let (z0, z1) = (k as f64 * tread, (k + 1) as f64 * tread);
            let (y0, y1) = (k as f64 * riser, (k + 1) as f64 * riser);
            quad(
                [-half, y0, z0],
                [half, y0, z0],
                [half, y1, z0],
                [-half, y1, z0],
            );
            quad(
                [-half, y1, z0],
                [half, y1, z0],
                [half, y1, z1],
                [-half, y1, z1],
            );
        }
        let top = steps as f64 * riser;
        let end = steps as f64 * tread;
        quad(
            [-half, top, end],
            [half, top, end],
            [half, top, end + 6.0],
            [-half, top, end + 6.0],
        );
        let model = format!(
            "disk:test:stairs_{steps}_{}",
            (riser * 1000.0) as u32 + (tread * 1e6) as u32
        );
        register_structure(&model, "test", vertices, triangles);
        let object = WorldObject {
            id: "stairs".into(),
            model,
            position: [RADIUS + height, 0.0, 0.0],
            scale_m: 1.0,
            yaw: 0.0,
            collision_radius_m: 8.0,
            biome: 4,
            ground_offset_m: 0.0,
        };
        (terrain, object, RADIUS + height)
    }

    /// Walks along world +Z (or -Z) for the given seconds; returns the final point.
    fn walk_along(
        terrain: &WalkingTerrain,
        object: &WorldObject,
        mut point: [f64; 3],
        flight: &mut Flight,
        direction: f64,
        seconds: f64,
    ) -> [f64; 3] {
        for _ in 0..(seconds / 0.05) as usize {
            flight.horizontal = [0.0, 0.0, direction * WALK_SPEED];
            point = terrain.advance_over_objects(point, 0.05, flight, std::slice::from_ref(object));
        }
        point
    }

    #[test]
    fn a_walker_climbs_stairs_with_low_risers_and_comes_down_again() {
        let _serial = serial();
        let (terrain, object, ground) = staircase(0.3, 10);
        let mut flight = Flight {
            impact: 0.0,
            airborne: false,
            vertical_speed: 0.0,
            horizontal: [0.0; 3],
        };
        let start = [ground, 0.0, -3.0];
        let top = walk_along(&terrain, &object, start, &mut flight, 1.0, 3.5);
        assert!(!flight.airborne, "climbing stairs is walking");
        assert!(top[2] > 8.0, "reached the platform: z = {}", top[2]);
        assert!(
            (top[0] - ground - 3.0).abs() < 0.05,
            "standing on the platform at 3 m: {}",
            top[0] - ground
        );
        let down = walk_along(&terrain, &object, top, &mut flight, -1.0, 5.0);
        assert!(down[2] < -1.0, "walked off the stairs: z = {}", down[2]);
        assert!(
            (length(down) - RADIUS - terrain.surface(unit(down)).unwrap()).abs() < 0.05,
            "back on the ground: {}",
            length(down) - RADIUS - terrain.surface(unit(down)).unwrap()
        );
        assert!(!flight.airborne);
    }

    #[test]
    fn a_walker_climbs_stairs_as_steep_as_those_of_a_ziggurat() {
        let _serial = serial();
        // Risers of 0.3 m and treads of 0.316 m: the 43 degrees of the stairs of the Anu ziggurat.
        let (terrain, object, ground) = staircase_with(0.3, 0.316, 25);
        let mut flight = Flight {
            impact: 0.0,
            airborne: false,
            vertical_speed: 0.0,
            horizontal: [0.0; 3],
        };
        let top = walk_along(
            &terrain,
            &object,
            [ground, 0.0, -3.0],
            &mut flight,
            1.0,
            3.5,
        );
        let top = walk_along(&terrain, &object, top, &mut flight, 0.0, 1.0);
        let above = length(top) - RADIUS - terrain.surface(unit(top)).unwrap();
        assert!(!flight.airborne, "standing on the platform: z = {}", top[2]);
        assert!(
            (above - 7.5).abs() < 0.1,
            "the platform is 7.5 m up: {above}, z = {}",
            top[2]
        );
    }

    /// `GLB=path cargo test real_stairs -- --ignored --nocapture`: walks up the stairs of a real model that faces +Z,
    /// the way the server does (`advance`, in steps of 0.1 s).
    #[test]
    #[ignore = "needs a model file in GLB"]
    fn real_stairs() {
        let _serial = serial();
        let Ok(path) = std::env::var("GLB") else {
            return; // a utility: it needs GLB=path
        };
        let loaded = crate::glb::triangles(&std::fs::read(path).unwrap()).unwrap();
        let (terrain, mut object, ground) = staircase(0.3, 1);
        object.model = "disk:test:real".into();
        register_structure("disk:test:real", "x", loaded.vertices, loaded.triangles);
        object.collision_radius_m = 45.0;
        let start = std::env::var("START")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(40.0);
        let key = format!(
            "{}:{}",
            terrain.environment.seed, terrain.environment.heightmap_sha256
        );
        set_statics(key, vec![object]);
        let mut flight = Flight {
            impact: 0.0,
            airborne: false,
            vertical_speed: 0.0,
            horizontal: [0.0; 3],
        };
        let mut point = [ground, 0.0, start];
        let mut on_object = false;
        let intent = Intent {
            direction: [0.0, 0.0, -1.0],
            sequence: "1".into(),
            jump: false,
            run: false,
            body_scale: 1.0,
        };
        for step in 0..120 {
            (point, on_object) = terrain.advance(point, &intent, 0.1, &mut flight, on_object);
            if step % 3 == 0 {
                println!(
                    "z {:7.3} h {:6.3} air {} on_object {}",
                    point[2],
                    length(point) - ground,
                    flight.airborne,
                    on_object
                );
            }
        }
    }

    #[test]
    fn a_wall_higher_than_a_step_stops_the_walker() {
        let _serial = serial();
        let (terrain, object, ground) = staircase(1.2, 1);
        let mut flight = Flight {
            impact: 0.0,
            airborne: false,
            vertical_speed: 0.0,
            horizontal: [0.0; 3],
        };
        let end = walk_along(
            &terrain,
            &object,
            [ground, 0.0, -3.0],
            &mut flight,
            1.0,
            3.0,
        );
        assert!(end[2] < 0.0, "stopped at the wall: z = {}", end[2]);
        assert!((end[0] - ground).abs() < 0.05);
    }

    #[test]
    fn directional_jump_cannot_restart_in_air_and_returns_to_ground() {
        let _serial = serial();
        let pixels = vec![legacy_sample(144); 16 * 16 * 6];
        let mut terrain = WalkingTerrain {
            size: 16,
            environment: environment::generate(16, &pixels, "42", "test").unwrap(),
            pixels,
            catalog: Vec::new(),
            removed: Default::default(),
        };
        terrain.environment.water_m.fill(environment::DRY);
        terrain.environment.biomes.fill(4);
        let mut point = [RADIUS + terrain.height([1.0, 0.0, 0.0]), 0.0, 0.0];
        let mut flight = Flight::default();
        (point, _) = terrain.advance(
            point,
            &Intent {
                direction: [0.0, 0.0, 1.0],
                jump: true,
                ..Intent::default()
            },
            0.1,
            &mut flight,
            false,
        );
        assert!(flight.airborne && point[2] > 0.39 && point[2] < 0.41);
        let velocity = flight.vertical_speed;
        (point, _) = terrain.advance(
            point,
            &Intent {
                direction: [0.0, 0.0, -1.0],
                jump: true,
                ..Intent::default()
            },
            0.1,
            &mut flight,
            false,
        );
        assert!(flight.vertical_speed < velocity && point[2] > 0.79);
        for _ in 0..20 {
            (point, _) = terrain.advance(point, &Intent::default(), 0.1, &mut flight, false);
        }
        assert!(!flight.airborne);
        assert!((length(point) - RADIUS - terrain.height(unit(point))).abs() < 0.000001);
    }

    #[test]
    fn fall_damage_grows_with_the_landing_speed() {
        let _serial = serial();
        // Jumps and small drops are harmless: a run-jump lands at 8.5 m/s only slightly above the limit.
        assert_eq!(fall_damage(0.0), 0);
        assert_eq!(fall_damage(JUMP_SPEED), 0);
        assert_eq!(fall_damage(SAFE_IMPACT_SPEED), 0);
        let speed = |metres: f64| (2.0 * GRAVITY * metres).sqrt();
        let (low, terrace, tower) = (
            fall_damage(speed(5.0)),
            fall_damage(speed(13.5)),
            fall_damage(speed(20.0)),
        );
        assert!(low > 0 && low < 15, "5 m: {low}");
        assert!(terrace > 40 && terrace < 70, "13.5 m: {terrace}");
        assert!(
            tower > 80 && tower < 100,
            "20 m leaves a hero at death's door: {tower}"
        );
        assert_eq!(fall_damage(speed(25.0)), 100);
        // A demigod of 3 m who endures half again as much: the ziggurat is a bruise-and-a-limp, a tower is not.
        let demigod = |metres: f64| fall_damage_of(speed(metres), 3.0 / 1.8, 1.5);
        assert!(
            demigod(13.5) < terrace / 2,
            "13.5 m: {} vs a man's {terrace}",
            demigod(13.5)
        );
        assert!((20..45).contains(&demigod(20.0)), "20 m: {}", demigod(20.0));
        assert!(
            demigod(60.0) >= 100 && demigod(30.0) < 100,
            "a demigod dies beyond 50 m"
        );
        assert_eq!(
            fall_damage_of(speed(20.0), 1.0, 1.0),
            tower,
            "a man is unchanged"
        );
        assert_eq!(fall_damage(f64::NAN), 0);
        let mut previous = 0;
        for tenth in 0..400 {
            let damage = fall_damage(f64::from(tenth) / 10.0);
            assert!(damage >= previous);
            previous = damage;
        }
    }

    #[test]
    fn a_fall_accelerates_and_lands_with_the_speed_of_its_height() {
        let _serial = serial();
        let pixels = vec![legacy_sample(144); 16 * 16 * 6];
        let mut terrain = WalkingTerrain {
            size: 16,
            environment: environment::generate(16, &pixels, "42", "test").unwrap(),
            pixels,
            catalog: Vec::new(),
            removed: Default::default(),
        };
        terrain.environment.water_m.fill(environment::DRY);
        terrain.environment.biomes.fill(4);
        let ground = RADIUS + terrain.height([1.0, 0.0, 0.0]);
        let mut point = [ground + 20.0, 0.0, 0.0];
        let mut flight = Flight {
            airborne: true,
            ..Flight::default()
        };
        let mut speeds = Vec::new();
        for _ in 0..80 {
            (point, _) = terrain.advance(point, &Intent::default(), 0.1, &mut flight, false);
            if !flight.airborne {
                break;
            }
            speeds.push(flight.vertical_speed);
        }
        assert!(!flight.airborne, "it lands");
        assert!(
            speeds.windows(2).all(|pair| pair[1] < pair[0]),
            "it gets faster every tick: {speeds:?}"
        );
        let expected = (2.0 * GRAVITY * 20.0).sqrt();
        assert!(
            (flight.impact - expected).abs() < 1.5,
            "impact {} m/s, expected about {expected}",
            flight.impact
        );
        let damage = fall_damage(flight.impact);
        assert!((80..100).contains(&damage), "20 m fall: {damage}");
    }

    #[test]
    fn round_obstacles_without_a_mesh_block_sideways_but_not_a_jump() {
        let _serial = serial();
        let pixels = vec![legacy_sample(144); 16 * 16 * 6];
        let mut terrain = WalkingTerrain {
            size: 16,
            environment: environment::generate(16, &pixels, "42", "test").unwrap(),
            pixels,
            catalog: Vec::new(),
            removed: Default::default(),
        };
        terrain.environment.water_m.fill(environment::DRY);
        terrain.environment.biomes.fill(4);
        let start = [RADIUS + terrain.height([1.0, 0.0, 0.0]), 0.0, 0.0];
        let grave = WorldObject {
            id: "grave".into(),
            model: String::new(),
            position: [start[0], 0.0, 1.2],
            scale_m: 1.0,
            yaw: 0.0,
            collision_radius_m: 0.4,
            biome: 4,
            ground_offset_m: 0.0,
        };
        let launch = |direction: [f64; 3]| {
            let mut flight = Flight {
                impact: 0.0,
                airborne: true,
                vertical_speed: JUMP_SPEED,
                horizontal: direction.map(|value| value * WALK_SPEED),
            };
            let point =
                terrain.advance_over_objects(start, 0.2, &mut flight, std::slice::from_ref(&grave));
            (point, flight)
        };
        let (up, flight) = launch([0.0; 3]);
        assert!(
            flight.airborne && length(up) > length(start) + 0.5,
            "a standing jump rises"
        );
        let (toward, _) = launch([0.0, 0.0, 1.0]);
        assert!(
            toward[2] < 1.2 - 0.4 - PLAYER_RADIUS + 0.01,
            "the grave stops a sideways jump at {toward:?}"
        );
        let (away, _) = launch([0.0, 0.0, -1.0]);
        assert!(away[2] < -0.5, "moving away is free");
    }

    #[test]
    fn directional_jump_clears_rock_side_and_lands_on_top() {
        let _serial = serial();
        let pixels = vec![legacy_sample(144); 16 * 16 * 6];
        let mut terrain = WalkingTerrain {
            size: 16,
            environment: environment::generate(16, &pixels, "42", "test").unwrap(),
            pixels,
            catalog: Vec::new(),
            removed: Default::default(),
        };
        terrain.environment.water_m.fill(environment::DRY);
        terrain.environment.biomes.fill(4);
        let height = terrain.height([1.0, 0.0, 0.0]);
        let object_direction = unit([RADIUS + height, 0.0, 5.2]);
        let object = WorldObject {
            id: "rock".into(),
            model: "nature.rock".into(),
            position: object_direction
                .map(|value| value * (RADIUS + terrain.height(object_direction))),
            scale_m: 2.0,
            yaw: 0.0,
            collision_radius_m: 2.0,
            biome: 4,
            ground_offset_m: 0.0,
        };
        let mut point = [RADIUS + height, 0.0, 0.0];
        assert!(!clear_jump_path(
            point,
            object.position,
            std::slice::from_ref(&object)
        ));
        let high = unit(point).map(|value| value * (length(point) + 5.0));
        let high_end = object_direction.map(|value| value * length(high));
        assert!(clear_jump_path(
            high,
            high_end,
            std::slice::from_ref(&object)
        ));
        let mut flight = Flight {
            impact: 0.0,
            airborne: true,
            vertical_speed: JUMP_SPEED,
            horizontal: [0.0, 0.0, WALK_SPEED],
        };
        for _ in 0..40 {
            point = terrain.advance_over_objects(
                point,
                0.05,
                &mut flight,
                std::slice::from_ref(&object),
            );
            if !flight.airborne {
                break;
            }
        }
        assert!(!flight.airborne);
        let support = object_floor(
            unit(point).map(|value| value * (length(point) + 0.05)),
            &object,
        );
        assert!(
            support.is_some(),
            "directional jump must land on the actual rock, not bypass it: {point:?}"
        );
        assert!(length(point) >= support.unwrap());
        assert_supported(point, &object);
    }

    #[test]
    fn descending_capsule_slides_off_steep_rock_without_getting_stuck() {
        let _serial = serial();
        let pixels = vec![legacy_sample(144); 16 * 16 * 6];
        let mut terrain = WalkingTerrain {
            size: 16,
            environment: environment::generate(16, &pixels, "42", "test").unwrap(),
            pixels,
            catalog: Vec::new(),
            removed: Default::default(),
        };
        terrain.environment.water_m.fill(environment::DRY);
        terrain.environment.biomes.fill(4);
        let original_center = [5830295.028572648, 2142795.0373625495, -1419450.942963967];
        let original_point = [5830295.971761411, 2142794.0988089717, -1419451.9966884125];
        let up = unit(original_center);
        let center = up.map(|value| value * (RADIUS + terrain.height(up)));
        let object = WorldObject {
            id: "steep-rock".into(),
            model: "nature.stone_largeD".into(),
            position: center,
            scale_m: 3.0614058257080616,
            yaw: 5.782347146974182,
            collision_radius_m: 1.8062294371677563,
            biome: 4,
            ground_offset_m: 0.0,
        };
        let offset: [f64; 3] =
            std::array::from_fn(|axis| original_point[axis] - original_center[axis]);
        let mut point = std::array::from_fn(|axis| center[axis] + offset[axis] + up[axis] * 2.0);
        let mut flight = Flight {
            impact: 0.0,
            airborne: true,
            vertical_speed: -1.0,
            horizontal: [0.0; 3],
        };
        let sample = |point: [f64; 3], flight: &Flight| {
            serde_json::json!({
                "offset": std::array::from_fn::<_, 3, _>(|axis| point[axis] - center[axis]),
                "airborne": flight.airborne,
                "on_object": !flight.airborne && length(point) > RADIUS + terrain.height(unit(point)) + 0.02,
            })
        };
        let mut trajectory = vec![sample(point, &flight)];
        for _ in 0..40 {
            point = terrain.advance_over_objects(
                point,
                0.05,
                &mut flight,
                std::slice::from_ref(&object),
            );
            assert_capsule_clear(point, &object);
            trajectory.push(sample(point, &flight));
        }
        assert!(!flight.airborne, "a side impact must eventually settle");
        let before = point;
        let outward = unit(std::array::from_fn(|axis| {
            offset[axis] - up[axis] * dot(offset, up)
        }));
        flight.horizontal = outward.map(|value| value * WALK_SPEED);
        point =
            terrain.advance_over_objects(point, 0.25, &mut flight, std::slice::from_ref(&object));
        assert!(
            dot(
                std::array::from_fn(|axis| point[axis] - before[axis]),
                outward
            ) > 0.2,
            "the character must be able to walk away after a side landing: {before:?} -> {point:?}"
        );
        assert_capsule_clear(point, &object);
        trajectory.push(sample(point, &flight));
        point = std::array::from_fn(|axis| center[axis] + offset[axis]);
        flight = Flight {
            impact: 0.0,
            horizontal: outward.map(|value| value * WALK_SPEED),
            ..Flight::default()
        };
        let before = point;
        let mut recovery = vec![sample(point, &flight)];
        for _ in 0..40 {
            point = terrain.advance_over_objects(
                point,
                0.05,
                &mut flight,
                std::slice::from_ref(&object),
            );
            assert_capsule_clear(point, &object);
            recovery.push(sample(point, &flight));
        }
        assert!(!flight.airborne);
        assert!(
            dot(
                std::array::from_fn(|axis| point[axis] - before[axis]),
                outward
            ) > 0.2,
            "persisted overlap must recover without a database reset"
        );
        if let Some(path) = std::env::var_os("ISHTARIA_SLIDE_CAPTURE") {
            let capture = serde_json::json!({"object": object, "trajectory": trajectory, "recovery": recovery});
            std::fs::write(path, serde_json::to_vec_pretty(&capture).unwrap()).unwrap();
        }
    }

    #[test]
    fn running_is_bounded_and_launch_speed_survives_shift_release() {
        let _serial = serial();
        let pixels = vec![legacy_sample(144); 16 * 16 * 6];
        let mut terrain = WalkingTerrain {
            size: 16,
            environment: environment::generate(16, &pixels, "42", "test").unwrap(),
            pixels,
            catalog: Vec::new(),
            removed: Default::default(),
        };
        terrain.environment.water_m.fill(environment::DRY);
        terrain.environment.biomes.fill(4);
        let start = [RADIUS + terrain.height([1.0, 0.0, 0.0]), 0.0, 0.0];
        let walking = Intent {
            direction: [0.0, 0.0, 1.0],
            ..Intent::default()
        };
        let running = Intent {
            direction: walking.direction,
            run: true,
            ..Intent::default()
        };
        let mut flight = Flight::default();
        let (point, _) = terrain.advance(start, &running, 100.0, &mut flight, false);
        assert!((point[2] - 1.5).abs() < 0.00001);
        let (point, _) = terrain.advance(point, &walking, 0.25, &mut flight, false);
        assert!((point[2] - 2.5).abs() < 0.00001);
        let jumping = Intent {
            direction: walking.direction,
            jump: true,
            run: true,
            ..Intent::default()
        };
        let (point, _) = terrain.advance(start, &jumping, 0.1, &mut flight, false);
        assert!(flight.airborne && (point[2] - 0.6).abs() < 0.00001);
        let (point, _) = terrain.advance(point, &walking, 0.1, &mut flight, false);
        assert!(flight.airborne && (point[2] - 1.2).abs() < 0.00001);
        assert!(length(flight.horizontal) <= RUN_SPEED + 1e-8);
        let stationary_jump = Intent {
            jump: true,
            run: true,
            ..Intent::default()
        };
        let mut stationary_flight = Flight::default();
        terrain.advance(start, &stationary_jump, 0.05, &mut stationary_flight, false);
        assert!(stationary_flight.vertical_speed < JUMP_SPEED);
        let trajectories = [false, true].map(|run| {
            let mut flight = Flight::default();
            let mut point = start;
            let mut apex: f64 = 0.0;
            let mut duration = 0.0;
            for step in 0..60 {
                let intent = Intent {
                    direction: walking.direction,
                    jump: step == 0,
                    run: run && step == 0,
                    ..Intent::default()
                };
                (point, _) = terrain.advance(point, &intent, 0.05, &mut flight, false);
                apex = apex.max(length(point) - length(start));
                duration += 0.05;
                if !flight.airborne {
                    break;
                }
            }
            assert!(!flight.airborne);
            (apex, duration, point[2])
        });
        let [walking_jump, running_jump] = trajectories;
        assert!((2.0..2.3).contains(&walking_jump.0));
        assert!((3.5..3.9).contains(&running_jump.0));
        assert!(running_jump.1 > walking_jump.1 * 1.2);
        assert!(running_jump.2 > walking_jump.2 * 1.5);
        terrain.environment.biomes.fill(0);
        // A sea is swum, not run: the swimmer still makes headway.
        assert!(terrain
            .step_at_speed(start, running.direction, 0.25, WALK_SPEED)
            .is_some());
        let object = WorldObject {
            id: "run-wall".into(),
            model: "nature.stone_largeD".into(),
            position: [start[0], 0.0, 1.0],
            scale_m: 3.0,
            yaw: 0.0,
            collision_radius_m: 1.0,
            biome: 4,
            ground_offset_m: 0.0,
        };
        assert!(!clear_path(start, [start[0], 0.0, 1.5], &[object]));
    }

    #[test]
    fn a_bridge_deck_is_ground_over_deep_water() {
        let _serial = serial();
        let pixels = vec![legacy_sample(60); 16 * 16 * 6];
        let environment = environment::generate(16, &pixels, "42", "test").unwrap();
        let terrain = WalkingTerrain {
            size: 16,
            pixels,
            environment,
            catalog: Vec::new(),
            removed: Default::default(),
        };
        let site = [0.6, 0.0, 0.8];
        let at = |x: f64, z: f64| crate::scenery::world_from_local(site, x, z, |_| 0.0, RADIUS).0;
        let (from, to) = (at(-15.0, 0.0), at(15.0, 0.0));
        let sea_floor = terrain.height(at(0.0, 0.0));
        assert!(sea_floor < -100.0, "{sea_floor}");
        set_bridges(vec![BridgeDeck {
            from,
            to,
            half_width_m: 1.5,
            from_height_m: 2.0,
            to_height_m: 2.0,
            sag_m: 1.0,
        }]);
        let middle = terrain.height(at(0.0, 0.0));
        let near_end = terrain.height(at(-14.0, 0.5));
        let beside = terrain.height(at(0.0, 3.0));
        let beyond = terrain.height(at(20.0, 0.0));
        let depth = terrain.water_depth(at(0.0, 0.0));
        set_bridges(Vec::new());
        assert_eq!(
            depth, 0.0,
            "a character on the deck neither swims nor drowns"
        );
        assert!((middle - 1.0).abs() < 0.05, "the deck sags to {middle}");
        assert!((near_end - 1.9).abs() < 0.2, "{near_end}");
        assert!(beside < -100.0 && beyond < -100.0, "off the deck is water");
    }

    #[test]
    fn every_zeroad_model_of_the_catalog_is_used_by_the_generator_or_a_datadisk() {
        let _serial = serial();
        // Overlay-only: water plants and the crocodile need a datadisk's water-hole, and the two flock animals
        // are driven by the shepherd of the Quaternius ones; the Gilgamesh and tutorial overlays place them.
        const OVERLAY_ONLY: [&str; 10] = [
            "zeroad.reed_beach_a",
            "zeroad.reed_beach_b",
            "zeroad.reed_pond_lush",
            "zeroad.reed_giant",
            "zeroad.papyrus_1",
            "zeroad.papyrus_2",
            "zeroad.cattails",
            "zeroad.crocodile",
            "zeroad.sheep",
            "zeroad.goat",
        ];
        let mut spawned = 0;
        for kind in catalog_with_fauna()
            .iter()
            .filter(|kind| kind.id.starts_with("zeroad."))
        {
            let on_farms = FARM_SPECIES.iter().any(|(model, _)| *model == kind.id);
            let overlay_only = OVERLAY_ONLY.contains(&kind.id.as_str());
            assert!(
                !kind.biomes.is_empty() || on_farms || overlay_only,
                "{} is neither spawned by the generator nor on a farm nor in an overlay",
                kind.id
            );
            spawned += usize::from(!kind.biomes.is_empty());
        }
        assert!(spawned >= 50, "{spawned} 0 A.D. models in the biome tables");
    }

    #[test]
    fn walking_is_bounded_and_cannot_fly_or_swim() {
        let _serial = serial();
        let pixels = vec![legacy_sample(144); 16 * 16 * 6];
        let environment = environment::generate(16, &pixels, "42", "test").unwrap();
        let mut terrain = WalkingTerrain {
            size: 16,
            pixels,
            environment,
            catalog: Vec::new(),
            removed: Default::default(),
        };
        terrain.environment.water_m.fill(environment::DRY);
        terrain.environment.biomes.fill(4);
        let height = terrain.height([1.0, 0.0, 0.0]);
        let point = [RADIUS + height, 0.0, 0.0];
        let moved = terrain.step(point, [0.0, 0.0, 1.0], 100.0).unwrap();
        assert!((length(moved) - (RADIUS + terrain.height(unit(moved)))).abs() < 1e-8);
        assert!((length(moved) - length(point)).abs() < 0.7);
        assert!((moved[2] - 1.0).abs() < 1e-6);
        assert!(terrain.step(point, [1.0, 0.0, 0.0], 0.25).is_none());
        assert!(terrain.step(point, [0.0; 3], 0.25).is_none());
    }

    #[test]
    fn a_swimmer_is_slow_and_can_reach_a_dry_shore() {
        let _serial = serial();
        let pixels = vec![legacy_sample(144); 16 * 16 * 6];
        let environment = environment::generate(16, &pixels, "42", "test").unwrap();
        let mut terrain = WalkingTerrain {
            size: 16,
            pixels,
            environment,
            catalog: Vec::new(),
            removed: Default::default(),
        };
        terrain.environment.water_m.fill(environment::DRY);
        terrain.environment.biomes.fill(4);
        let height = terrain.height([1.0, 0.0, 0.0]);
        let point = [RADIUS + height, 0.0, 0.0];
        assert!(!terrain.in_water(unit(point)));
        let walked = terrain.step(point, [0.0, 0.0, 1.0], 0.25).unwrap();
        // A lake: standing water 5 m above the ground everywhere.
        let ground = terrain.environment.elevation_m.clone();
        for (water, elevation) in terrain.environment.water_m.iter_mut().zip(&ground) {
            *water = *elevation + 5;
        }
        assert!(terrain.in_water(unit(point)));
        let swum = terrain.step(point, [0.0, 0.0, 1.0], 0.25).unwrap();
        let walked_metres = length(std::array::from_fn(|axis| walked[axis] - point[axis]));
        let swum_metres = length(std::array::from_fn(|axis| swum[axis] - point[axis]));
        assert!(swum_metres > 0.0 && swum_metres < walked_metres * 0.6);
        // The shore is dry again: a swimmer climbs out.
        terrain.environment.water_m.fill(environment::DRY);
        assert!(terrain.step(swum, [0.0, 0.0, 1.0], 0.25).is_some());
    }

    #[test]
    fn a_struck_herbivore_runs_from_the_hunter_rests_and_walks_back() {
        let _serial = serial();
        let pixels = vec![legacy_sample(145); 16 * 16 * 6];
        let environment = environment::generate(16, &pixels, "42", "test").unwrap();
        let terrain = WalkingTerrain {
            size: 16,
            pixels,
            environment,
            catalog: Vec::new(),
            removed: Default::default(),
        };
        let ground = RADIUS + terrain.height([1.0, 0.0, 0.0]);
        let animal = WorldObject {
            id: "test:flee:gazelle".to_owned(),
            model: "zeroad.gazelle".to_owned(),
            position: [ground, 0.0, 0.0],
            scale_m: 1.0,
            yaw: 0.0,
            collision_radius_m: 0.0,
            biome: 4,
            ground_offset_m: 0.0,
        };
        let now = 1_000_000_000;
        // The hunter stands 3 m to the -z side; the gazelle runs off to +z.
        terrain.start_flee(&animal, [ground, 0.0, -3.0], now);
        let route = terrain.animal_route(&animal, now);
        let distance =
            |a: [f64; 3], b: [f64; 3]| length(std::array::from_fn(|axis| a[axis] - b[axis]));
        let ran = route_position(&route, now + FLEE_RUN_MS);
        assert!(
            (distance(ran, animal.position) - FLEE_DISTANCE_M).abs() < 1.0,
            "ran {}",
            distance(ran, animal.position)
        );
        assert!(ran[2] > 40.0, "away from the hunter: {ran:?}");
        assert_eq!(
            route_position(&route, now + FLEE_RUN_MS + FLEE_REST_MS / 2),
            ran,
            "it rests where it stopped"
        );
        let end = now + FLEE_RUN_MS + FLEE_REST_MS + FLEE_RETURN_MS;
        assert_eq!(
            terrain.animal_route(&animal, end),
            terrain.usual_route(&animal, end),
            "then it wanders as before"
        );
    }

    #[test]
    fn ground_below_sea_level_in_a_dry_cell_is_land_as_the_client_draws_it() {
        let _serial = serial();
        let pixels = vec![legacy_sample(145); 16 * 16 * 6];
        let environment = environment::generate(16, &pixels, "42", "test").unwrap();
        let mut terrain = WalkingTerrain {
            size: 16,
            pixels,
            environment,
            catalog: Vec::new(),
            removed: Default::default(),
        };
        // The strip dips below zero between a water sample and land: the client draws no water there.
        terrain.pixels.fill(31_996); // -1 m
        terrain.environment.biomes.fill(4);
        terrain.environment.water_m.fill(environment::DRY);
        let here = [1.0, 0.0, 0.0];
        assert!(terrain.height(here) < 0.0);
        assert_eq!(
            terrain.water_depth(here),
            0.0,
            "no invisible sea to drown in"
        );
        // An ocean cell is water over its whole area.
        terrain.environment.biomes.fill(0);
        terrain.environment.water_m.fill(0);
        assert!(terrain.water_depth(here) > 0.5);
    }

    #[test]
    fn shallow_water_is_waded_slowly_and_only_deep_water_is_swum() {
        let _serial = serial();
        let pixels = vec![legacy_sample(145); 16 * 16 * 6];
        let environment = environment::generate(16, &pixels, "42", "test").unwrap();
        let mut terrain = WalkingTerrain {
            size: 16,
            pixels,
            environment,
            catalog: Vec::new(),
            removed: Default::default(),
        };
        terrain.environment.biomes.fill(4);
        terrain.environment.water_m.fill(environment::DRY);
        let height = terrain.height([1.0, 0.0, 0.0]);
        let point = [RADIUS + height, 0.0, 0.0];
        let metres = |from: [f64; 3], to: [f64; 3]| {
            length(std::array::from_fn(|axis| to[axis] - from[axis]))
        };
        let dry = terrain.step(point, [0.0, 0.0, 1.0], 0.25).unwrap();
        // The coarse ground lies below the heightmap here: standing water counts only above the heightmap's ground.
        terrain.environment.elevation_m.fill(height as i32 - 100);
        let fill = |terrain: &mut WalkingTerrain, depth_m: f64| {
            terrain
                .environment
                .water_m
                .fill((height + depth_m).round() as i32);
        };
        // Standing water about 60 cm deep: a wade, slower than a walk, not a swim, and no drowning.
        fill(&mut terrain, 0.6);
        let depth = terrain.water_depth(unit(point));
        assert!((0.3..WADE_DEPTH_M).contains(&depth), "depth {depth}");
        assert!(!terrain.in_water(unit(point)));
        let waded = terrain
            .step(point, [0.0, 0.0, 1.0], 0.25)
            .expect("a wader walks on");
        let waded_metres = metres(point, waded);
        assert!(
            waded_metres < metres(point, dry) * 0.7,
            "waded {waded_metres}"
        );
        assert!(
            waded_metres > metres(point, dry) * 0.5,
            "a wade is not a swim: {waded_metres}"
        );
        // Deeper than the chest is swimming, and slower still.
        fill(&mut terrain, 3.0);
        assert!(terrain.in_water(unit(point)));
        let swum = terrain.step(point, [0.0, 0.0, 1.0], 0.25).unwrap();
        assert!(metres(point, swum) < waded_metres);
        // A giant of a datadisk is waist-deep where a man swims: twice as tall, twice as deep before it swims.
        fill(&mut terrain, 2.0);
        assert!(terrain.in_water_for(unit(point), 1.0));
        assert!(!terrain.in_water_for(unit(point), 2.0));
        let giant = terrain
            .step_scaled(point, [0.0, 0.0, 1.0], 0.25, WALK_SPEED, 2.0)
            .expect("a giant wades on");
        assert!(
            metres(point, giant) > waded_metres * 0.9,
            "a wade, not a swim: {}",
            metres(point, giant)
        );
        // A swimmer who reaches shallow water wades on.
        fill(&mut terrain, 0.6);
        assert!(terrain.step(swum, [0.0, 0.0, 1.0], 0.25).is_some());
    }

    #[test]
    fn swept_obstacles_block_tunnelling_but_allow_escape() {
        let _serial = serial();
        let point = [RADIUS, 0.0, 0.0];
        let mut object = WorldObject {
            id: "test".into(),
            model: "rock".into(),
            position: [RADIUS, 0.0, 0.5],
            scale_m: 1.0,
            yaw: 0.0,
            collision_radius_m: 0.1,
            biome: 4,
            ground_offset_m: 0.0,
        };
        assert!(!clear_path(point, [RADIUS, 0.0, 1.0], &[object.clone()]));
        assert!(clear_path(point, [RADIUS, 1.0, 0.0], &[object.clone()]));
        object.position[2] = 0.2;
        assert!(clear_path(point, [RADIUS, 0.0, -1.0], &[object.clone()]));
        assert!(!clear_path(point, [RADIUS, 0.0, 1.0], &[object.clone()]));
        object.collision_radius_m = 0.0;
        assert!(clear_path(point, [RADIUS, 0.0, 1.0], &[object]));
    }

    #[test]
    fn walls_of_places_block_steps_for_the_terrain_they_belong_to() {
        let _serial = serial();
        let pixels = vec![legacy_sample(144); 16 * 16 * 6];
        let mut environment = environment::generate(16, &pixels, "42", "walls").unwrap();
        environment.water_m.fill(environment::DRY);
        environment.biomes.fill(4);
        let terrain = WalkingTerrain {
            size: 16,
            pixels,
            environment,
            catalog: Vec::new(),
            removed: Default::default(),
        };
        let height = terrain.height([1.0, 0.0, 0.0]);
        let origin = [RADIUS + height, 0.0, 0.0];
        let direction = unit(cross(unit(origin), [0.0, 1.0, 0.0]));
        let wall_at = |meters: f64| -> [f64; 3] {
            unit(std::array::from_fn(|axis| {
                origin[axis] + direction[axis] * meters
            }))
            .map(|value| value * (RADIUS + height))
        };
        let wall = WorldObject {
            id: "world:town_00#1.0".into(),
            model: String::new(),
            position: wall_at(1.0),
            scale_m: 1.0,
            yaw: 0.0,
            collision_radius_m: 1.25,
            biome: 4,
            ground_offset_m: 0.0,
        };
        // Without the obstacle (or for another terrain) the step is free.
        set_statics("other:key".into(), vec![wall.clone()]);
        assert!(terrain.step(origin, direction, 0.25).is_some());
        set_statics("42:walls".into(), vec![wall]);
        assert!(
            terrain.step(origin, direction, 0.25).is_none(),
            "the wall blocks the step"
        );
        assert!(
            terrain
                .step(origin, direction.map(|value| -value), 0.25)
                .is_some(),
            "walking away is allowed"
        );
        set_statics(String::new(), Vec::new());
    }

    #[test]
    fn authoritative_objects_are_stable_and_block_actual_steps() {
        let _serial = serial();
        let pixels = vec![legacy_sample(144); 16 * 16 * 6];
        let mut environment = environment::generate(16, &pixels, "42", "test").unwrap();
        environment.water_m.fill(environment::DRY);
        environment.biomes.fill(5);
        let terrain = WalkingTerrain {
            size: 16,
            pixels,
            environment,
            catalog: object_catalog().unwrap(),
            removed: Default::default(),
        };
        let height = terrain.height([1.0, 0.0, 0.0]);
        let point = [RADIUS + height, 0.0, 0.0];
        let objects = terrain.objects(point, 16);
        assert_eq!(objects, terrain.objects(point, 16));
        assert!(objects.len() > 100 && objects.len() <= 1089);
        let obstacle = objects
            .iter()
            .find(|object| object.collision_radius_m > 0.0)
            .unwrap();
        let direction = unit(obstacle.position);
        let tangent = unit(cross(direction, [0.0, 1.0, 0.0]));
        let distance = obstacle.collision_radius_m + PLAYER_RADIUS + 0.1;
        let start = unit(std::array::from_fn(|axis| {
            obstacle.position[axis] - tangent[axis] * distance
        }))
        .map(|value| value * (RADIUS + height));
        assert!(terrain.step(start, tangent, 0.25).is_none());
        assert!(terrain
            .step(start, tangent.map(|value| -value), 0.25)
            .is_some());
        let seam = terrain.objects(
            unit([1.0, 0.0, 1.0]).map(|value| value * (RADIUS + height)),
            16,
        );
        assert!(seam
            .iter()
            .any(|object| object.id.split(':').nth(1) == Some("0")));
        assert!(seam
            .iter()
            .any(|object| object.id.split(':').nth(1) == Some("4")));
        assert_eq!(
            seam.iter()
                .map(|object| &object.id)
                .collect::<HashSet<_>>()
                .len(),
            seam.len()
        );
        // Same catalog model appears at different sizes (about 0.55×–1.45× its base).
        let by_model = |model: &str| -> Vec<f64> {
            objects
                .iter()
                .filter(|object| object.model == model)
                .map(|object| object.scale_m)
                .collect()
        };
        let trees = by_model("nature.oak");
        assert!(
            trees.len() >= 2,
            "need several oaks to compare sizes, got {}",
            trees.len()
        );
        let min = trees.iter().copied().fold(f64::INFINITY, f64::min);
        let max = trees.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        assert!(
            max / min > 1.2,
            "oaks should visibly differ in size: {min} .. {max}"
        );
        assert!((varied_scale(10.0, 0.0) - 5.5).abs() < 1e-12);
        assert!((varied_scale(10.0, 1.0) - 14.5).abs() < 1e-12);
    }

    #[test]
    fn every_nature_variant_can_be_selected_on_authoritative_terrain() {
        let _serial = serial();
        let pixels = vec![legacy_sample(144); 16 * 16 * 6];
        let mut terrain = WalkingTerrain {
            size: 16,
            environment: environment::generate(16, &pixels, "42", "test").unwrap(),
            pixels,
            catalog: object_catalog().unwrap(),
            removed: Default::default(),
        };
        terrain.environment.water_m.fill(environment::DRY);
        let mut selected = HashSet::new();
        for biome in 3..=7 {
            terrain.environment.biomes.fill(biome);
            for row in 300_000..300_150 {
                for column in 300_000..300_150 {
                    if let Some(object) = terrain.object_cell(0, column, row) {
                        assert!(object.collision_radius_m <= 20.0);
                        assert!(
                            (length(object.position)
                                - RADIUS
                                - terrain.height(unit(object.position)))
                            .abs()
                                < 0.000001
                        );
                        selected.insert(object.model);
                    }
                }
            }
        }
        for kind in terrain
            .catalog
            .iter()
            // Reeds grow only in or beside water: see `reeds_grow_in_shallow_river_water`.
            .filter(|kind| kind.id.starts_with("nature.") && !kind.shallow_water)
        {
            assert!(
                selected.contains(&kind.id),
                "unreachable model: {}",
                kind.id
            );
        }
    }

    #[test]
    fn reeds_grow_in_shallow_river_water() {
        let _serial = serial();
        set_patches(String::new(), Vec::new());
        set_wildlife(String::new(), Vec::new());
        let mut terrain = terrain_with(144, 2, object_catalog().unwrap());
        let ground = terrain.height([0.0, 0.0, 1.0]).round() as i32;
        terrain.environment.elevation_m.fill(ground - 1);
        terrain.environment.water_m.fill(ground + 1);
        let mut models = HashSet::new();
        for row in 300_000..300_150 {
            for column in 300_000..300_150 {
                if let Some(object) = terrain.object_cell(0, column, row) {
                    models.insert(object.model);
                }
            }
        }
        assert!(
            models.contains("nature.reed_clump"),
            "river reeds in shallow water: {models:?}"
        );
        assert!(
            models.iter().all(|model| !model.starts_with("zeroad.")),
            "overlay-only reeds must not grow without an overlay: {models:?}"
        );
    }

    fn terrain_with(pixel: u8, biome: u8, catalog: Vec<ObjectKind>) -> WalkingTerrain {
        let pixels = vec![legacy_sample(pixel); 16 * 16 * 6];
        let mut environment = environment::generate(16, &pixels, "42", "test").unwrap();
        environment.biomes.fill(biome);
        if biome < 3 {
            environment.water_m.fill(0);
        } else {
            environment.water_m.fill(environment::DRY);
        }
        WalkingTerrain {
            size: 16,
            pixels,
            environment,
            catalog,
            removed: Default::default(),
        }
    }

    #[test]
    fn water_within_reach_is_fresh_salt_or_absent() {
        let _serial = serial();
        let here = [RADIUS, 0.0, 0.0];
        assert_eq!(
            terrain_with(80, 1, Vec::new()).water_near(here, 6.0),
            WaterNear::Fresh
        );
        assert_eq!(
            terrain_with(80, 2, Vec::new()).water_near(here, 6.0),
            WaterNear::Fresh
        );
        assert_eq!(
            terrain_with(80, 0, Vec::new()).water_near(here, 6.0),
            WaterNear::Salt
        );
        assert_eq!(
            terrain_with(80, 4, Vec::new()).water_near(here, 6.0),
            WaterNear::None
        );
        // Only the cube face of this point holds a lake: the far side of the planet has none.
        let mut terrain = terrain_with(80, 4, Vec::new());
        let size = terrain.environment.face_size;
        let (face, _, _) = coordinates([1.0, 0.0, 0.0]);
        terrain.environment.biomes[face * size * size..(face + 1) * size * size].fill(1);
        assert_eq!(terrain.water_near(here, 6.0), WaterNear::Fresh);
        assert_eq!(
            terrain.water_near([-RADIUS, 0.0, 0.0], 6.0),
            WaterNear::None
        );
        assert_eq!(terrain.water_near([0.0; 3], 6.0), WaterNear::None);
    }

    fn catalog_with_fauna() -> Vec<ObjectKind> {
        let mut catalog = object_catalog().unwrap();
        catalog.extend(fauna_catalog().unwrap());
        catalog
    }

    #[test]
    fn animals_do_not_change_where_trees_and_rocks_stand() {
        let _serial = serial();
        let flora = terrain_with(144, 5, object_catalog().unwrap());
        let both = terrain_with(144, 5, catalog_with_fauna());
        let point = [RADIUS + flora.height([1.0, 0.0, 0.0]), 0.0, 0.0];
        assert!(!flora.objects(point, 16).is_empty());
        assert_eq!(flora.objects(point, 16), both.objects(point, 16));
    }

    #[test]
    fn a_wildlife_overlay_puts_game_on_a_dry_floodplain_in_a_coarse_river_cell() {
        let _serial = serial();
        // The hunter's water-hole: dry steppe the 150 km map marks "river". Without the overlay its cells only
        // hold fish; with it the herds of the epic stand there.
        set_patches(String::new(), Vec::new());
        let mut terrain = terrain_with(144, 2, catalog_with_fauna());
        let ground = terrain.height([0.0, -1.0, 0.0]).floor() as i32;
        terrain.environment.elevation_m.fill(ground);
        terrain.environment.water_m.fill(ground);
        let place = [0.0, -1.0, 0.0];
        let origin = place.map(|value| value * (RADIUS + terrain.height(place)));
        let around = |terrain: &WalkingTerrain| {
            let mut found = Vec::new();
            for step in 0..60 {
                let angle = f64::from(step) * 0.00002;
                let near = unit([angle.sin(), -angle.cos(), 0.0]);
                found.extend(
                    terrain.fauna(near.map(|value| value * (RADIUS + terrain.height(near)))),
                );
            }
            found
        };
        set_wildlife(String::new(), Vec::new());
        assert!(
            around(&terrain)
                .iter()
                .all(|animal| !is_land_animal(&animal.model)),
            "no game without an overlay"
        );
        set_wildlife(
            "test".into(),
            vec![WildlifeZone {
                direction: unit(origin),
                radius_m: 3000.0,
                density: 8.0,
                flora: Vec::new(),
                fauna: vec![("animal.deer".into(), 1.0, false)],
            }],
        );
        let game = around(&terrain);
        set_wildlife(String::new(), Vec::new());
        assert!(
            game.iter().any(|animal| animal.model == "animal.deer"),
            "the overlay's deer stand on the floodplain: {game:?}"
        );
    }

    #[test]
    fn land_animals_are_deterministic_passable_and_fit_their_biome() {
        let _serial = serial();
        let terrain = terrain_with(144, 4, catalog_with_fauna());
        let point = [RADIUS + terrain.height([1.0, 0.0, 0.0]), 0.0, 0.0];
        let mut found = Vec::new();
        // Animals are sparse: look around a few places on the map.
        for step in 0..40 {
            let angle = f64::from(step) * 0.0003;
            let place = unit([angle.cos(), angle.sin(), 0.0]);
            let origin = place.map(|value| value * (RADIUS + terrain.height(place)));
            found.extend(terrain.fauna(origin));
            assert_eq!(terrain.fauna(origin), terrain.fauna(origin));
        }
        assert!(!found.is_empty(), "grassland has animals near {point:?}");
        for animal in &found {
            assert!(animal.id.contains(":fauna:"), "{}", animal.id);
            assert!(is_land_animal(&animal.model), "{}", animal.model);
            assert_eq!(animal.collision_radius_m, 0.0, "animals are passable");
            assert!(
                terrain.object_by_id(&animal.id).is_none(),
                "animals are not harvestable"
            );
        }
        let kinds: std::collections::HashSet<_> =
            found.iter().map(|animal| &animal.model).collect();
        assert!(
            kinds.iter().all(|model| [
                "animal.cow",
                "animal.bull",
                "animal.horse",
                "animal.white_horse",
                "animal.donkey",
                "animal.deer",
                "animal.fox",
                "animal.shiba_inu",
                "animal.alpaca",
                // The 0 A.D. animals of the grassland biome.
                "zeroad.deer",
                "zeroad.fox",
                "zeroad.gazelle",
                "zeroad.lion",
                "zeroad.lioness",
                "zeroad.horse_persian",
                "zeroad.hawk",
                "zeroad.crow"
            ]
            .contains(&model.as_str())),
            "{kinds:?}"
        );
    }

    fn some_animals(terrain: &WalkingTerrain) -> Vec<WorldObject> {
        let mut found = Vec::new();
        for step in 0..40 {
            let angle = f64::from(step) * 0.0003;
            let place = unit([angle.cos(), angle.sin(), 0.0]);
            found
                .extend(terrain.fauna(place.map(|value| value * (RADIUS + terrain.height(place)))));
        }
        found
    }

    #[test]
    fn animals_walk_along_routes_that_depend_only_on_id_and_time() {
        let _serial = serial();
        let terrain = terrain_with(144, 4, catalog_with_fauna());
        let animals = some_animals(&terrain);
        assert!(!animals.is_empty());
        let now = 1_700_000_000_000;
        for animal in &animals {
            let route = terrain.animal_route(animal, now);
            assert_eq!(route, terrain.animal_route(animal, now), "deterministic");
            // The next call, half a minute later, agrees where the routes overlap.
            let later = terrain.animal_route(animal, now + WANDER_SLOT_MS);
            let end = route.last().unwrap()[0] as i64;
            for time in (now + WANDER_SLOT_MS..end).step_by(1500) {
                let first = route_position(&route, time);
                let second = route_position(&later, time);
                assert!(distance(first, second) < 0.05, "{} at {time}", animal.id);
            }
            // It never jumps and strays only a little from where the seed put it.
            let flying = terrain
                .fauna_kind(&animal.model)
                .is_some_and(|kind| kind.flying);
            let stray = if flying {
                FLY_WANDER_M + 20.0
            } else {
                WILD_WANDER_M + 12.0
            };
            let mut previous = route_position(&route, now);
            for time in (now..now + WANDER_SLOTS * WANDER_SLOT_MS).step_by(500) {
                let position = route_position(&route, time);
                assert!(
                    distance(previous, position) < 3.0,
                    "{} at {time}",
                    animal.id
                );
                assert!(
                    distance(position, animal.position) < stray,
                    "{} strays",
                    animal.id
                );
                previous = position;
            }
        }
        let moving = animals.iter().any(|animal| {
            let route = terrain.animal_route(animal, now);
            distance(
                route_position(&route, now),
                route_position(&route, now + 15_000),
            ) > 1.0
        });
        assert!(moving, "some animal walks");
    }

    #[test]
    fn waterhole_animals_drink_then_graze_away_from_the_bank() {
        let _serial = serial();
        set_patches(String::new(), Vec::new());
        set_wildlife(String::new(), Vec::new());
        let terrain = terrain_with(144, 4, catalog_with_fauna());
        let anchor = [1.0, 0.0, 0.0];
        let anchor_height = terrain.height(anchor);
        // Pond in the middle of the patch; dry banks around it for drinking and grazing.
        let mut water = vec![i16::MIN; 32 * 32];
        for row in 12..20 {
            for column in 12..20 {
                water[row * 32 + column] = 10;
            }
        }
        set_patches(
            "42:waterhole".into(),
            vec![TerrainPatch {
                anchor,
                anchor_height_m: anchor_height,
                extent_m: [400.0, 400.0],
                offset_m: [0.0, 0.0],
                feather_m: 40.0,
                width: 32,
                height: 32,
                elevation: vec![0; 32 * 32],
                water: Some(water),
                clear: None,
            }],
        );
        let (axis_x, _) = crate::scenery::client_axes(anchor);
        // Stand on the dry steppe outside the pond (pond is ~100 m across in the patch centre).
        let bank = unit(std::array::from_fn(|axis| {
            anchor[axis] * RADIUS + axis_x[axis] * 90.0
        }));
        set_wildlife(
            "42:waterhole".into(),
            vec![WildlifeZone {
                direction: anchor,
                radius_m: 500.0,
                density: 8.0,
                flora: Vec::new(),
                fauna: vec![("animal.gazelle".into(), 10.0, false)],
            }],
        );
        let animal = WorldObject {
            id: format!("{}:fauna:0:0:0", terrain.environment.seed),
            model: "animal.gazelle".into(),
            position: bank.map(|value| value * (RADIUS + terrain.height(bank))),
            scale_m: 2.09,
            yaw: 0.0,
            collision_radius_m: 0.0,
            biome: 4,
            ground_offset_m: 0.0,
        };
        assert!(
            terrain
                .waterhole_route(&animal, 1_700_000_000_000)
                .is_some(),
            "bank animal joins the drink cycle"
        );
        let now = 1_700_000_000_000_i64;
        let mut saw_near_water = false;
        let mut saw_far_graze = false;
        let mut previous = route_position(&terrain.animal_route(&animal, now), now);
        for time in (now..now + 400_000).step_by(4_000) {
            let route = terrain.animal_route(&animal, time);
            assert_eq!(route, terrain.animal_route(&animal, time), "deterministic");
            let position = route_position(&route, time);
            let direction = unit(position);
            let step = distance(previous, position);
            assert!(step < 12.0, "jump {step} at {time}");
            previous = position;
            if let Some((metres, _)) = nearest_water_bank(direction, WATERHOLE_REACH_M) {
                if metres < 12.0 {
                    saw_near_water = true;
                }
                if metres > WATERHOLE_GRAZE_MIN_M * 0.7 {
                    saw_far_graze = true;
                }
            }
        }
        assert!(saw_near_water, "animal visits the bank to drink");
        assert!(saw_far_graze, "animal grazes away from the water");
        set_wildlife(String::new(), Vec::new());
        set_patches(String::new(), Vec::new());
    }

    #[test]
    fn waterside_overlay_fauna_only_stand_near_water() {
        let _serial = serial();
        set_patches(String::new(), Vec::new());
        let mut terrain = terrain_with(144, 4, catalog_with_fauna());
        let ground = terrain.height([0.0, -1.0, 0.0]).floor() as i32;
        terrain.environment.elevation_m.fill(ground);
        terrain.environment.water_m.fill(ground); // dry coarse map
        let place = [0.0, -1.0, 0.0];
        let origin = place.map(|value| value * (RADIUS + terrain.height(place)));
        set_wildlife(
            "test".into(),
            vec![WildlifeZone {
                direction: unit(origin),
                radius_m: 3000.0,
                density: 8.0,
                flora: Vec::new(),
                fauna: vec![
                    ("animal.deer".into(), 1.0, false),
                    ("zeroad.crocodile".into(), 50.0, true),
                ],
            }],
        );
        let mut far = Vec::new();
        for step in 0..80 {
            let angle = f64::from(step) * 0.00002;
            let near = unit([angle.sin(), -angle.cos(), 0.0]);
            far.extend(terrain.fauna(near.map(|value| value * (RADIUS + terrain.height(near)))));
        }
        set_wildlife(String::new(), Vec::new());
        assert!(
            far.iter().any(|animal| animal.model == "animal.deer"),
            "land game still spawns away from water"
        );
        assert!(
            far.iter().all(|animal| animal.model != "zeroad.crocodile"),
            "waterside crocodile must not stand on dry steppe: {far:?}"
        );
    }

    #[test]
    fn reeds_do_not_stand_on_dry_steppe_far_from_water() {
        let _serial = serial();
        set_patches(String::new(), Vec::new());
        let mut terrain = terrain_with(144, 4, catalog_with_fauna());
        let ground = terrain.height([0.0, -1.0, 0.0]).floor() as i32;
        terrain.environment.elevation_m.fill(ground);
        terrain.environment.water_m.fill(ground);
        let place = [0.0, -1.0, 0.0];
        let origin = place.map(|value| value * (RADIUS + terrain.height(place)));
        set_wildlife(
            "test".into(),
            vec![WildlifeZone {
                direction: unit(origin),
                radius_m: 3000.0,
                density: 8.0,
                flora: vec![
                    ("nature.tree_palmTall".into(), 1.0, false),
                    ("zeroad.cattails".into(), 50.0, true),
                    ("nature.reed_clump".into(), 50.0, false),
                ],
                fauna: Vec::new(),
            }],
        );
        let mut far = Vec::new();
        for step in 0..100 {
            let angle = f64::from(step) * 0.00002;
            let near = unit([angle.sin(), -angle.cos(), 0.0]);
            far.extend(
                terrain.objects(near.map(|value| value * (RADIUS + terrain.height(near))), 2),
            );
        }
        set_wildlife(String::new(), Vec::new());
        assert!(
            far.iter()
                .any(|object| object.model == "nature.tree_palmTall"),
            "shade trees still grow on dry ground"
        );
        assert!(
            far.iter().all(|object| {
                object.model != "zeroad.cattails" && object.model != "nature.reed_clump"
            }),
            "reeds must not stand far from water: {far:?}"
        );
    }

    #[test]
    fn birds_fly_above_the_ground_along_routes() {
        let _serial = serial();
        let terrain = terrain_with(144, 4, catalog_with_fauna());
        let direction = [1.0, 0.0, 0.0];
        let height = terrain.height(direction);
        let hawk = WorldObject {
            id: "test:fauna:bird".into(),
            model: "zeroad.hawk".into(),
            position: direction.map(|value| value * (RADIUS + height + 22.0)),
            scale_m: 0.45,
            yaw: 0.0,
            collision_radius_m: 0.0,
            biome: 4,
            ground_offset_m: 0.0,
        };
        assert!(
            terrain
                .fauna_kind(&hawk.model)
                .is_some_and(|kind| kind.flying),
            "hawk is marked flying in the catalog"
        );
        let now = 1_700_000_000_000;
        let route = terrain.animal_route(&hawk, now);
        assert!(route.len() > 4, "bird has a route");
        for point in &route {
            let position = [point[1], point[2], point[3]];
            let ground = terrain.height(unit(position)).max(0.0);
            let clearance = length(position) - RADIUS - ground;
            assert!(
                clearance > 15.0,
                "hawk flies only {clearance:.1} m up at {:?}",
                point[0]
            );
        }
        let first = route_position(&route, now);
        let later = route_position(&route, now + 15_000);
        assert!(distance(first, later) > 0.5, "hawk moves while flying");
        // Fish stay put; a flying model must not be treated as aquatic.
        let fish = WorldObject {
            id: "test:fauna:fish".into(),
            model: "zeroad.fish_generic".into(),
            position: hawk.position,
            scale_m: 0.35,
            yaw: 0.0,
            collision_radius_m: 0.0,
            biome: 2,
            ground_offset_m: 0.0,
        };
        assert!(terrain
            .fauna_kind(&fish.model)
            .is_some_and(|kind| kind.aquatic && !kind.flying));
    }

    fn distance(first: [f64; 3], second: [f64; 3]) -> f64 {
        length(std::array::from_fn(|axis| first[axis] - second[axis]))
    }

    #[test]
    fn animals_are_found_by_id_where_they_stand_until_butchered() {
        let _serial = serial();
        let terrain = terrain_with(144, 4, catalog_with_fauna());
        let animals = some_animals(&terrain);
        let animal = animals
            .iter()
            .find(|animal| is_land_animal(&animal.model))
            .unwrap();
        let now = 1_700_000_000_000;
        let found = terrain.animal_by_id(&animal.id, now).unwrap();
        assert_eq!(found.model, animal.model);
        let route = terrain.animal_route(animal, now);
        assert_eq!(found.position, route_position(&route, now));
        for bad in [
            "42:fauna:0:1",
            "42:fauna:9:1:1",
            "other:fauna:0:1:1",
            "42:farm:0:nowhere",
            "42:0:1:1",
            "",
        ] {
            assert!(terrain.animal_by_id(bad, now).is_none(), "{bad}");
        }
        terrain.mark_removed(&animal.id, unix_now() + 60, None);
        assert!(terrain.animal_by_id(&animal.id, now).is_none());
        assert!(!terrain
            .fauna(animal.position)
            .iter()
            .any(|other| other.id == animal.id));
    }

    #[test]
    fn farm_animals_graze_beside_a_settlement_without_changing_other_objects() {
        let _serial = serial();
        let terrain = terrain_with(144, 4, catalog_with_fauna());
        let site = FarmSite {
            id: "world:town_00".into(),
            direction: unit([1.0, 0.1, 0.1]),
            radius_m: 55.0,
            pasture: None,
            phase_ms: 0,
        };
        let centre = site
            .direction
            .map(|value| value * (RADIUS + terrain.height(site.direction)));
        let before = terrain.objects(centre, 4);
        let key = format!("42:{}", terrain.environment.heightmap_sha256);
        set_farms(key, vec![site.clone()]);
        let animals = terrain.farm_animals(centre);
        assert_eq!(animals.len(), farm_herd(55.0) + flock_size(55.0));
        assert_eq!(animals, terrain.farm_animals(centre));
        for animal in &animals {
            assert!(animal.id.starts_with("42:farm:"), "{}", animal.id);
            let number: usize = animal.id.split(':').nth(2).unwrap().parse().unwrap();
            if number < farm_herd(55.0) {
                assert!(FARM_SPECIES.iter().any(|(model, _)| *model == animal.model));
            } else {
                assert!(
                    ["animal.sheep", "animal.goat"].contains(&animal.model.as_str()),
                    "{} is not of the flock",
                    animal.model
                );
            }
            let away = distance(animal.position, centre);
            assert!(
                (site.radius_m + 10.0..site.radius_m + 60.0).contains(&away),
                "{} is {away} m away",
                animal.id
            );
            let found = terrain.animal_by_id(&animal.id, 1_700_000_000_000).unwrap();
            assert_eq!(found.model, animal.model);
        }
        assert_eq!(before, terrain.objects(centre, 4));
        assert!(terrain.farm_animals([-RADIUS, 0.0, 0.0]).is_empty());
        let flock: Vec<_> = animals
            .iter()
            .filter(|animal| animal.model == "animal.sheep" || animal.model == "animal.goat")
            .filter(|animal| {
                let number: usize = animal.id.split(':').nth(2).unwrap().parse().unwrap();
                number >= farm_herd(55.0)
            })
            .collect();
        assert_eq!(flock.len(), flock_size(55.0));
        // Across many flocks there are goats among the sheep, and more sheep than goats.
        let models: Vec<_> = (0..40)
            .filter_map(|number| {
                let other = FarmSite {
                    id: format!("world:town_{number:02}"),
                    ..site.clone()
                };
                terrain.farm_animal(&other, farm_herd(55.0))
            })
            .map(|animal| animal.model)
            .collect();
        let goats = models
            .iter()
            .filter(|model| *model == "animal.goat")
            .count();
        assert!(
            goats > 0 && goats < models.len() / 2,
            "{goats} of {}",
            models.len()
        );
        // Without a pasture the flock keeps to its fold at any time of the cycle.
        let fold = terrain.fold_of(&site);
        for animal in &flock {
            for time in [0, 300_000, 600_000] {
                let route = terrain.animal_route(animal, time);
                let here = route_position(&route, time);
                let at_fold = fold.map(|value| value * (RADIUS + terrain.height(fold)));
                assert!(distance(here, at_fold) < 40.0);
            }
        }
        // With one it rests in the fold, is driven out and grazes at the pasture.
        let pasture =
            crate::scenery::world_from_local(fold, crate::herd::DRIVE_M, 0.0, |_| 0.0, RADIUS).0;
        let driven = FarmSite {
            pasture: Some(pasture),
            ..site.clone()
        };
        set_farms(
            format!("42:{}", terrain.environment.heightmap_sha256),
            vec![driven],
        );
        let animal = flock[0];
        let resting = route_position(&terrain.animal_route(animal, 0), 0);
        let grazing = route_position(&terrain.animal_route(animal, 300_000), 300_000);
        let travelled = distance(resting, grazing);
        assert!(
            (crate::herd::DRIVE_M - 40.0..crate::herd::DRIVE_M + 40.0).contains(&travelled),
            "the flock moved {travelled} m"
        );
        // Everyone agrees on the route: it is the same whenever it is asked within the step.
        assert_eq!(
            terrain.animal_route(animal, 300_000),
            terrain.animal_route(animal, 304_999)
        );
        set_farms(String::new(), Vec::new());
    }

    #[test]
    fn fish_swim_below_the_surface_of_deep_water_only() {
        let _serial = serial();
        let sea = terrain_with(80, 0, catalog_with_fauna());
        let mut fish = Vec::new();
        for step in 0..40 {
            let angle = f64::from(step) * 0.0003;
            let place = unit([angle.cos(), angle.sin(), 0.0]);
            fish.extend(sea.fauna(place.map(|value| value * RADIUS)));
        }
        assert!(!fish.is_empty(), "the sea has fish");
        for animal in &fish {
            assert!(animal.model.starts_with("fish."), "{}", animal.model);
            let below = RADIUS - length(animal.position);
            assert!(
                (0.7..=60.5).contains(&below),
                "{} swims {below} m below the surface",
                animal.id
            );
            assert_eq!(animal.collision_radius_m, 0.0);
        }
        // Water above the seabed by less than three metres gets no fish; land gets no fish.
        let shallow = terrain_with(128, 0, catalog_with_fauna());
        let start = [RADIUS, 0.0, 0.0];
        assert!(shallow.fauna(start).is_empty());
        let land = terrain_with(144, 4, catalog_with_fauna());
        assert!(land
            .fauna([RADIUS + 1000.0, 0.0, 0.0])
            .iter()
            .all(|animal| is_land_animal(&animal.model)));
        // Fresh water only holds freshwater fish.
        let lake = terrain_with(80, 1, catalog_with_fauna());
        let mut lake_fish = Vec::new();
        for step in 0..60 {
            let angle = f64::from(step) * 0.0003;
            let place = unit([angle.cos(), angle.sin(), 0.0]);
            lake_fish.extend(lake.fauna(place.map(|value| value * RADIUS)));
        }
        assert!(
            lake_fish.iter().all(|animal| [
                "fish.armored_catfish",
                "fish.betta",
                "fish.blue_goldfish",
                "fish.flower_horn",
                "fish.goldfish",
                "fish.koi",
                "fish.piranha",
                "fish.tetra",
                "zeroad.fish_generic"
            ]
            .contains(&animal.model.as_str())),
            "{:?}",
            lake_fish.iter().map(|a| &a.model).collect::<Vec<_>>()
        );
    }

    #[test]
    fn landscape_patch_shapes_ground_water_and_fades_at_the_edge() {
        let _serial = serial();
        let terrain = terrain_with(144, 4, Vec::new());
        let anchor = [1.0, 0.0, 0.0];
        let (axis_x, axis_z) = crate::scenery::client_axes(anchor);
        let at = |east: f64, north: f64| {
            unit(std::array::from_fn(|axis| {
                anchor[axis] * RADIUS + axis_x[axis] * east + axis_z[axis] * north
            }))
        };
        let anchor_height = terrain.height(anchor);
        let untouched = |direction: [f64; 3]| terrain.height(direction);
        let core = at(0.0, 0.0);
        let margin = at(450.0, 0.0);
        let outside = at(600.0, 0.0);
        let (plain_core, plain_margin, plain_outside) =
            (untouched(core), untouched(margin), untouched(outside));
        // A flat patch of 32 x 32 pixels, five metres above the anchor, water at its centre.
        set_patches(
            "42:test".to_owned(),
            vec![TerrainPatch {
                anchor,
                anchor_height_m: anchor_height,
                extent_m: [1000.0, 1000.0],
                offset_m: [0.0, 0.0],
                feather_m: 100.0,
                width: 32,
                height: 32,
                elevation: vec![50; 32 * 32],
                water: Some(vec![10; 32 * 32]),
                clear: Some(ClearMask {
                    width: 4,
                    height: 4,
                    // the middle two columns of the patch are cleared
                    pixels: (0..16).map(|index| matches!(index % 4, 1 | 2)).collect(),
                }),
            }],
        );
        // A cleared strip is known to the scenery placement, ground outside it and outside the patch is not.
        assert!(patch_cleared(at(100.0, 0.0)));
        assert!(patch_cleared(at(-100.0, 300.0)));
        assert!(!patch_cleared(at(400.0, 0.0)));
        assert!(!patch_cleared(at(900.0, 0.0)));
        let plateau = anchor_height + 5.0;
        assert!((terrain.height(core) - plateau).abs() < 1.0e-9);
        let feathered = terrain.height(margin);
        let blended = plain_margin + (plateau - plain_margin) * 0.5;
        assert!((feathered - blended).abs() < 1.0e-6);
        assert!((terrain.height(outside) - plain_outside).abs() < 1.0e-9);
        // The water mask is fresh water within reach, and only while the patch is set.
        let here = anchor.map(|value| value * (RADIUS + terrain.height(anchor)));
        assert_eq!(terrain.water_near(here, 6.0), WaterNear::Fresh);
        assert_eq!(
            terrain.water_near(outside.map(|value| value * RADIUS), 6.0),
            WaterNear::None
        );
        // Scenery never stands in the river of a patch.
        assert!(terrain.surface(core).is_none());
        set_patches(String::new(), Vec::new());
        assert!((terrain.height(core) - plain_core).abs() < 1.0e-9);
        assert_eq!(terrain.water_near(here, 6.0), WaterNear::None);
    }

    #[test]
    fn dry_landscape_patch_over_coarse_water_is_walkable_and_not_a_drink() {
        let _serial = serial();
        // Pixel 120 decodes below sea level so the bare heightmap is wet; only the patch dries it.
        let mut terrain = terrain_with(120, 0, Vec::new());
        terrain.environment.biomes.fill(0);
        terrain.environment.water_m.fill(0);
        let anchor = [1.0, 0.0, 0.0];
        let anchor_height = 40.0;
        set_patches(
            "42:uruk-sea".to_owned(),
            vec![TerrainPatch {
                anchor,
                anchor_height_m: anchor_height,
                extent_m: [800.0, 800.0],
                offset_m: [0.0, 0.0],
                feather_m: 50.0,
                width: 16,
                height: 16,
                elevation: vec![0; 16 * 16],
                water: Some(vec![i16::MIN; 16 * 16]),
                clear: None,
            }],
        );
        assert!(terrain.surface(anchor).is_some());
        assert!((terrain.surface(anchor).unwrap() - anchor_height).abs() < 1.0e-9);
        let site = terrain.land_site(anchor).unwrap();
        assert_eq!(site.0, 4);
        let here = anchor.map(|value| value * (RADIUS + anchor_height));
        assert_eq!(terrain.water_near(here, 6.0), WaterNear::None);
        // Horizontal walking must not be zeroed: surface stays dry under the patch.
        let step = terrain
            .step_at_speed(here, [0.0, 0.0, 1.0], 0.1, 4.0)
            .expect("walk on dry patch over coarse sea");
        assert!(length(std::array::from_fn(|axis| step[axis] - here[axis])) > 0.1);
        set_patches(String::new(), Vec::new());
        assert!(terrain.surface(anchor).is_none());
    }

    #[test]
    fn dry_floodplain_in_a_coarse_river_cell_is_walkable() {
        let _serial = serial();
        // Early Dynastic Mesopotamia: the heightmap is +30 m dry land, the 150 km cell is
        // marked river by drainage. Without this, spawn at the hunter water-hole only jumps.
        set_patches(String::new(), Vec::new());
        let mut terrain = terrain_with(144, 2, Vec::new());
        let height = terrain.height([1.0, 0.0, 0.0]);
        assert!(height > 0.0);
        // Flow rivers set water_m to the fill level; on a flat plain that equals the ground.
        let ground = height.floor() as i32;
        terrain.environment.elevation_m.fill(ground);
        terrain.environment.water_m.fill(ground);
        let anchor = [1.0, 0.0, 0.0];
        assert!(terrain.surface(anchor).is_some());
        assert_eq!(terrain.land_site(anchor).unwrap().0, 4);
        let here = anchor.map(|value| value * (RADIUS + height));
        let step = terrain
            .step_at_speed(here, [0.0, 0.0, 1.0], 0.1, 4.0)
            .expect("walk on dry floodplain inside a coarse river cell");
        assert!(length(std::array::from_fn(|axis| step[axis] - here[axis])) > 0.1);
        // Real ponding above the coarse ground drowns the sample.
        terrain.environment.water_m.fill(ground + 40);
        assert!(terrain.surface(anchor).is_none());
    }

    #[test]
    fn dry_heightmap_in_a_coarse_ocean_cell_is_walkable() {
        let _serial = serial();
        // Gilgamesh earth strip: Uruk's pin is land (+30 m) inside a drowned coarse cell.
        set_patches(String::new(), Vec::new());
        let mut terrain = terrain_with(144, 0, Vec::new());
        terrain.environment.biomes.fill(0);
        terrain.environment.water_m.fill(0);
        let anchor = [1.0, 0.0, 0.0];
        assert!(terrain.height(anchor) > 0.0);
        assert!(terrain.surface(anchor).is_some());
        let here = anchor.map(|value| value * (RADIUS + terrain.height(anchor)));
        let step = terrain
            .step_at_speed(here, [0.0, 0.0, 1.0], 0.1, 4.0)
            .expect("walk on dry heightmap inside a coarse ocean cell");
        assert!(length(std::array::from_fn(|axis| step[axis] - here[axis])) > 0.1);
    }

    #[test]
    fn live_gilgamesh_hunter_spawn_is_walkable() {
        let _serial = serial();
        set_patches(String::new(), Vec::new());
        let Ok(pixels) = std::fs::read("target/gilgamesh_pixels.bin") else {
            return;
        };
        let pixels = crate::terrain::samples_from_bytes(&pixels);
        let seed = "2235404937";
        let sha = "eb2707db9004b69ee7ff7279ec56191570d4dc66c99f77941f6657ba35419fed";
        let environment = environment::generate(256, &pixels, seed, sha).unwrap();
        let terrain = WalkingTerrain {
            size: 256,
            pixels,
            environment,
            catalog: Vec::new(),
            removed: Default::default(),
        };
        let pos = [3800062.780581708, 3322231.004257387, 3887458.1523896814];
        let anchor = unit(pos);
        let height = terrain.height(anchor);
        assert!(height > 0.0, "height {height}");
        assert!(
            terrain.surface(anchor).is_some(),
            "surface none at hunter spawn"
        );
        let here = anchor.map(|value| value * (RADIUS + height));
        let step = terrain
            .step_at_speed(here, [0.0, 0.0, 1.0], 0.15, 4.0)
            .expect("walk at hunter spawn");
        assert!(length(std::array::from_fn(|axis| step[axis] - here[axis])) > 0.1);
    }
}
