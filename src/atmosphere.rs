//! The sun and moon above the planet: where they stand at a moment, announced to clients with
//! the world (`GET /world`) so that every client draws the same day and night.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize)]
pub struct Solar {
    pub version: u32,
    pub unix_seconds: f64,
    pub direction: [f64; 3],
    pub sidereal_day_seconds: f64,
    pub angular_radius_degrees: f64,
    /// Unit vector toward the moon's centre in the same planet-fixed frame as `direction`.
    pub moon_direction: [f64; 3],
    pub moon_angular_radius_degrees: f64,
    /// Lit fraction of the lunar disc as seen from the planet (0 = new, 1 = full).
    pub moon_illumination: f64,
}

impl Solar {
    pub fn now() -> Self {
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        Self::at(seconds)
    }

    pub fn at(unix_seconds: f64) -> Self {
        let days = unix_seconds / 86400.0 + 2440587.5 - 2451545.0;
        let anomaly = (357.528 + 0.9856003 * days).rem_euclid(360.0).to_radians();
        let longitude =
            (280.460 + 0.9856474 * days + 1.915 * anomaly.sin() + 0.020 * (2.0 * anomaly).sin())
                .rem_euclid(360.0)
                .to_radians();
        let obliquity = (23.439 - 0.0000004 * days).to_radians();
        let right_ascension = (obliquity.cos() * longitude.sin()).atan2(longitude.cos());
        let declination = (obliquity.sin() * longitude.sin()).asin();
        let sidereal = (280.46061837 + 360.98564736629 * days)
            .rem_euclid(360.0)
            .to_radians();
        let local_longitude = right_ascension - sidereal;
        let distance_au = 1.00014 - 0.01671 * anomaly.cos() - 0.00014 * (2.0 * anomaly).cos();
        let direction = [
            declination.cos() * local_longitude.cos(),
            declination.sin(),
            -declination.cos() * local_longitude.sin(),
        ];
        let (moon_direction, moon_angular_radius_degrees) = moon_at(days, obliquity, sidereal);
        let elongation = (direction[0] * moon_direction[0]
            + direction[1] * moon_direction[1]
            + direction[2] * moon_direction[2])
            .clamp(-1.0, 1.0)
            .acos();
        // New moon when the moon stands with the sun (elongation 0); full when opposite.
        let moon_illumination = (1.0 - elongation.cos()) * 0.5;
        Self {
            version: 1,
            unix_seconds,
            direction,
            sidereal_day_seconds: 86164.0905,
            angular_radius_degrees: 0.2666 / distance_au,
            moon_direction,
            moon_angular_radius_degrees,
            moon_illumination,
        }
    }
}

/// Low-order lunar position in the same planet-fixed frame as the sun (Meeus-style mean
/// elements with the leading longitude and latitude terms). Distance is kilometres from
/// the planet's centre; the angular radius uses the Moon's mean volumetric radius.
fn moon_at(days: f64, obliquity: f64, sidereal: f64) -> ([f64; 3], f64) {
    let mean_longitude = (218.316 + 13.176396 * days).rem_euclid(360.0).to_radians();
    let mean_anomaly = (134.963 + 13.064993 * days).rem_euclid(360.0).to_radians();
    let argument = (93.272 + 13.229350 * days).rem_euclid(360.0).to_radians();
    let lambda = mean_longitude + 6.289_f64.to_radians() * mean_anomaly.sin();
    let beta = 5.128_f64.to_radians() * argument.sin();
    let distance_km = 385001.0 - 20905.0 * mean_anomaly.cos();
    let cos_beta = beta.cos();
    let sin_beta = beta.sin();
    let cos_lambda = lambda.cos();
    let sin_lambda = lambda.sin();
    let cos_eps = obliquity.cos();
    let sin_eps = obliquity.sin();
    let x = cos_beta * cos_lambda;
    let y = cos_beta * sin_lambda * cos_eps - sin_beta * sin_eps;
    let z = cos_beta * sin_lambda * sin_eps + sin_beta * cos_eps;
    let right_ascension = y.atan2(x);
    let declination = z.atan2((x * x + y * y).sqrt());
    let local_longitude = right_ascension - sidereal;
    let direction = [
        declination.cos() * local_longitude.cos(),
        declination.sin(),
        -declination.cos() * local_longitude.sin(),
    ];
    let angular_radius_degrees = (1737.4 / distance_km).asin().to_degrees();
    (direction, angular_radius_degrees)
}

