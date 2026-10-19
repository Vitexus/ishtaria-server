//! Deterministic placement of datadisk places on a planet.
//!
//! Candidates come from a hash of `(seed, place id, attempt)`, so the same seed, map and
//! disks always give the same sites. Results are persisted by the caller: once a place has
//! a site it never moves.

use super::{disk::{BIOMES, Requires}, Anchor, Story};
use crate::movement::{cross, direction_at, dot, unit, WalkingTerrain, RADIUS};
use anyhow::{bail, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

const FREE_ATTEMPTS: u32 = 200_000;
const RELATIVE_ATTEMPTS: u32 = 5_000;
const LOCATE_ATTEMPTS: u32 = 20_000;
const DEFAULT_RADIUS_M: f64 = 20.0;
const DEFAULT_MAX_SLOPE_DEG: f64 = 20.0;
const SLOPE_PROBE_M: f64 = 40.0;
/// Where a place is allowed to stand inside its parent (fraction of the parent radius).
const PARENT_FILL: f64 = 0.8;

/// What placement needs to know about the ground.
pub trait Ground {
    /// Biome number and height of dry land in a direction; `None` over water.
    fn land_site(&self, direction: [f64; 3]) -> Option<(u8, f64)>;
    /// Ground height in a direction.
    fn height(&self, direction: [f64; 3]) -> f64;
    /// Open sea in a direction.
    fn is_sea(&self, direction: [f64; 3]) -> bool;
    /// Whether a character walking the straight line from one direction to another would run into a
    /// wall, fence, tree or rock. Walking characters of a datadisk do not make such walks.
    fn blocked(&self, _from: [f64; 3], _to: [f64; 3]) -> bool {
        false
    }
}

impl Ground for WalkingTerrain {
    fn land_site(&self, direction: [f64; 3]) -> Option<(u8, f64)> {
        WalkingTerrain::land_site(self, direction)
    }
    fn height(&self, direction: [f64; 3]) -> f64 {
        self.ground_height(direction)
    }
    fn is_sea(&self, direction: [f64; 3]) -> bool {
        WalkingTerrain::is_sea(self, direction)
    }
    fn blocked(&self, from: [f64; 3], to: [f64; 3]) -> bool {
        self.walk_blocked(from, to)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Placed {
    /// Qualified place id, `<disk>:<place>`.
    pub id: String,
    /// Unit vector from the planet centre.
    pub direction: [f64; 3],
    pub height_m: f64,
}

fn samples(seed: &str, id: &str, attempt: u32) -> [f64; 8] {
    let hash = Sha256::digest(format!("ishtaria-story:{seed}:{id}:{attempt}").as_bytes());
    std::array::from_fn(|index| {
        f64::from(u32::from_le_bytes(
            hash[index * 4..index * 4 + 4].try_into().unwrap(),
        )) / 4_294_967_296.0
    })
}

/// Great-circle distance in metres.
pub fn distance_m(first: [f64; 3], second: [f64; 3]) -> f64 {
    RADIUS * dot(first, second).clamp(-1.0, 1.0).acos()
}

/// The point `meters` away from `from` in the compass direction `azimuth` (radians).
pub fn offset(from: [f64; 3], meters: f64, azimuth: f64) -> [f64; 3] {
    let reference = if from[1].abs() < 0.9 {
        [0.0, 1.0, 0.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    let east = unit(cross(from, reference));
    let north = cross(from, east);
    let angle = meters / RADIUS;
    let tangent: [f64; 3] =
        std::array::from_fn(|axis| azimuth.cos() * east[axis] + azimuth.sin() * north[axis]);
    unit(std::array::from_fn(|axis| {
        from[axis] * angle.cos() + tangent[axis] * angle.sin()
    }))
}

/// The point `meters` away from `site` in the direction `bearing_deg` of the map of the place: the frame of its
/// landscape patch and buildings, in which 0 is up the map (north) and 90 is east. Along a great circle.
pub fn offset_local(site: [f64; 3], meters: f64, bearing_deg: f64) -> [f64; 3] {
    let (axis_x, axis_z) = crate::scenery::client_axes(site);
    let bearing = bearing_deg.to_radians();
    let tangent: [f64; 3] =
        std::array::from_fn(|axis| axis_x[axis] * bearing.sin() - axis_z[axis] * bearing.cos());
    let angle = meters / RADIUS;
    unit(std::array::from_fn(|axis| {
        site[axis] * angle.cos() + tangent[axis] * angle.sin()
    }))
}

/// How far from a coastal place the sea may start.
pub const COAST_M: f64 = 300.0;

/// The nearest sea around `direction` within `max_m`: the heading in the site's local plane
/// (unit `(x, z)`) and the distance to the first water.
pub fn sea_toward(
    ground: &dyn Ground,
    direction: [f64; 3],
    max_m: f64,
) -> Option<((f64, f64), f64)> {
    let mut best: Option<((f64, f64), f64)> = None;
    for step in 0..16 {
        let angle = f64::from(step) * std::f64::consts::TAU / 16.0;
        let heading = (angle.cos(), angle.sin());
        let mut meters = 15.0;
        while meters <= max_m {
            let (probe, _) = crate::scenery::world_from_local(
                direction,
                heading.0 * meters,
                heading.1 * meters,
                |_| 0.0,
                RADIUS,
            );
            if ground.is_sea(probe) {
                if best.is_none_or_closer(meters) {
                    best = Some((heading, meters));
                }
                break;
            }
            meters += 15.0;
        }
    }
    best
}

/// A cheap test whether the sea lies within `max_m`: a few probes in eight directions.
pub fn sea_near(ground: &dyn Ground, direction: [f64; 3], max_m: f64) -> bool {
    (0..8).any(|step| {
        let angle = f64::from(step) * std::f64::consts::TAU / 8.0;
        [max_m * 0.5, max_m].iter().any(|meters| {
            let (probe, _) = crate::scenery::world_from_local(
                direction,
                angle.cos() * meters,
                angle.sin() * meters,
                |_| 0.0,
                RADIUS,
            );
            ground.is_sea(probe)
        })
    })
}

trait Closer {
    fn is_none_or_closer(&self, meters: f64) -> bool;
}

impl Closer for Option<((f64, f64), f64)> {
    fn is_none_or_closer(&self, meters: f64) -> bool {
        self.as_ref().map_or(true, |(_, best)| meters < *best)
    }
}

fn radius_of(anchor: &Anchor) -> f64 {
    anchor.place.radius_m.unwrap_or(DEFAULT_RADIUS_M)
}

fn slope_deg(ground: &dyn Ground, direction: [f64; 3], height: f64) -> f64 {
    let steepest = (0..4)
        .map(|quarter| {
            let probe = offset(
                direction,
                SLOPE_PROBE_M,
                f64::from(quarter) * std::f64::consts::FRAC_PI_2,
            );
            (ground.height(probe) - height).abs()
        })
        .fold(0.0, f64::max);
    (steepest / SLOPE_PROBE_M).atan().to_degrees()
}

fn is_ancestor(story: &Story, descendant: &Anchor, target: &str) -> bool {
    let mut current = descendant;
    while let Some(parent) = current.place.parent.as_deref() {
        if parent == target {
            return true;
        }
        match story.anchors.iter().find(|anchor| anchor.id == parent) {
            Some(next) => current = next,
            None => return false,
        }
    }
    false
}

fn related(story: &Story, first: &Anchor, second: &Anchor) -> bool {
    is_ancestor(story, first, &second.id) || is_ancestor(story, second, &first.id)
}

/// Places every anchor of `story` that is not in `existing`, in dependency order. Returns
/// only the new sites. Fails, naming the place, when no site satisfies its requirements.
pub fn place_anchors(
    story: &Story,
    ground: &dyn Ground,
    seed: &str,
    existing: &[Placed],
) -> Result<Vec<Placed>> {
    let mut placed: HashMap<String, Placed> = existing
        .iter()
        .map(|site| (site.id.clone(), site.clone()))
        .collect();
    let mut fresh = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut remaining: Vec<&Anchor> = story
        .anchors
        .iter()
        .filter(|a| !placed.contains_key(&a.id))
        .collect();
    while !remaining.is_empty() {
        let position = remaining
            .iter()
            .position(|anchor| {
                anchor
                    .place
                    .parent
                    .iter()
                    .map(String::as_str)
                    .chain(anchor.place.requires.near.iter().map(|n| n.place.as_str()))
                    .chain(anchor.place.locate.iter().map(|l| l.from.as_str()))
                    .all(|dependency| placed.contains_key(dependency))
            })
            .ok_or_else(|| anyhow::anyhow!("places depend on each other in a cycle"))?;
        let anchor = remaining.remove(position);
        let site = match find_site(story, anchor, ground, seed, &placed) {
            Ok(site) => site,
            // A generated settlement that finds no room is simply left out, with what depends on it.
            Err(error) if anchor.id.starts_with("world:") => {
                eprintln!("{error:#}");
                skipped.push(anchor.id.clone());
                remaining.retain(|other| {
                    !other
                        .place
                        .parent
                        .iter()
                        .map(String::as_str)
                        .chain(other.place.requires.near.iter().map(|n| n.place.as_str()))
                        .chain(other.place.locate.iter().map(|l| l.from.as_str()))
                        .any(|dependency| skipped.iter().any(|gone| gone == dependency))
                });
                continue;
            }
            Err(error) => return Err(error),
        };
        placed.insert(site.id.clone(), site.clone());
        fresh.push(site);
    }
    Ok(fresh)
}

/// What the `requires` of a place say about a site the story fixes anyway (a historical pin or a
/// surveyed point): advice for the author, logged for the operator, never a constraint to move the
/// place away from where history or the survey put it.
fn requires_notes(requires: &Requires, ground: &dyn Ground, direction: [f64; 3]) -> Vec<String> {
    let allowed: Vec<u8> = requires
        .biome
        .iter()
        .filter_map(|name| {
            BIOMES
                .iter()
                .find(|(known, _)| known == name)
                .map(|(_, number)| *number)
        })
        .collect();
    match ground.land_site(direction) {
        None => vec!["the site is under water or sea".to_owned()],
        Some((biome, height)) => {
            let mut reasons = Vec::new();
            if !allowed.is_empty() && !allowed.contains(&biome) {
                reasons.push(format!("its biome is {}", BIOMES[biome as usize].0));
            }
            if requires.height_m.as_ref().is_some_and(|range| {
                range.min.is_some_and(|min| height < min) || range.max.is_some_and(|max| height > max)
            }) {
                reasons.push(format!("its height is {height:.0} m"));
            }
            reasons
        }
    }
}

/// A place pinned to its historical site on the disk's real planet stands exactly there: history
/// has already decided, so the `requires` of the place are only advice for its author. The
/// ground around the site is the disk's business (a landscape patch of the place carries the
/// real relief), so violations of `requires` are logged, not searched away.
fn historical_site(anchor: &Anchor, ground: &dyn Ground) -> Option<Placed> {
    let geo = anchor.place.geo.as_ref()?;
    let direction = geo.direction();
    let height_m = ground.height(direction);
    let reasons = requires_notes(&anchor.place.requires, ground, direction);
    if !reasons.is_empty() {
        eprintln!(
            "place {} stands at its historical site ({} N, {} E) although {}",
            anchor.id,
            geo.latitude_deg,
            geo.longitude_deg,
            reasons.join(" and ")
        );
    }
    Some(Placed {
        id: anchor.id.clone(),
        direction,
        height_m,
    })
}

/// A place located with a zero tolerance stands exactly at its surveyed point: the survey
/// decided, so its `requires` are advice for the author, logged like a historical pin's.
fn surveyed_site(
    anchor: &Anchor,
    ground: &dyn Ground,
    placed: &HashMap<String, Placed>,
) -> Result<Option<Placed>> {
    let Some(locate) = &anchor.place.locate else {
        return Ok(None);
    };
    if locate.tolerance() != 0.0 {
        return Ok(None);
    }
    let from = placed.get(&locate.from).ok_or_else(|| {
        anyhow::anyhow!(
            "place {} is located from {} before it stands",
            anchor.id,
            locate.from
        )
    })?;
    let direction = offset_local(from.direction, locate.distance_m, locate.bearing_deg);
    let height_m = ground.height(direction);
    let reasons = requires_notes(&anchor.place.requires, ground, direction);
    if !reasons.is_empty() {
        eprintln!(
            "place {} stands at its surveyed point ({} m at bearing {} from {}) although {}",
            anchor.id,
            locate.distance_m,
            locate.bearing_deg,
            locate.from,
            reasons.join(" and ")
        );
    }
    Ok(Some(Placed {
        id: anchor.id.clone(),
        direction,
        height_m,
    }))
}

/// A place pinned to a spot of its parent's walled town (`at`) stands exactly there.
fn pinned_site(
    story: &Story,
    anchor: &Anchor,
    ground: &dyn Ground,
    placed: &HashMap<String, Placed>,
) -> Result<Option<Placed>> {
    let (Some(spot), Some(parent)) = (&anchor.place.at, &anchor.place.parent) else {
        return Ok(None);
    };
    let town = story.anchors.iter().find(|a| &a.id == parent);
    let size = town
        .and_then(|a| a.place.scenery.as_ref())
        .filter(|scenery| scenery.preset == "town")
        .and_then(|scenery| scenery.size.as_deref())
        .and_then(crate::scenery::Size::parse);
    let (x, z) = size
        .and_then(|size| crate::scenery::town_spot(size, spot))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "place {} is pinned to {spot}, which the parent {parent} (a town of size town) does not have",
                anchor.id
            )
        })?;
    let (direction, _) =
        crate::scenery::world_from_local(placed[parent].direction, x, z, |_| 0.0, RADIUS);
    Ok(Some(Placed {
        id: anchor.id.clone(),
        direction,
        height_m: ground.height(direction),
    }))
}

fn find_site(
    story: &Story,
    anchor: &Anchor,
    ground: &dyn Ground,
    seed: &str,
    placed: &HashMap<String, Placed>,
) -> Result<Placed> {
    if let Some(site) = historical_site(anchor, ground) {
        return Ok(site);
    }
    if let Some(site) = surveyed_site(anchor, ground, placed)? {
        return Ok(site);
    }
    if let Some(site) = pinned_site(story, anchor, ground, placed)? {
        return Ok(site);
    }
    let requires = &anchor.place.requires;
    let allowed: Vec<u8> = requires
        .biome
        .iter()
        .filter_map(|name| {
            BIOMES
                .iter()
                .find(|(known, _)| known == name)
                .map(|(_, number)| *number)
        })
        .collect();
    let center = anchor
        .place
        .parent
        .as_ref()
        .map(|parent| {
            (
                parent,
                0.0,
                radius_of(
                    story
                        .anchors
                        .iter()
                        .find(|a| &a.id == parent)
                        .expect("checked at compose"),
                ) * PARENT_FILL,
            )
        })
        .or_else(|| {
            requires
                .near
                .first()
                .map(|near| (&near.place, near.min_m, near.max_m))
        });
    let attempts = if anchor.place.locate.is_some() {
        // The search spreads out from the historical point: many tries make the nearest fit close to it.
        LOCATE_ATTEMPTS
    } else if center.is_some() {
        RELATIVE_ATTEMPTS
    } else {
        FREE_ATTEMPTS
    };
    // A located place widens its search twice (four and sixteen times the tolerance) before it gives up.
    let stages: u32 = if anchor.place.locate.is_some() { 3 } else { 1 };
    for step in 0..attempts * stages {
        let (attempt, stage) = (step % attempts, step / attempts);
        let sample = samples(seed, &anchor.id, attempt);
        let direction = match (&anchor.place.locate, center) {
            // The nearest site that does is the one: the search widens from the historical point outwards.
            (Some(locate), _) => {
                let target = offset_local(
                    placed[&locate.from].direction,
                    locate.distance_m,
                    locate.bearing_deg,
                );
                let reach = locate.tolerance()
                    * 4f64.powi(stage as i32)
                    * ((f64::from(attempt) + 1.0) / f64::from(attempts)).sqrt();
                offset(target, reach, sample[4] * std::f64::consts::TAU)
            }
            (None, center) => match center {
                Some((origin, minimum, maximum)) => {
                    let meters = (minimum * minimum
                        + sample[3] * (maximum * maximum - minimum * minimum))
                        .sqrt();
                    offset(
                        placed[origin].direction,
                        meters,
                        sample[4] * std::f64::consts::TAU,
                    )
                }
                None => direction_at((sample[0] * 6.0) as usize % 6, sample[1], sample[2]),
            },
        };
        let Some((biome, height)) = ground.land_site(direction) else {
            continue;
        };
        if !allowed.is_empty() && !allowed.contains(&biome) {
            continue;
        }
        if let Some(range) = &requires.height_m {
            if range.min.is_some_and(|min| height < min)
                || range.max.is_some_and(|max| height > max)
            {
                continue;
            }
        }
        if requires.near.iter().any(|near| {
            let meters = distance_m(direction, placed[&near.place].direction);
            meters < near.min_m || meters > near.max_m
        }) {
            continue;
        }
        match requires.coast {
            Some(true) if !sea_near(ground, direction, COAST_M) => continue,
            Some(false) if sea_near(ground, direction, COAST_M * 2.0) => continue,
            _ => {}
        }
        if let Some(spacing) = anchor.place.spacing_m {
            let crowded = story.anchors.iter().any(|other| {
                other.id != anchor.id
                    && other.place.kind == anchor.place.kind
                    && placed
                        .get(&other.id)
                        .is_some_and(|site| distance_m(direction, site.direction) < spacing)
            });
            if crowded {
                continue;
            }
        }
        let separated = story.anchors.iter().all(|other| {
            other.id == anchor.id
                || related(story, anchor, other)
                || placed.get(&other.id).map_or(true, |site| {
                    distance_m(direction, site.direction) >= radius_of(anchor) + radius_of(other)
                })
        });
        if !separated {
            continue;
        }
        if slope_deg(ground, direction, height)
            > requires.max_slope_deg.unwrap_or(DEFAULT_MAX_SLOPE_DEG)
        {
            continue;
        }
        if let (Some(locate), true) = (&anchor.place.locate, stage > 0) {
            let target = offset_local(
                placed[&locate.from].direction,
                locate.distance_m,
                locate.bearing_deg,
            );
            eprintln!(
                "place {} lies {:.0} km from its historical point (no fitting site within {:.0} km)",
                anchor.id,
                distance_m(direction, target) / 1000.0,
                locate.tolerance() * 4f64.powi(stage as i32 - 1) / 1000.0
            );
        }
        return Ok(Placed {
            id: anchor.id.clone(),
            direction,
            height_m: height,
        });
    }
    bail!(
        "no site found for place {} after {attempts} attempts",
        anchor.id
    )
}
