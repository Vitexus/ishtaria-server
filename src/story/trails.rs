//! Footpaths between the places of a generated world.
//!
//! A datadisk may define roads of its own; wherever it does not, the generator lays trails itself so
//! that a town is not an island: from a town to its graveyard, its harbour, its pasture and the
//! nearest other town. A trail is the cheapest walk over the ground (A* on a grid in the tangent plane
//! of the place the trail starts from, steep slopes dear, water forbidden), smoothed and laid down as
//! stepping stones of the road module, so the clients need no new model.

use super::placement::Ground;
use crate::scenery::{client_axes, world_from_local};
use std::{cmp::Reverse, collections::BinaryHeap};

/// Distance between two stones of a trail, in metres.
pub const STONE_SPACING_M: f64 = 4.0;
/// A trail goes no further than this, in metres.
pub const MAX_TRAIL_M: f64 = 3_000.0;
/// Longest bridge, in metres between the banks.
pub const MAX_SPAN_M: f64 = 36.0;
/// Steepest slope (rise over run) a trail climbs.
const MAX_SLOPE: f64 = 0.3;
/// Most cells of the search grid on a side.
const MAX_SIDE: usize = 220;

const NEIGHBOURS: [(isize, isize); 8] = [
    (1, 0),
    (-1, 0),
    (0, 1),
    (0, -1),
    (1, 1),
    (1, -1),
    (-1, 1),
    (-1, -1),
];

/// Local metres `(x, z)` of a direction around `origin` (the inverse of `world_from_local` on the tangent plane).
pub fn local_of(origin: [f64; 3], direction: [f64; 3]) -> (f64, f64) {
    let (axis_x, axis_z) = client_axes(origin);
    let radius = crate::movement::RADIUS;
    let up_component: f64 = (0..3).map(|axis| direction[axis] * origin[axis]).sum();
    let scale = radius / up_component.max(1e-9);
    let along = |axis: [f64; 3]| (0..3).map(|i| direction[i] * axis[i]).sum::<f64>() * scale;
    (along(axis_x), along(axis_z))
}

#[derive(Clone, Copy)]
enum Cell {
    Unknown,
    Water,
    Land(f64),
    /// Shallow fresh water, with the height of its bed: crossed on stepping stones at a price.
    Ford(f64),
    /// The sea: never crossed.
    Sea,
}

/// A trail: the smoothed walk and the stretches of it that cross deep water on a bridge.
#[derive(Clone, Debug, PartialEq)]
pub struct Route {
    pub line: Vec<(f64, f64)>,
    pub spans: Vec<Span>,
}

/// A bridge between two banks, in local metres.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Span {
    pub from: (f64, f64),
    pub to: (f64, f64),
}

impl Span {
    pub fn length(&self) -> f64 {
        (self.to.0 - self.from.0).hypot(self.to.1 - self.from.1)
    }
}

/// The cheapest walk from `from` to `to` (local metres around `origin`), as a smoothed polyline that starts and
/// ends exactly there, or `None` when water, cliffs or the search limits leave no way.
pub fn route(
    ground: &dyn Ground,
    origin: [f64; 3],
    from: (f64, f64),
    to: (f64, f64),
) -> Option<Route> {
    route_with(ground, origin, from, to, false)
}

/// [`route`] for a road a disk lays by hand: it also goes round walls, houses and fences where there is room, instead of
/// leaving the stones out where they would stand in one.
pub fn route_avoiding(
    ground: &dyn Ground,
    origin: [f64; 3],
    from: (f64, f64),
    to: (f64, f64),
) -> Option<Route> {
    route_with(ground, origin, from, to, true)
}

fn route_with(
    ground: &dyn Ground,
    origin: [f64; 3],
    from: (f64, f64),
    to: (f64, f64),
    avoid: bool,
) -> Option<Route> {
    // Dry feet first: a bridge is built only where there is no way round and no ford.
    search(ground, origin, from, to, false, avoid)
        .or_else(|| search(ground, origin, from, to, true, avoid))
}

