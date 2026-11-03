//! Festivals of the world's calendar, counted by the sky: a datadisk's dialogues may ask whether one is on (the flag
//! `festival:<id>` in a condition, never stored for a player).

use crate::atmosphere::ecliptic_longitudes;

/// The moon gains this many degrees a day on the sun (a synodic month is 29.53 days).
const ELONGATION_DEGREES_PER_DAY: f64 = 12.19;
/// The sun moves this many degrees along the ecliptic in a day.
const SUN_DEGREES_PER_DAY: f64 = 0.9856;
/// The days of the Akitu, the Mesopotamian New Year, counted from the new moon.
pub const AKITU_DAYS: f64 = 11.0;

/// The Akitu began with the first new moon after the spring equinox (1 Nisannu) and was kept for eleven days. The
/// first new moon after the equinox is the one at which the sun stands within the first 30 degrees of the ecliptic.
pub fn akitu_on(unix_seconds: f64) -> bool {
    let (sun, moon) = ecliptic_longitudes(unix_seconds);
    let age = (moon - sun).rem_euclid(360.0) / ELONGATION_DEGREES_PER_DAY;
    if age >= AKITU_DAYS {
        return false;
    }
    let sun_at_new_moon = (sun - age * SUN_DEGREES_PER_DAY).rem_euclid(360.0);
    sun_at_new_moon < 30.0
}

/// The festival flags of a moment, as the dialogue conditions name them.
pub fn flags_at(unix_seconds: f64) -> Vec<&'static str> {
    let mut flags = Vec::new();
    if akitu_on(unix_seconds) {
        flags.push("festival:akitu");
    }
    flags
}

pub fn flags_now() -> Vec<&'static str> {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    flags_at(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn akitu_follows_the_first_new_moon_after_the_equinox() {
        // 2026: equinox 20 March, new moons 19 March and 17 April: the Akitu is 17 to 28 April.
        assert!(akitu_on(1_776_686_400.0), "20 April 2026");
        assert!(akitu_on(1_776_384_000.0 + 86_400.0), "18 April 2026");
        assert!(
            !akitu_on(1_774_440_000.0),
            "25 March 2026: the moon is of the month before the equinox"
        );
        assert!(!akitu_on(1_778_414_400.0), "10 May 2026");
        // 2027: equinox 20 March, new moons 8 March and 6 April: the Akitu is 6 to 17 April.
        assert!(akitu_on(1_807_185_600.0), "8 April 2027");
        assert!(!akitu_on(1_805_544_000.0), "20 March 2027");
        assert_eq!(flags_at(1_776_686_400.0), ["festival:akitu"]);
        assert!(flags_at(1_778_414_400.0).is_empty());
    }

    #[test]
    fn the_festival_lasts_about_eleven_days_a_year() {
        // Over a year the flag is on about eleven days (a new moon after the equinox falls within a month).
        let start = 1_767_225_600.0; // 1 January 2026
        let on = (0..365)
            .filter(|day| akitu_on(start + *day as f64 * 86_400.0))
            .count();
        assert!((10..=12).contains(&on), "{on} days");
    }
}
