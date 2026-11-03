//! Characters of a datadisk that walk (`movement` of an NPC).
//!
//! Like the animals of the world they have no state: where a character is depends only on its id, its place
//! and the time, so every server process and every request agrees and nothing is stored. The server sends
//! a route of waypoints `[unix milliseconds, x, y, z]` that covers the next minute and a half; clients
//! walk the character along it and ask again from time to time.
//!
//! * `wander`: the character walks to a spot near its home (on land, within `radius_m`), stands for `pause_s`,
//!   walks on to the next spot, and so on. A new spot every time slot, the slot being long enough for the
//!   longest walk and the pause.
//! * Walks that would run into a wall, fence, tree or rock are not made (`Ground::blocked`): a wanderer goes by its home,
//!   a patrol by its place.
//! * `herd`: a shepherd of a generated settlement walks behind its flock, which rests in the fold, is driven to the
//!   pasture, grazes and is driven home (`crate::herd`, the animals follow the same cycle).
//! * `patrol`: the character walks the loop of `waypoints` (metres east and south of its place, as for
//!   buildings), standing `pause_s` at each; the start of the loop depends on the id so that several
//!   characters on one beat do not walk in step.
//! * `travel`: like a patrol, but the waypoints follow a long footpath between towns (generated
//!   wayfarers only).

use super::disk::{Movement, MovementKind};
use super::placement::{offset, Ground};
use crate::movement::{dot, unit, RADIUS};
use sha2::{Digest, Sha256};

/// A route covers at least this long from now.
const COVER_MS: i64 = 90_000;
/// The most waypoints a route has (clients accept 40).
const MAX_POINTS: usize = 36;
/// Waypoints along a walk are at most this far apart.
const STEP_M: f64 = 18.0;

/// The walks of wanderers by (character, slot, slot length, radius and speed), so that the routes asked for again and
/// again are not worked out again. Home and ground are fixed for a character of a world.
type SlotKey = (String, i64, i64, u64, u64, usize, [u64; 3]);
static SLOT_WALKS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<SlotKey, Vec<[f64; 4]>>>,
> = std::sync::LazyLock::new(Default::default);
/// The cache is emptied when it holds this many slots.
const MAX_CACHED_SLOTS: usize = 50_000;

fn word(hash: &[u8], index: usize) -> f64 {
    f64::from(u32::from_le_bytes(
        hash[index * 4..index * 4 + 4].try_into().unwrap(),
    )) / 4_294_967_296.0
}

fn arc_m(first: [f64; 3], second: [f64; 3]) -> f64 {
    RADIUS * dot(first, second).clamp(-1.0, 1.0).acos()
}

/// A waypoint `[time, x, y, z]` of the character standing at `direction` at `time_ms`.
fn entry(ground: &dyn Ground, time_ms: i64, direction: [f64; 3]) -> [f64; 4] {
    let metres = RADIUS + ground.height(direction).max(0.0);
    let round = |value: f64| (value * 100.0).round() / 100.0;
    let point = direction.map(|value| round(value * metres));
    [time_ms as f64, point[0], point[1], point[2]]
}

/// Appends the walk from `from` to `to` that starts at `start_ms` and the standing after it up to `end_ms`.
fn leg(
    route: &mut Vec<[f64; 4]>,
    ground: &dyn Ground,
    from: [f64; 3],
    to: [f64; 3],
    start_ms: i64,
    end_ms: i64,
    speed: f64,
) {
    let distance = arc_m(from, to);
    // Along a road the pace is half as fast again (both ends and the middle of the stretch are on it).
    let middle = unit(std::array::from_fn(|axis| (from[axis] + to[axis]) / 2.0));
    let speed = if ground.on_trail(from) && ground.on_trail(middle) && ground.on_trail(to) {
        speed * crate::movement::ROAD_PACE
    } else {
        speed
    };
    let move_ms = ((distance / speed) * 1000.0).round() as i64;
    let move_ms = move_ms.min(end_ms - start_ms).max(0);
    let parts = (distance / STEP_M).ceil().clamp(1.0, 8.0) as i64;
    for part in 0..=parts {
        let fraction = part as f64 / parts as f64;
        let direction = unit(std::array::from_fn(|axis| {
            from[axis] + (to[axis] - from[axis]) * fraction
        }));
        let time = start_ms + (move_ms as f64 * fraction) as i64;
        route.push(entry(ground, time, direction));
    }
    // Standing at the end until the next walk starts.
    if route.last().map_or(true, |last| last[0] < end_ms as f64) {
        route.push(entry(ground, end_ms, to));
    }
}