fn search(
    ground: &dyn Ground,
    origin: [f64; 3],
    from: (f64, f64),
    to: (f64, f64),
    bridges: bool,
    avoid: bool,
) -> Option<Route> {
    let distance = (to.0 - from.0).hypot(to.1 - from.1);
    if !(STONE_SPACING_M..=MAX_TRAIL_M).contains(&distance) {
        return None;
    }
    let margin = (distance * 0.4).max(120.0);
    let cell_m = ((distance + 2.0 * margin) / MAX_SIDE as f64).max(8.0);
    let (west, north) = (from.0.min(to.0) - margin, from.1.min(to.1) - margin);
    let side_x = (((from.0.max(to.0) + margin) - west) / cell_m).ceil() as usize + 1;
    let side_z = (((from.1.max(to.1) + margin) - north) / cell_m).ceil() as usize + 1;
    let centre = |column: usize, row: usize| {
        (
            west + (column as f64 + 0.5) * cell_m,
            north + (row as f64 + 0.5) * cell_m,
        )
    };
    let cell_of = |point: (f64, f64)| {
        (
            (((point.0 - west) / cell_m) as usize).min(side_x - 1),
            (((point.1 - north) / cell_m) as usize).min(side_z - 1),
        )
    };
    let mut cells = vec![Cell::Unknown; side_x * side_z];
    // Cells with a fixed obstacle (wall, house, fence) in them, for a road that goes round them.
    let mut obstacles = vec![false; side_x * side_z];
    let mut look = |column: usize, row: usize| -> (Cell, bool) {
        let index = row * side_x + column;
        if let Cell::Unknown = cells[index] {
            let (x, z) = centre(column, row);
            let (direction, _) = world_from_local(origin, x, z, |_| 0.0, crate::movement::RADIUS);
            obstacles[index] = avoid && ground.near_fixed_obstacle(direction, cell_m * 0.5 + 3.0);
            cells[index] = match ground.land_site(direction) {
                Some(_) => Cell::Land(ground.height(direction)),
                None if ground.wade_level(direction).is_some() => {
                    Cell::Ford(ground.height(direction))
                }
                None if ground.is_sea(direction) => Cell::Sea,
                None => Cell::Water,
            };
        }
        (cells[index], obstacles[index])
    };
    let (start, goal) = (cell_of(from), cell_of(to));
    // Ends may stand on a wet or odd cell (a harbour landing): the first and last step are free of the check.
    let key = |column: usize, row: usize| row * side_x + column;
    let mut best = vec![i64::MAX; side_x * side_z];
    let mut came = vec![usize::MAX; side_x * side_z];
    let mut queue = BinaryHeap::new();
    let heuristic = |column: usize, row: usize| {
        let (x, z) = centre(column, row);
        ((x - to.0).hypot(z - to.1) * 100.0) as i64
    };
    best[key(start.0, start.1)] = 0;
    queue.push(Reverse((
        heuristic(start.0, start.1),
        0i64,
        start.0,
        start.1,
    )));
    let mut found = false;
    while let Some(Reverse((_, spent, column, row))) = queue.pop() {
        if spent > best[key(column, row)] {
            continue;
        }
        if (column, row) == goal {
            found = true;
            break;
        }
        let (here, _) = look(column, row);
        for (dx, dz) in NEIGHBOURS {
            let (next_column, next_row) = (column as isize + dx, row as isize + dz);
            if next_column < 0
                || next_row < 0
                || next_column >= side_x as isize
                || next_row >= side_z as isize
            {
                continue;
            }
            let (next_column, next_row) = (next_column as usize, next_row as usize);
            let run = cell_m * ((dx * dx + dz * dz) as f64).sqrt();
            let (there, fixed_there) = look(next_column, next_row);
            let rise = match (here, there) {
                (Cell::Land(a) | Cell::Ford(a), Cell::Land(b) | Cell::Ford(b)) => (b - a).abs(),
                _ if (next_column, next_row) == goal || (column, row) == start => 0.0,
                // Deep fresh water is crossed on a bridge, if bridges are allowed and the sea is not in the way.
                (Cell::Sea, _) | (_, Cell::Sea) => continue,
                _ if bridges => 0.0,
                _ => continue,
            };
            let slope = rise / run;
            if slope > MAX_SLOPE {
                continue;
            }
            // A ford is a last resort: crossing costs as much as a long walk round.
            let wet = match there {
                Cell::Ford(_) => 6.0,
                Cell::Water => 12.0,
                _ => 1.0,
            };
            // A cell with a wall or house in it is crossed only where there is no way round.
            let fixed = if fixed_there { 30.0 } else { 1.0 };
            let cost = (run * (1.0 + 8.0 * slope * slope) * wet * fixed * 100.0) as i64;
            let total = spent + cost;
            let index = key(next_column, next_row);
            if total < best[index] {
                best[index] = total;
                came[index] = key(column, row);
                queue.push(Reverse((
                    total + heuristic(next_column, next_row),
                    total,
                    next_column,
                    next_row,
                )));
            }
        }
    }
    if !found {
        return None;
    }
    let mut cells_on_way = vec![key(goal.0, goal.1)];
    while let Some(&last) = cells_on_way.last() {
        if came[last] == usize::MAX {
            break;
        }
        cells_on_way.push(came[last]);
    }
    cells_on_way.reverse();
    let mut points = vec![from];
    points.extend(
        cells_on_way
            .iter()
            .skip(1)
            .take(cells_on_way.len().saturating_sub(2))
            .map(|&index| centre(index % side_x, index / side_x)),
    );
    points.push(to);
    let line = smooth(&points);
    let spans = spans(ground, origin, &line)?;
    Some(Route { line, spans })
}

