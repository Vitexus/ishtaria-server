//! Herds of sheep and goats that a shepherd drives to pasture and home again.
//!
//! Like every other walker of the world a herd has no state: where it is depends only on the settlement it
//! belongs to and the time, so every server process and every request agrees and nothing is stored. The
//! herd lives in a cycle of a few minutes: it rests in the fold beside the settlement, is driven out to
//! the pasture, grazes there and is driven home. The animals (`movement`, ids `<seed>:farm:<n>:<site>`
//! with `n` beyond the village's own animals) and the shepherd (`story::walking`, movement kind `herd`)
//! both take the position of the herd's centre from [`centre`].

use crate::movement::unit;

/// Walking speed of a driven herd, metres per second.
pub const SPEED_M_S: f64 = 1.1;
/// How far beyond the fold the pasture lies, metres.
pub const DRIVE_M: f64 = 140.0;
/// How long the herd rests in the fold before it is driven out.
const REST_MS: i64 = 90_000;
/// How long it grazes at the pasture.
const GRAZE_MS: i64 = 360_000;
/// How far behind the herd the shepherd walks.
pub const SHEPHERD_LAG_MS: i64 = 8_000;

/// How long one walk between the fold and the pasture takes.
fn walk_ms(distance_m: f64) -> i64 {
    ((distance_m / SPEED_M_S) * 1000.0).round() as i64
}

/// The length of one cycle. Every pasture is [`DRIVE_M`] from its fold, so that the animals and the shepherd,
/// who work out their positions apart, keep to the same time whatever rounding there is in the places.
pub fn cycle_ms() -> i64 {
    REST_MS + 2 * walk_ms(DRIVE_M) + GRAZE_MS
}

/// How far along the way from the fold (0) to the pasture (1) the herd is at `time_ms`.
pub fn progress(time_ms: i64) -> f64 {
    let walk = walk_ms(DRIVE_M);
    let into = time_ms.rem_euclid(cycle_ms());
    if into < REST_MS {
        0.0
    } else if into < REST_MS + walk {
        (into - REST_MS) as f64 / walk as f64
    } else if into < REST_MS + walk + GRAZE_MS {
        1.0
    } else {
        1.0 - (into - REST_MS - walk - GRAZE_MS) as f64 / walk as f64
    }
}

/// Where the centre of the herd is at `time_ms` (a unit direction). A herd without a pasture stays in its fold.
pub fn centre(fold: [f64; 3], pasture: Option<[f64; 3]>, time_ms: i64) -> [f64; 3] {
    let Some(pasture) = pasture else {
        return fold;
    };
    let along = progress(time_ms);
    unit(std::array::from_fn(|axis| {
        fold[axis] + (pasture[axis] - fold[axis]) * along
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::movement::{dot, RADIUS};

    fn arc_m(first: [f64; 3], second: [f64; 3]) -> f64 {
        RADIUS * dot(first, second).clamp(-1.0, 1.0).acos()
    }

    #[test]
    fn the_herd_rests_walks_out_grazes_and_walks_home() {
        let walk = walk_ms(DRIVE_M);
        assert_eq!(progress(0), 0.0);
        assert_eq!(progress(REST_MS - 1), 0.0);
        let halfway = progress(REST_MS + walk / 2);
        assert!((halfway - 0.5).abs() < 0.01, "{halfway}");
        assert_eq!(progress(REST_MS + walk + 1_000), 1.0);
        let homeward = progress(REST_MS + walk + GRAZE_MS + walk / 2);
        assert!((homeward - 0.5).abs() < 0.01, "{homeward}");
        assert_eq!(progress(cycle_ms()), 0.0);
        // no jump anywhere in the cycle: the herd never moves faster than it is driven
        let step_ms = 1_000;
        let mut last = progress(0);
        for time in (step_ms..=cycle_ms()).step_by(step_ms as usize) {
            let now = progress(time);
            assert!(
                (now - last).abs() * DRIVE_M <= SPEED_M_S * 1.01 + 1e-9,
                "jump at {time}"
            );
            last = now;
        }
    }

    #[test]
    fn the_centre_moves_between_fold_and_pasture() {
        let fold = unit([1.0, 0.0, 0.0]);
        let pasture = unit([1.0, 0.0, DRIVE_M / RADIUS]);
        assert!(arc_m(centre(fold, Some(pasture), 0), fold) < 0.01);
        let out = REST_MS + walk_ms(DRIVE_M) + 1_000;
        assert!(arc_m(centre(fold, Some(pasture), out), pasture) < 0.01);
        assert_eq!(centre(fold, None, out), fold);
    }
}
