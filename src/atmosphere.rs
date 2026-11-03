//! The sun and moon above the planet: where they stand at a moment, announced to clients with
//! the world (`GET /world`) so that every client draws the same day and night.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// The sky of the world: the tilt of the axis, the precession of the stars (how far the star field has turned about
/// the pole of the ecliptic since J2000) and the planet's turn. Today's Earth unless a disk says otherwise.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sky {
    /// Degrees. `None` keeps today's slowly changing tilt.
    pub obliquity_degrees: Option<f64>,
    pub precession_degrees: f64,
    pub sidereal_day_seconds: f64,
}

pub const EARTH_SIDEREAL_DAY_SECONDS: f64 = 86164.0905;

impl Sky {
    pub const TODAY: Sky = Sky {
        obliquity_degrees: None,
        precession_degrees: 0.0,
        sidereal_day_seconds: EARTH_SIDEREAL_DAY_SECONDS,
    };

    /// The sky a datadisk asks for.
    pub fn of(astronomy: &super::story::disk::Astronomy) -> Sky {
        let centuries = astronomy.year.map(|year| (year - 2000.0) / 100.0);
        Sky {
            obliquity_degrees: astronomy.obliquity_deg.or(centuries.map(obliquity_of_year)),
            precession_degrees: centuries.map_or(0.0, precession_since),
            sidereal_day_seconds: astronomy
                .sidereal_day_seconds
                .unwrap_or(EARTH_SIDEREAL_DAY_SECONDS),
        }
    }
}

/// Mean obliquity of the ecliptic in degrees `centuries` after J2000 (Laskar 1986/2004, a tenth-order polynomial in
/// ten-thousand-year units; good to a few arc seconds over a thousand years and well within a hundredth of a degree
/// over the millennia of recorded history).
pub fn obliquity_of_year(centuries: f64) -> f64 {
    let u = centuries / 100.0;
    let arcseconds = 84381.448
        + u * (-4680.93
            + u * (-1.55
                + u * (1999.25
                    + u * (-51.38
                        + u * (-249.67
                            + u * (-39.05 + u * (7.12 + u * (27.87 + u * (5.79 + u * 2.45)))))))));
    arcseconds / 3600.0
}

/// How far, in degrees, the star field of J2000 has turned about the ecliptic pole from the equinox of a year
/// `centuries` from J2000: positive for the past (the equinox then lay behind the stars it lies at today). General
/// precession in longitude, p_A = 5028.796195 T + 1.1054348 T^2 arc seconds (Capitaine et al. 2003).
pub fn precession_since(centuries: f64) -> f64 {
    -(5028.796195 * centuries + 1.1054348 * centuries * centuries) / 3600.0
}

static SKY: std::sync::RwLock<Sky> = std::sync::RwLock::new(Sky::TODAY);

/// Sets the sky of the world the server runs (its disks' `astronomy`); `None` is today's Earth.
pub fn set_astronomy(astronomy: Option<super::story::disk::Astronomy>) {
    if let Ok(mut sky) = SKY.write() {
        *sky = astronomy.as_ref().map_or(Sky::TODAY, Sky::of);
    }
}

fn sky() -> Sky {
    SKY.read().map_or(Sky::TODAY, |sky| *sky)
}

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
    /// The tilt of the planet's axis in degrees, and how far the star field has turned about the ecliptic pole since
    /// J2000 (0 for today's sky). Clients draw the sun, the moon and the stars with them.
    pub obliquity_degrees: f64,
    pub precession_degrees: f64,
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
        let world = sky();
        let obliquity_degrees = world.obliquity_degrees.unwrap_or(23.439 - 0.0000004 * days);
        let obliquity = obliquity_degrees.to_radians();
        let right_ascension = (obliquity.cos() * longitude.sin()).atan2(longitude.cos());
        let declination = (obliquity.sin() * longitude.sin()).asin();
        // The planet turns once against the stars in `sidereal_day_seconds` (86164.0905 s on Earth).
        let sidereal = (280.46061837
            + 360.98564736629 * days * EARTH_SIDEREAL_DAY_SECONDS / world.sidereal_day_seconds)
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
            sidereal_day_seconds: world.sidereal_day_seconds,
            angular_radius_degrees: 0.2666 / distance_au,
            moon_direction,
            moon_angular_radius_degrees,
            moon_illumination,
            obliquity_degrees,
            precession_degrees: world.precession_degrees,
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
    use super::{
        next_horizon_crossing, obliquity_of_year, precession_since, Sky, Solar,
        HORIZON_ELEVATION_DEGREES,
    };

    #[test]
    fn the_sky_of_a_past_year_has_its_own_tilt_and_precession() {
        // Laskar: 24.0 degrees around 2700 BCE and 23.8 around 700 BCE; today 23.44.
        let bronze_age = obliquity_of_year((-2699.0 - 2000.0) / 100.0);
        assert!((bronze_age - 24.0).abs() < 0.1, "{bronze_age}");
        let iron_age = obliquity_of_year((-699.0 - 2000.0) / 100.0);
        assert!((iron_age - 23.79).abs() < 0.05, "{iron_age}");
        assert!((obliquity_of_year(0.0) - 23.4393).abs() < 0.001);
        // The star field has turned 65-66 degrees since 2700 BCE (4700 years of 50.3 arc seconds), none at J2000.
        let turned = precession_since((-2699.0 - 2000.0) / 100.0);
        assert!((turned - 65.5).abs() < 1.0, "{turned}");
        assert!(precession_since(0.0).abs() < 1e-12);
        let sky = Sky::of(&crate::story::disk::Astronomy {
            year: Some(-2699.0),
            obliquity_deg: None,
            sidereal_day_seconds: None,
        });
        assert!((sky.obliquity_degrees.unwrap() - bronze_age).abs() < 1e-9);
        // A disk may name the tilt and the turn itself (another planet).
        let other = Sky::of(&crate::story::disk::Astronomy {
            year: None,
            obliquity_deg: Some(25.2),
            sidereal_day_seconds: Some(88_642.7),
        });
        assert_eq!(other.obliquity_degrees, Some(25.2));
        assert_eq!(other.sidereal_day_seconds, 88_642.7);
        assert_eq!(other.precession_degrees, 0.0);
        // Without `astronomy` the announced sky is today's.
        let today = Solar::at(946_728_000.0);
        assert!((today.obliquity_degrees - 23.439).abs() < 0.001);
        assert_eq!(today.precession_degrees, 0.0);
    }

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