/// The stretches of a trail over deep water, each from the last dry sample before it to the first after it.
/// `None` when one is longer than a bridge can be. A trail that starts or ends in water (a harbour landing)
/// has no bridge there.
fn spans(ground: &dyn Ground, origin: [f64; 3], line: &[(f64, f64)]) -> Option<Vec<Span>> {
    const STEP_M: f64 = 1.5;
    let deep = |(x, z): (f64, f64)| {
        let (direction, _) = world_from_local(origin, x, z, |_| 0.0, crate::movement::RADIUS);
        ground.land_site(direction).is_none() && ground.wade_level(direction).is_none()
    };
    let samples: Vec<(f64, f64)> = stones(line, STEP_M, 0.0, 0.0)
        .into_iter()
        .map(|(point, _)| point)
        .collect();
    let mut result = Vec::new();
    let mut index = 0;
    while index < samples.len() {
        if !deep(samples[index]) {
            index += 1;
            continue;
        }
        let first = index;
        while index < samples.len() && deep(samples[index]) {
            index += 1;
        }
        // A run touching an end of the trail is a landing, not a river.
        if first == 0 || index == samples.len() {
            continue;
        }
        let span = Span {
            from: samples[first - 1],
            to: samples[index],
        };
        if span.length() > MAX_SPAN_M {
            return None;
        }
        result.push(span);
    }
    Some(result)
}

/// Rounds the corners of a grid walk (Chaikin cutting) while keeping both ends.
fn smooth(points: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut line = points.to_vec();
    for _ in 0..3 {
        if line.len() < 3 {
            break;
        }
        let mut next = vec![line[0]];
        for pair in line.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            next.push((a.0 * 0.75 + b.0 * 0.25, a.1 * 0.75 + b.1 * 0.25));
            next.push((a.0 * 0.25 + b.0 * 0.75, a.1 * 0.25 + b.1 * 0.75));
        }
        next.push(*line.last().unwrap());
        line = next;
    }
    line
}

/// Whether a point lies within `reach` metres of the segment between two others.
pub fn near_segment(point: (f64, f64), from: (f64, f64), to: (f64, f64), reach: f64) -> bool {
    let (dx, dz) = (to.0 - from.0, to.1 - from.1);
    let length_squared = dx * dx + dz * dz;
    let t = if length_squared == 0.0 {
        0.0
    } else {
        (((point.0 - from.0) * dx + (point.1 - from.1) * dz) / length_squared).clamp(0.0, 1.0)
    };
    (point.0 - from.0 - dx * t).hypot(point.1 - from.1 - dz * t) <= reach
}