/// Appends the walk from `from` to `to`; when a wall, fence or tree is in the way it goes by `via` (a spot known to
/// be open, the home or the place of the character) instead, the two walks sharing the time up to `end_ms`.
fn leg_around(
    route: &mut Vec<[f64; 4]>,
    ground: &dyn Ground,
    [from, via, to]: [[f64; 3]; 3],
    (start_ms, end_ms): (i64, i64),
    speed: f64,
) {
    if !ground.blocked(from, to) {
        return leg(route, ground, from, to, start_ms, end_ms, speed);
    }
    let first_ms = ((arc_m(from, via) / speed) * 1000.0).round() as i64;
    let middle = (start_ms + first_ms).min(end_ms);
    leg(route, ground, from, via, start_ms, middle, speed);
    leg(route, ground, via, to, middle, end_ms, speed);
}

/// The spot a wandering character heads for in `slot`: on land near its home, else home itself.
fn waypoint(ground: &dyn Ground, id: &str, home: [f64; 3], slot: i64, radius: f64) -> [f64; 3] {
    let hash = Sha256::digest(format!("{id}:walk:{slot}").as_bytes());
    let direction = offset(
        home,
        word(&hash, 1).sqrt() * radius,
        word(&hash, 0) * std::f64::consts::TAU,
    );
    if ground.land_site(direction).is_some() && !ground.blocked(home, direction) {
        direction
    } else {
        home
    }
}

/// The route of a wandering character around `home` (a unit direction), from the start of the current slot.
fn wander(
    ground: &dyn Ground,
    id: &str,
    home: [f64; 3],
    movement: &Movement,
    now_ms: i64,
) -> Vec<[f64; 4]> {
    let pause_ms = (movement.pause_s * 1000.0) as i64;
    let slot_ms =
        ((2.0 * movement.radius_m / movement.speed_m_s) * 1000.0).ceil() as i64 + pause_ms;
    let slot_ms = slot_ms.max(10_000);
    let first = now_ms.div_euclid(slot_ms);
    let mut route = Vec::new();
    let mut slot = first;
    loop {
        // The walk of a slot is the same whenever it is asked for, and costs a dozen look-ups of the ground: it is
        // worked out once (every client asks for the routes of the same characters again and again).
        let key = (
            id.to_owned(),
            slot,
            slot_ms,
            movement.radius_m.to_bits(),
            movement.speed_m_s.to_bits(),
            // The ground and the home, so that another world (or a rebuilt ground) never reads these walks.
            ground.cache_id(),
            home.map(f64::to_bits),
        );
        let cacheable = key.5 != 0;
        let cached = if cacheable {
            SLOT_WALKS
                .lock()
                .ok()
                .and_then(|cache| cache.get(&key).cloned())
        } else {
            None
        };
        let walk = cached.unwrap_or_else(|| {
            let from = waypoint(ground, id, home, slot, movement.radius_m);
            let to = waypoint(ground, id, home, slot + 1, movement.radius_m);
            let mut walk = Vec::new();
            leg_around(
                &mut walk,
                ground,
                [from, home, to],
                (slot * slot_ms, (slot + 1) * slot_ms),
                movement.speed_m_s,
            );
            if let (true, Ok(mut cache)) = (cacheable, SLOT_WALKS.lock()) {
                if cache.len() >= MAX_CACHED_SLOTS {
                    cache.clear();
                }
                cache.insert(key, walk.clone());
            }
            walk
        });
        route.extend(walk);
        slot += 1;
        let covered = route
            .last()
            .is_some_and(|last| last[0] >= (now_ms + COVER_MS) as f64);
        if covered || route.len() + 10 > MAX_POINTS {
            return route;
        }
    }
}

