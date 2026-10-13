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
//! * `patrol`: the character walks the loop of `waypoints` (metres east and south of its place, as for
//!   buildings), standing `pause_s` at each; the start of the loop depends on the id so that several
//!   characters on one beat do not walk in step.

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
    route.push(entry(ground, end_ms, to));
}

/// The spot a wandering character heads for in `slot`: on land near its home, else home itself.
fn waypoint(ground: &dyn Ground, id: &str, home: [f64; 3], slot: i64, radius: f64) -> [f64; 3] {
    let hash = Sha256::digest(format!("{id}:walk:{slot}").as_bytes());
    let direction = offset(
        home,
        word(&hash, 1).sqrt() * radius,
        word(&hash, 0) * std::f64::consts::TAU,
    );
    if ground.land_site(direction).is_some() {
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
        let from = waypoint(ground, id, home, slot, movement.radius_m);
        let to = waypoint(ground, id, home, slot + 1, movement.radius_m);
        leg(
            &mut route,
            ground,
            from,
            to,
            slot * slot_ms,
            (slot + 1) * slot_ms,
            movement.speed_m_s,
        );
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
    movement: &Movement,
    now_ms: i64,
) -> Vec<[f64; 4]> {
    let pause_ms = (movement.pause_s * 1000.0) as i64;
    let leg_ms: Vec<i64> = (0..points.len())
        .map(|index| {
            let next = points[(index + 1) % points.len()];
            ((arc_m(points[index], next) / movement.speed_m_s) * 1000.0).round() as i64 + pause_ms
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
        leg(
            &mut route,
            ground,
            points[index],
            points[next],
            start,
            start + leg_ms[index],
            movement.speed_m_s,
        );
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
        MovementKind::Patrol => {
            let points = patrol_points(ground, site, movement);
            if points.len() < 2 {
                Vec::new()
            } else {
                patrol(ground, id, &points, movement, now_ms)
            }
        }
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

    fn wanderer() -> Movement {
        Movement {
            kind: MovementKind::Wander,
            radius_m: 30.0,
            speed_m_s: 1.2,
            pause_s: 8.0,
            waypoints: Vec::new(),
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