/// The sun's centre sits this many degrees below the horizon when the day begins and ends:
/// the same astronomical horizon the client uses for its clock.
pub const HORIZON_ELEVATION_DEGREES: f64 = -0.833;

/// The moment the sun crosses the horizon at `observer` (a unit vector pointing from the
/// planet's centre) next after `after`. `rising` picks dawn over dusk: when the observer
/// already stands on the requested side, the next crossing in that direction comes about a
/// day later. When none fits one solar day (a polar night or midnight sun) the search gives
/// up and returns `after`. The crossing is bracketed with quarter-hour steps and then
/// bisected to below a second.
pub fn next_horizon_crossing(observer: [f64; 3], after: f64, rising: bool) -> f64 {
    let threshold = HORIZON_ELEVATION_DEGREES.to_radians().sin();
    let above = |seconds: f64| {
        let direction = Solar::at(seconds).direction;
        observer[0] * direction[0] + observer[1] * direction[1] + observer[2] * direction[2]
            >= threshold
    };
    let step = 300.0;
    let mut beginning = after;
    for index in 1..289 {
        let end = after + index as f64 * step;
        if above(end) == above(beginning) {
            beginning = end;
            continue;
        }
        // A crossing sits in (beginning, end), but only the asked-for direction ends it.
        if above(beginning) == rising {
            beginning = end;
            continue;
        }
        let mut low = beginning;
        let mut high = end;
        for _ in 0..20 {
            let middle = (low + high) * 0.5;
            if above(middle) == above(low) {
                low = middle;
            } else {
                high = middle;
            }
        }
        return high;
    }
    after
}

#[cfg(test)]
mod solar_tests {
    use super::{next_horizon_crossing, Solar, HORIZON_ELEVATION_DEGREES};

    #[test]
    fn noon_midnight_and_unit_direction() {
        let noon = Solar::at(946728000.0);
        let midnight = Solar::at(946728000.0 + 43200.0);
        assert!(noon.direction[0] > 0.9);
        assert!(midnight.direction[0] < -0.9);
        for solar in [noon, midnight] {
            assert!(
                (solar
                    .direction
                    .iter()
                    .map(|value| value * value)
                    .sum::<f64>()
                    - 1.0)
                    .abs()
                    < 1e-12
            );
            assert!((0.26..0.28).contains(&solar.angular_radius_degrees));
            assert!(
                (solar
                    .moon_direction
                    .iter()
                    .map(|value| value * value)
                    .sum::<f64>()
                    - 1.0)
                    .abs()
                    < 1e-12
            );
            assert!((0.24..0.30).contains(&solar.moon_angular_radius_degrees));
            assert!((0.0..=1.0).contains(&solar.moon_illumination));
        }
    }

    #[test]
    fn the_moon_moves_and_phases() {
        let first = Solar::at(946728000.0);
        let later = Solar::at(946728000.0 + 7.0 * 86400.0);
        let fortnight = Solar::at(946728000.0 + 14.0 * 86400.0);
        let moved = first.moon_direction[0] * later.moon_direction[0]
            + first.moon_direction[1] * later.moon_direction[1]
            + first.moon_direction[2] * later.moon_direction[2];
        assert!(moved < 0.5, "moon should travel far in a week, cos={moved}");
        // Over half a synodic month the illuminated fraction must change substantially.
        assert!((first.moon_illumination - fortnight.moon_illumination).abs() > 0.3);
    }

    #[test]
    fn seasons_and_polar_day_are_opposite() {
        let june = Solar::at((2460483.0 - 2440587.5) * 86400.0);
        let december = Solar::at((2460666.0 - 2440587.5) * 86400.0);
        assert!((june.direction[1] - 23.44_f64.to_radians().sin()).abs() < 0.001);
        assert!((december.direction[1] + 23.44_f64.to_radians().sin()).abs() < 0.001);
        for hours in 0..24 {
            let summer = Solar::at(june.unix_seconds + hours as f64 * 3600.0);
            let winter = Solar::at(december.unix_seconds + hours as f64 * 3600.0);
            assert!(summer.direction[1] > 0.0 && winter.direction[1] < 0.0);
        }
    }