/// The route of a patrolling character on the loop of `points` (unit directions).
fn patrol(
    ground: &dyn Ground,
    id: &str,
    points: &[[f64; 3]],
    site: [f64; 3],
    movement: &Movement,
    now_ms: i64,
) -> Vec<[f64; 4]> {
    let pause_ms = (movement.pause_s * 1000.0) as i64;
    let leg_ms: Vec<i64> = (0..points.len())
        .map(|index| {
            let next = points[(index + 1) % points.len()];
            // A leg that a wall blocks goes by the place of the character.
            let metres = if movement.follow.is_none() && ground.blocked(points[index], next) {
                arc_m(points[index], site) + arc_m(site, next)
            } else {
                arc_m(points[index], next)
            };
            // Along a road the pace is half as fast again, as in `leg`.
            let from = points[index];
            let middle = unit(std::array::from_fn(|axis| (from[axis] + next[axis]) / 2.0));
            let pace = if ground.on_trail(from) && ground.on_trail(middle) && ground.on_trail(next)
            {
                crate::movement::ROAD_PACE
            } else {
                1.0
            };
            ((metres / (movement.speed_m_s * pace)) * 1000.0).round() as i64 + pause_ms
        })
        .collect();
    let cycle: i64 = leg_ms.iter().sum::<i64>().max(1);
    let hash = Sha256::digest(format!("{id}:patrol").as_bytes());
    let phase = (word(&hash, 0) * cycle as f64) as i64;
    // Where in the loop the character is now: the leg and how far into it.
    let mut into = (now_ms + phase).rem_euclid(cycle);
    let mut index = 0;
    while into >= leg_ms[index] {
        into -= leg_ms[index];
        index = (index + 1) % points.len();
    }
    let mut start = now_ms - into;
    let mut route = Vec::new();
    loop {
        let next = (index + 1) % points.len();
        if movement.follow.is_some() {
            // The road is clear of what would be in the way: it never goes round by its place.
            leg(
                &mut route,
                ground,
                points[index],
                points[next],
                start,
                start + leg_ms[index],
                movement.speed_m_s,
            );
        } else {
            leg_around(
                &mut route,
                ground,
                [points[index], site, points[next]],
                (start, start + leg_ms[index]),
                movement.speed_m_s,
            );
        }
        start += leg_ms[index];
        index = next;
        let covered = route
            .last()
            .is_some_and(|last| last[0] >= (now_ms + COVER_MS) as f64);
        if covered || route.len() + 10 > MAX_POINTS {
            return route;
        }
    }
}

/// The route of a shepherd from the start of the current step: it walks a little behind the centre of its flock.
fn herd(ground: &dyn Ground, fold: [f64; 3], movement: &Movement, now_ms: i64) -> Vec<[f64; 4]> {
    let pasture = patrol_points(ground, fold, movement).first().copied();
    let phase_ms = (movement.phase_s * 1000.0) as i64;
    let first = now_ms.div_euclid(HERD_STEP_MS) * HERD_STEP_MS;
    (0..=HERD_POINTS)
        .map(|step| {
            let time = first + step * HERD_STEP_MS;
            let behind = time - crate::herd::SHEPHERD_LAG_MS + phase_ms;
            entry(ground, time, crate::herd::centre(fold, pasture, behind))
        })
        .collect()
}

/// A shepherd's route has a waypoint every step and covers two minutes.
const HERD_STEP_MS: i64 = 10_000;
const HERD_POINTS: i64 = 13;

/// The directions of the waypoints of a patrol, as far as they lie on land.
pub fn patrol_points(ground: &dyn Ground, site: [f64; 3], movement: &Movement) -> Vec<[f64; 3]> {
    movement
        .waypoints
        .iter()
        .filter_map(|spot| {
            let (direction, _) =
                crate::scenery::world_from_local(site, spot[0], spot[1], |_| 0.0, RADIUS);
            ground.land_site(direction).map(|_| direction)
        })
        .collect()
}

/// The route of a character from `now_ms` on; empty when it has nowhere to walk (it then stands at home).
///
/// `home` and `site` are unit directions of where the character was put and of its place.
pub fn route(
    ground: &dyn Ground,
    id: &str,
    home: [f64; 3],
    site: [f64; 3],
    movement: &Movement,
    now_ms: i64,
) -> Vec<[f64; 4]> {
    match movement.kind {
        MovementKind::Wander => wander(ground, id, home, movement, now_ms),
        MovementKind::Herd => herd(ground, site, movement, now_ms),
        MovementKind::Patrol | MovementKind::Travel => {
            let points = patrol_points(ground, site, movement);
            if points.len() < 2 {
                Vec::new()
            } else {
                patrol(ground, id, &points, site, movement, now_ms)
            }
        }
    }
}