/// Points every `spacing` metres along a polyline, with the heading (yaw) of the line there. The ends are
/// left out when they lie within `skip_start` / `skip_end` metres of the polyline's own ends (the places).
pub fn stones(
    line: &[(f64, f64)],
    spacing: f64,
    skip_start: f64,
    skip_end: f64,
) -> Vec<((f64, f64), f64)> {
    let total: f64 = line
        .windows(2)
        .map(|pair| (pair[1].0 - pair[0].0).hypot(pair[1].1 - pair[0].1))
        .sum();
    let mut result = Vec::new();
    let (mut walked, mut next) = (0.0, spacing * 0.5);
    for pair in line.windows(2) {
        let length = (pair[1].0 - pair[0].0).hypot(pair[1].1 - pair[0].1);
        if length == 0.0 {
            continue;
        }
        while next <= walked + length {
            let t = (next - walked) / length;
            if next >= skip_start && next <= total - skip_end {
                result.push((
                    (
                        pair[0].0 + (pair[1].0 - pair[0].0) * t,
                        pair[0].1 + (pair[1].1 - pair[0].1) * t,
                    ),
                    (pair[1].0 - pair[0].0).atan2(pair[1].1 - pair[0].1),
                ));
            }
            next += spacing;
        }
        walked += length;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain around the north pole with a lake and a ridge, in local metres of the pole.
    struct Plain {
        lake: Option<((f64, f64), f64)>,
        ridge_x: Option<f64>,
        /// A shallow river across the plain at `x` in `[from, to]`.
        river: Option<(f64, f64)>,
        /// A deep one, too deep to wade.
        deep_river: Option<(f64, f64)>,
    }

    fn xz(direction: [f64; 3]) -> (f64, f64) {
        let scale = crate::movement::RADIUS / direction[1];
        (direction[0] * scale, direction[2] * scale)
    }

    impl Ground for Plain {
        fn land_site(&self, direction: [f64; 3]) -> Option<(u8, f64)> {
            let (x, z) = xz(direction);
            match self.lake {
                Some((centre, radius)) if (x - centre.0).hypot(z - centre.1) < radius => None,
                _ if self.wade_level(direction).is_some() => None,
                _ if self.is_deep(direction) => None,
                _ => Some((4, self.height(direction))),
            }
        }
        fn wade_level(&self, direction: [f64; 3]) -> Option<f64> {
            if self.is_deep(direction) {
                return None;
            }
            let (x, _) = xz(direction);
            self.river
                .filter(|(from, to)| (*from..=*to).contains(&x))
                .map(|_| 0.0)
        }
        fn height(&self, direction: [f64; 3]) -> f64 {
            let (x, _) = xz(direction);
            // A wall of rock 80 m high across the plain, but for the gap at z in [150, 250].
            match self.ridge_x {
                Some(ridge) if (x - ridge).abs() < 30.0 => {
                    let (_, z) = xz(direction);
                    if (150.0..250.0).contains(&z) {
                        0.0
                    } else {
                        80.0
                    }
                }
                _ => 0.0,
            }
        }
        fn is_sea(&self, _: [f64; 3]) -> bool {
            false
        }
    }

    impl Plain {
        fn is_deep(&self, direction: [f64; 3]) -> bool {
            let (x, _) = xz(direction);
            self.deep_river
                .is_some_and(|(from, to)| (from..=to).contains(&x))
        }
    }

    const POLE: [f64; 3] = [0.0, 1.0, 0.0];

    #[test]
    fn local_coordinates_invert_world_from_local() {
        for (x, z) in [(0.0, 0.0), (500.0, -300.0), (-1200.0, 900.0)] {
            let (direction, _) = world_from_local(POLE, x, z, |_| 0.0, crate::movement::RADIUS);
            let (back_x, back_z) = local_of(POLE, direction);
            assert!((back_x - x).abs() < 0.01 && (back_z - z).abs() < 0.01);
        }
        let site = [0.6, 0.0, 0.8];
        let (direction, _) =
            world_from_local(site, 321.0, -123.0, |_| 0.0, crate::movement::RADIUS);
        let (x, z) = local_of(site, direction);
        assert!(
            (x - 321.0).abs() < 0.1 && (z + 123.0).abs() < 0.1,
            "{x} {z}"
        );
    }

    #[test]
    fn a_trail_goes_round_a_lake_and_stays_on_land() {
        let ground = Plain {
            lake: Some(((400.0, 0.0), 150.0)),
            ridge_x: None,
            river: None,
            deep_river: None,
        };
        let line = route(&ground, POLE, (0.0, 0.0), (800.0, 0.0))
            .expect("a way round the lake")
            .line;
        assert_eq!(line.first(), Some(&(0.0, 0.0)));
        assert_eq!(line.last(), Some(&(800.0, 0.0)));
        for &(x, z) in &line {
            assert!((x - 400.0).hypot(z) > 140.0, "({x}, {z}) is in the lake");
        }
        let length: f64 = line
            .windows(2)
            .map(|pair| (pair[1].0 - pair[0].0).hypot(pair[1].1 - pair[0].1))
            .sum();
        assert!(length > 850.0 && length < 1300.0, "{length}");
        assert_eq!(
            line,
            route(&ground, POLE, (0.0, 0.0), (800.0, 0.0)).unwrap().line
        );
    }

    #[test]
    fn a_trail_uses_the_gap_in_a_ridge() {
        let ground = Plain {
            lake: None,
            ridge_x: Some(300.0),
            river: None,
            deep_river: None,
        };
        let line = route(&ground, POLE, (0.0, 0.0), (600.0, 0.0))
            .expect("through the gap")
            .line;
        let crossing = line
            .iter()
            .filter(|(x, _)| (x - 300.0).abs() < 25.0)
            .all(|(_, z)| (140.0..260.0).contains(z));
        assert!(crossing, "{line:?}");
    }

    #[test]
    fn no_trail_over_open_water_or_beyond_the_limit() {
        let sea = Plain {
            lake: Some(((400.0, 0.0), 2000.0)),
            ridge_x: None,
            river: None,
            deep_river: None,
        };
        assert!(route(&sea, POLE, (-1800.0, 0.0), (1900.0, 0.0)).is_none());
        let plain = Plain {
            lake: None,
            ridge_x: None,
            river: None,
            deep_river: None,
        };
        assert!(route(&plain, POLE, (0.0, 0.0), (MAX_TRAIL_M + 10.0, 0.0)).is_none());
        assert!(route(&plain, POLE, (0.0, 0.0), (1.0, 0.0)).is_none());
    }

    #[test]
    fn stones_are_evenly_spaced_and_skip_the_places() {
        let line = [(0.0, 0.0), (100.0, 0.0)];
        let stones = stones(&line, 4.0, 20.0, 30.0);
        assert!(stones.first().unwrap().0 .0 >= 20.0);
        assert!(stones.last().unwrap().0 .0 <= 70.0);
        for pair in stones.windows(2) {
            assert!((pair[1].0 .0 - pair[0].0 .0 - 4.0).abs() < 1e-9);
        }
        // Heading along +x is a quarter turn.
        assert!((stones[0].1 - std::f64::consts::FRAC_PI_2).abs() < 1e-9);
    }

    #[test]
    fn a_trail_fords_a_shallow_river_but_never_a_lake() {
        let river = Plain {
            lake: None,
            ridge_x: None,
            river: Some((390.0, 420.0)),
            deep_river: None,
        };
        let line = route(&river, POLE, (0.0, 0.0), (800.0, 0.0))
            .expect("a ford")
            .line;
        assert!(line.iter().any(|(x, _)| (390.0..=420.0).contains(x)));
        // The river is crossed straight over, not followed: the walk inside it is about its width.
        let inside: f64 = line
            .windows(2)
            .filter(|pair| (390.0..=420.0).contains(&pair[0].0))
            .map(|pair| (pair[1].0 - pair[0].0).hypot(pair[1].1 - pair[0].1))
            .sum();
        assert!(inside < 60.0, "{inside} m in the river");
    }

    #[test]
    fn a_trail_bridges_a_deep_river_only_when_it_must() {
        let river = Plain {
            lake: None,
            ridge_x: None,
            river: None,
            deep_river: Some((390.0, 420.0)),
        };
        let route = route(&river, POLE, (0.0, 0.0), (800.0, 0.0)).expect("a bridge");
        assert_eq!(route.spans.len(), 1, "{:?}", route.spans);
        let span = route.spans[0];
        assert!(
            span.length() > 28.0 && span.length() < 36.0,
            "{}",
            span.length()
        );
        assert!(span.from.0 < 390.0 && span.to.0 > 420.0);
        // Dry ground needs none.
        let dry = Plain {
            lake: None,
            ridge_x: None,
            river: None,
            deep_river: None,
        };
        assert!(route_of(&dry).spans.is_empty());
        // A river wider than a bridge can be is no way.
        let wide = Plain {
            lake: None,
            ridge_x: None,
            river: None,
            deep_river: Some((300.0, 380.0)),
        };
        assert!(super::route(&wide, POLE, (0.0, 0.0), (800.0, 0.0)).is_none());
    }

    fn route_of(ground: &Plain) -> Route {
        route(ground, POLE, (0.0, 0.0), (800.0, 0.0)).unwrap()
    }

    #[test]
    fn near_segment_measures_to_the_segment_not_the_line() {
        assert!(near_segment((5.0, 1.0), (0.0, 0.0), (10.0, 0.0), 2.0));
        assert!(!near_segment((5.0, 3.0), (0.0, 0.0), (10.0, 0.0), 2.0));
        assert!(!near_segment((14.0, 0.0), (0.0, 0.0), (10.0, 0.0), 2.0));
    }
}