    #[test]
    fn the_sun_rises_in_the_east() {
        // At Greenwich on the equator up is +X, north +Y and east -Z (right-handed frame, see
        // `ishtaria-worldgen --earth`): the sun must stand on the east side when it rises and on the
        // west side when it sets.
        let equator = [1.0, 0.0, 0.0];
        let noon = 946728000.0;
        let sunrise = next_horizon_crossing(equator, noon, true);
        let sunset = next_horizon_crossing(equator, noon, false);
        let at_rise = Solar::at(sunrise).direction;
        let at_set = Solar::at(sunset).direction;
        assert!(-at_rise[2] > 0.5, "sunrise direction {at_rise:?}");
        assert!(-at_set[2] < -0.5, "sunset direction {at_set:?}");
    }

    #[test]
    fn dawn_and_dusk_follow_each_other_on_the_equator() {
        let equator = [1.0, 0.0, 0.0];
        let noon = 946728000.0;
        let threshold = HORIZON_ELEVATION_DEGREES.to_radians().sin();
        let elevation = |seconds: f64| {
            let direction = Solar::at(seconds).direction;
            equator[0] * direction[0] + equator[1] * direction[1] + equator[2] * direction[2]
        };
        let dusk = next_horizon_crossing(equator, noon, false);
        assert!(dusk > noon && dusk < noon + 14.0 * 3600.0);
        assert!((elevation(dusk) - threshold).abs() < 1e-6);
        // Asking for a dusk while it is already night still finds the next one.
        assert!(next_horizon_crossing(equator, dusk + 60.0, false) > dusk);
        let dawn = next_horizon_crossing(equator, noon, true);
        assert!(dawn > noon + 15.0 * 3600.0 && dawn < noon + 21.0 * 3600.0);
        assert!((elevation(dawn) - threshold).abs() < 1e-6);
        // Above the polar circle the sun may not cross the horizon at all.
        let polar_night = [0.0, 1.0, 0.0];
        let december = (2460666.0 - 2440587.5) * 86400.0;
        assert_eq!(
            next_horizon_crossing(polar_night, december, true),
            december,
            "no dawn during the polar night"
        );
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Atmosphere {
    pub skybox: String,
    pub space_skybox: String,
    pub sun_color: [f32; 3],
    pub sun_energy: f32,
    pub ambient_color: [f32; 3],
    pub ambient_energy: f32,
    pub fog_color: [f32; 3],
    pub fog_density: f32,
    pub sky_energy: f32,
}

impl Default for Atmosphere {
    fn default() -> Self {
        Self {
            skybox: "day".into(),
            space_skybox: "galaxy".into(),
            sun_color: [1.0, 0.95, 0.85],
            sun_energy: 1.2,
            ambient_color: [0.7, 0.78, 0.84],
            ambient_energy: 0.45,
            fog_color: [0.72, 0.84, 0.85],
            fog_density: 0.000025,
            sky_energy: 1.0,
        }
    }
}

impl Atmosphere {
    pub fn validate(&self) -> Result<()> {
        if !["day", "morning", "night", "alien", "space"].contains(&self.skybox.as_str()) {
            bail!("unknown surface skybox");
        }
        if !["band", "dark", "day", "galaxy", "nebula"].contains(&self.space_skybox.as_str()) {
            bail!("unknown space skybox");
        }
        for color in [self.sun_color, self.ambient_color, self.fog_color] {
            if color
                .iter()
                .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
            {
                bail!("atmosphere RGB values must be finite and in 0..1");
            }
        }
        for (value, maximum) in [
            (self.sun_energy, 16.0),
            (self.ambient_energy, 8.0),
            (self.sky_energy, 8.0),
            (self.fog_density, 0.02),
        ] {
            if !value.is_finite() || !(0.0..=maximum).contains(&value) {
                bail!("invalid atmosphere energy or fog density");
            }
        }
        Ok(())
    }
}