/// The stops of a character during conversations (migration 0040): the time of its walk stands still while it talks to a player.
///
/// `shift_ms` is how long it stood in earlier conversations; the current stop lasts from `from_ms` to `to_ms` (unix milliseconds).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pause {
    pub shift_ms: i64,
    pub from_ms: i64,
    pub to_ms: i64,
}

impl Pause {
    /// The time of the character's walk at `now_ms`, and whether it stands still now. The walk's time never runs backwards and is
    /// continuous: it is `now_ms` less everything the character has stood, and constant while it stands.
    pub fn clock(&self, now_ms: i64) -> (i64, bool) {
        let stood = (now_ms.min(self.to_ms) - self.from_ms).max(0);
        (
            now_ms - self.shift_ms - stood,
            now_ms >= self.from_ms && now_ms < self.to_ms,
        )
    }

    /// What the walk's times are to be shifted by to get unix times: everything stood until now (a stop that has not begun counts for nothing).
    pub fn offset_ms(&self, now_ms: i64) -> i64 {
        self.shift_ms + (now_ms.min(self.to_ms) - self.from_ms).max(0)
    }
}

/// Where a character is at `now_ms` (metres), given its route; its home when it has none.
pub fn position_at(route: &[[f64; 4]], home: [f64; 3], now_ms: i64) -> [f64; 3] {
    if route.is_empty() {
        home
    } else {
        crate::movement::route_position(route, now_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_conversation_stops_the_clock_and_the_walk_goes_on_from_there() {
        let pause = Pause {
            shift_ms: 5_000,
            from_ms: 100_000,
            to_ms: 160_000,
        };
        // before the stop only the earlier stops count
        assert_eq!(pause.clock(90_000), (85_000, false));
        // during the stop the walk's time stands still
        assert_eq!(pause.clock(100_000), (95_000, true));
        assert_eq!(pause.clock(130_000), (95_000, true));
        assert_eq!(pause.clock(159_999), (95_000, true));
        // after it the walk goes on from the same time, and the stop is part of the shift
        assert_eq!(pause.clock(160_000), (95_000, false));
        assert_eq!(pause.clock(200_000), (135_000, false));
        assert_eq!(pause.offset_ms(200_000), 65_000);
        // the walk's time never runs backwards
        let times: Vec<i64> = (0..300).map(|i| pause.clock(i * 1_000).0).collect();
        assert!(times.windows(2).all(|pair| pair[0] <= pair[1]));
    }
    use crate::story::placement::Ground;

    /// A flat world of land.
    struct Flat;

    impl Ground for Flat {
        fn land_site(&self, direction: [f64; 3]) -> Option<(u8, f64)> {
            Some((4, self.height(direction)))
        }
        fn height(&self, _direction: [f64; 3]) -> f64 {
            50.0
        }
        fn is_sea(&self, _direction: [f64; 3]) -> bool {
            false
        }
    }

    /// A flat world of land that is all road.
    struct Road;

    impl Ground for Road {
        fn land_site(&self, direction: [f64; 3]) -> Option<(u8, f64)> {
            Flat.land_site(direction)
        }
        fn height(&self, direction: [f64; 3]) -> f64 {
            Flat.height(direction)
        }
        fn is_sea(&self, _direction: [f64; 3]) -> bool {
            false
        }
        fn on_trail(&self, _direction: [f64; 3]) -> bool {
            true
        }
    }

    #[test]
    fn a_character_on_a_road_walks_half_as_fast_again() {
        let from = unit([1.0, 0.0, 0.0]);
        let to = unit([1.0, 0.0, 0.00004]);
        let country = {
            let mut route = Vec::new();
            leg(&mut route, &Flat, from, to, 0, 1_000_000, 1.0);
            route[route.len() - 2][0]
        };
        let road = {
            let mut route = Vec::new();
            leg(&mut route, &Road, from, to, 0, 1_000_000, 1.0);
            route[route.len() - 2][0]
        };
        assert!(
            (country / road - 1.5).abs() < 0.02,
            "{country} against {road}"
        );
    }

    /// A flat world of land in which every walk of any length runs into a wall.
    struct Boxed;

    impl Ground for Boxed {
        fn land_site(&self, direction: [f64; 3]) -> Option<(u8, f64)> {
            Flat.land_site(direction)
        }
        fn height(&self, direction: [f64; 3]) -> f64 {
            Flat.height(direction)
        }
        fn is_sea(&self, _direction: [f64; 3]) -> bool {
            false
        }
        fn blocked(&self, _from: [f64; 3], _to: [f64; 3]) -> bool {
            true
        }
    }

    #[test]
    fn characters_do_not_walk_through_walls() {
        let home = unit([1.0, 0.2, 0.1]);
        let now = 1_800_000_123_456;
        let route = wander(&Boxed, "gilgamesh:potter", home, &wanderer(), now);
        assert!(!route.is_empty());
        assert!(
            route
                .iter()
                .all(|waypoint| metres_from(home, [waypoint[1], waypoint[2], waypoint[3]]) < 0.1),
            "a wanderer whose every walk is blocked stays at home"
        );
        assert!(
            route.windows(2).all(|pair| pair[0][0] <= pair[1][0]),
            "time never runs back"
        );
        // A patrol whose legs are blocked goes by its place and never through the wall between its waypoints.
        let site = unit([1.0, 0.2, 0.1]);
        let points = [
            unit([1.0, 0.2, 0.1 + 0.00001]),
            unit([1.0, 0.2, 0.1 - 0.00001]),
        ];
        let patrol_movement = Movement {
            kind: MovementKind::Patrol,
            radius_m: 0.0,
            speed_m_s: 1.0,
            pause_s: 2.0,
            waypoints: Vec::new(),
            phase_s: 0.0,
            follow: None,
        };
        let walked = patrol(
            &Boxed,
            "gilgamesh:royal_guard",
            &points,
            site,
            &patrol_movement,
            now,
        );
        assert!(walked.windows(2).all(|pair| pair[0][0] <= pair[1][0]));
        assert!(walked
            .iter()
            .any(|waypoint| metres_from(site, [waypoint[1], waypoint[2], waypoint[3]]) < 0.2));
    }

    #[test]
    fn a_shepherd_walks_behind_its_flock_to_the_pasture_and_back() {
        let fold = unit([1.0, 0.2, 0.1]);
        let herd_movement = Movement {
            kind: MovementKind::Herd,
            radius_m: crate::herd::DRIVE_M,
            speed_m_s: crate::herd::SPEED_M_S,
            pause_s: 0.0,
            waypoints: vec![[crate::herd::DRIVE_M, 0.0]],
            phase_s: 0.0,
            follow: None,
        };
        let at = |time: i64| {
            let walk = route(
                &Flat,
                "shepherds:shepherd_00",
                fold,
                fold,
                &herd_movement,
                time,
            );
            assert!(walk.len() >= 13, "a route covers two minutes");
            assert!(walk.windows(2).all(|pair| pair[0][0] < pair[1][0]));
            assert!(walk[0][0] <= time as f64 && walk.last().unwrap()[0] >= (time + 90_000) as f64);
            position_at(&walk, [0.0; 3], time)
        };
        let resting = at(10_000);
        let grazing = at(400_000);
        assert!(metres_from(fold, resting) < 1.0);
        let out = metres_from(fold, grazing);
        assert!(
            (crate::herd::DRIVE_M - 1.0..=crate::herd::DRIVE_M + 1.0).contains(&out),
            "{out}"
        );
        assert!(metres_from(fold, at(crate::herd::cycle_ms() + 30_000)) < 1.0);
    }

    fn wanderer() -> Movement {
        Movement {
            kind: MovementKind::Wander,
            radius_m: 30.0,
            speed_m_s: 1.2,
            pause_s: 8.0,
            waypoints: Vec::new(),
            phase_s: 0.0,
            follow: None,
        }
    }

    fn metres_from(home: [f64; 3], point: [f64; 3]) -> f64 {
        arc_m(home, unit(point))
    }

    #[test]
    fn a_wanderer_stays_near_home_and_its_route_is_well_formed() {
        let home = unit([1.0, 0.2, 0.1]);
        let now = 1_800_000_123_456;
        let route = wander(&Flat, "gilgamesh:potter", home, &wanderer(), now);
        assert!(
            route.len() >= 4 && route.len() <= MAX_POINTS,
            "{} points",
            route.len()
        );
        assert!(route[0][0] <= now as f64, "starts before now");
        assert!(
            route.last().unwrap()[0] >= (now + COVER_MS) as f64,
            "covers the next minute and a half"
        );
        assert!(
            route.windows(2).all(|pair| pair[0][0] <= pair[1][0]),
            "time never runs back"
        );
        for waypoint in &route {
            let distance = metres_from(home, [waypoint[1], waypoint[2], waypoint[3]]);
            assert!(distance <= 31.0, "{distance} m from home");
            let height =
                (waypoint[1].powi(2) + waypoint[2].powi(2) + waypoint[3].powi(2)).sqrt() - RADIUS;
            assert!((height - 50.0).abs() < 0.02, "on the ground: {height}");
        }
    }

    #[test]
    fn a_wanderer_walks_at_its_speed_and_stands_between_walks() {
        let home = unit([1.0, 0.2, 0.1]);
        let route = wander(
            &Flat,
            "gilgamesh:potter",
            home,
            &wanderer(),
            1_800_000_000_000,
        );
        let mut walked = false;
        let mut stood = false;
        for pair in route.windows(2) {
            let span = (pair[1][0] - pair[0][0]) / 1000.0;
            if span <= 0.0 {
                continue;
            }
            let metres = ((pair[1][1] - pair[0][1]).powi(2)
                + (pair[1][2] - pair[0][2]).powi(2)
                + (pair[1][3] - pair[0][3]).powi(2))
            .sqrt();
            let speed = metres / span;
            assert!(speed <= 1.25, "never faster than its speed: {speed}");
            walked |= speed > 1.0;
            stood |= speed < 0.01;
        }
        assert!(walked && stood, "walked {walked}, stood {stood}");
    }

    #[test]
    fn the_route_is_the_same_wherever_and_whenever_it_is_asked() {
        let home = unit([1.0, 0.2, 0.1]);
        let a = wander(&Flat, "x", home, &wanderer(), 1_800_000_000_000);
        let b = wander(&Flat, "x", home, &wanderer(), 1_800_000_010_000);
        // Ten seconds later the same slot gives the same waypoints; where both routes overlap they agree.
        let at = 1_800_000_050_000;
        let one = position_at(&a, home, at);
        let two = position_at(&b, home, at);
        assert!(
            one.iter().zip(two).all(|(p, q)| (p - q).abs() < 0.01),
            "{one:?} vs {two:?}"
        );
        let other = wander(&Flat, "y", home, &wanderer(), 1_800_000_000_000);
        assert_ne!(a, other, "another character walks another way");
    }

    #[test]
    fn a_patrol_walks_its_loop_and_characters_start_apart() {
        let site = unit([1.0, 0.2, 0.1]);
        let beat = Movement {
            kind: MovementKind::Patrol,
            radius_m: 25.0,
            speed_m_s: 1.5,
            pause_s: 5.0,
            waypoints: vec![[0.0, 0.0], [30.0, 0.0], [30.0, 30.0], [0.0, 30.0]],
            phase_s: 0.0,
            follow: None,
        };
        let now = 1_800_000_000_000;
        let first = route(&Flat, "a", site, site, &beat, now);
        let second = route(&Flat, "b", site, site, &beat, now);
        assert!(first.len() >= 4 && first.len() <= MAX_POINTS);
        assert!(first.windows(2).all(|pair| pair[0][0] <= pair[1][0]));
        assert_ne!(
            position_at(&first, site, now),
            position_at(&second, site, now),
            "not in step"
        );
        for waypoint in &first {
            let point = unit([waypoint[1], waypoint[2], waypoint[3]]);
            assert!(metres_from(site, point) < 60.0, "stays on its beat");
        }
        // One loop is 120 m of walking at 1.5 m/s and four pauses: back where it was after a whole cycle.
        let cycle = (120.0 / 1.5 * 1000.0) as i64 + 4 * 5_000;
        let again = route(&Flat, "a", site, site, &beat, now + cycle);
        let here = position_at(&first, site, now + 20_000);
        let there = position_at(&again, site, now + cycle + 20_000);
        assert!(
            here.iter().zip(there).all(|(p, q)| (p - q).abs() < 0.05),
            "{here:?} vs {there:?}"
        );
        let lone = Movement {
            waypoints: vec![[0.0, 0.0]],
            ..beat
        };
        assert!(
            route(&Flat, "a", site, site, &lone, now).is_empty(),
            "one waypoint is no beat"
        );
    }
}
