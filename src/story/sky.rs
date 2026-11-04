//! Sky ships crossing between cities (Easter egg of the Gilgamesh disk).
//!
//! Like the herds, a flight has no state: the schedule is a pure function of the clock, so every process and
//! every request agrees and nothing is stored. Time is cut into slots of `interval_s`; each slot holds one flight
//! that starts at a seeded moment of it (never so late that it would reach into the next slot), between two seeded
//! distinct cities, in a seeded kind of ship. Clients draw the ship on the straight line between the cities, lifted
//! to the flight's altitude; it is scenery only and changes no game state.

use super::disk::SkyFlyers;
use serde::Serialize;
use sha2::{Digest, Sha256};

/// A kind of ship: what a client needs to draw it, also for a flight over the player that no schedule announces.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Ship {
    pub id: String,
    /// Served media key of its model (`<disk>/media/models/….glb`).
    pub model: String,
    pub scale: f64,
}

impl From<&super::disk::SkyShip> for Ship {
    fn from(ship: &super::disk::SkyShip) -> Self {
        Self {
            id: ship.id.clone(),
            model: ship.model.clone(),
            scale: ship.scale,
        }
    }
}

/// The answer of `GET /story/sky_flights`.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Sky {
    pub ships: Vec<Ship>,
    pub flights: Vec<Flight>,
}

/// One flight as clients draw it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Flight {
    /// Stable id of the flight (`<slot>`), the same for every client.
    pub id: String,
    /// Id of the ship kind.
    pub ship: String,
    /// Served media key of its model (`<disk>/media/models/….glb`).
    pub model: String,
    pub scale: f64,
    /// Unix milliseconds.
    pub start_ms: i64,
    pub duration_ms: i64,
    /// World metres from the planet's centre, like every position.
    pub from: [f64; 3],
    pub to: [f64; 3],
    pub altitude_m: f64,
}

fn word(hash: &[u8], index: usize) -> u64 {
    u64::from(u32::from_le_bytes(
        hash[index * 4..index * 4 + 4].try_into().unwrap(),
    ))
}

/// The flight of slot number `slot`, or `None` when a city has no site in the world.
fn flight_of(
    flyers: &SkyFlyers,
    slot: i64,
    position: &impl Fn(&str) -> Option<[f64; 3]>,
) -> Option<Flight> {
    let interval_ms = i64::from(flyers.interval_s) * 1000;
    let duration_ms = i64::from(flyers.duration_s) * 1000;
    let hash = Sha256::digest(format!("ishtaria-sky:{slot}").as_bytes());
    let start_ms =
        slot * interval_ms + (word(&hash, 0) % (interval_ms - duration_ms) as u64) as i64;
    let cities = flyers.cities.len() as u64;
    let first = word(&hash, 1) % cities;
    let second = (first + 1 + word(&hash, 2) % (cities - 1)) % cities;
    let ship = &flyers.ships[(word(&hash, 3) % flyers.ships.len() as u64) as usize];
    Some(Flight {
        id: slot.to_string(),
        ship: ship.id.clone(),
        model: ship.model.clone(),
        scale: ship.scale,
        start_ms,
        duration_ms,
        from: position(&flyers.cities[first as usize])?,
        to: position(&flyers.cities[second as usize])?,
        altitude_m: flyers.altitude_m,
    })
}

/// The flights in the air at `now_ms` or still to come in this slot and the next, oldest first.
pub fn flights(
    flyers: &SkyFlyers,
    now_ms: i64,
    position: impl Fn(&str) -> Option<[f64; 3]>,
) -> Vec<Flight> {
    let slot = now_ms.div_euclid(i64::from(flyers.interval_s) * 1000);
    (slot - 1..=slot + 1)
        .filter_map(|slot| flight_of(flyers, slot, &position))
        .filter(|flight| flight.start_ms + flight.duration_ms > now_ms)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::story::disk::SkyShip;

    fn flyers() -> SkyFlyers {
        SkyFlyers {
            interval_s: 3600,
            duration_s: 90,
            altitude_m: 1500.0,
            cities: ["d:uruk", "d:ur", "d:eridu"].map(str::to_owned).to_vec(),
            ships: vec![
                SkyShip {
                    id: "disc".into(),
                    model: "d/media/models/sky_disc.glb".into(),
                    scale: 4.0,
                    achievement: None,
                },
                SkyShip {
                    id: "gir".into(),
                    model: "d/media/models/sky_gir.glb".into(),
                    scale: 4.0,
                    achievement: None,
                },
            ],
        }
    }

    fn place(id: &str) -> Option<[f64; 3]> {
        Some([id.len() as f64, 0.0, 0.0])
    }

    #[test]
    fn one_flight_per_slot_inside_its_slot_between_different_cities() {
        let flyers = flyers();
        let interval = 3_600_000;
        for slot in 0..500 {
            let flight = flight_of(&flyers, slot, &place).unwrap();
            assert!(flight.start_ms >= slot * interval);
            assert!(flight.start_ms + flight.duration_ms <= (slot + 1) * interval);
            assert_ne!(flight.from, flight.to);
        }
    }

    #[test]
    fn the_schedule_depends_only_on_the_clock() {
        let flyers = flyers();
        let now = 1_700_000_123_456;
        assert_eq!(flights(&flyers, now, place), flights(&flyers, now, place));
        // an hour later the same slot is in the past and another flight is shown
        let later = flights(&flyers, now + 7_200_000, place);
        assert!(later
            .iter()
            .all(|flight| flight.start_ms + flight.duration_ms > now + 7_200_000));
    }

    #[test]
    fn both_kinds_of_ship_and_all_cities_appear() {
        let flyers = flyers();
        let seen: std::collections::HashSet<_> = (0..200)
            .filter_map(|slot| flight_of(&flyers, slot, &place))
            .map(|f| f.ship)
            .collect();
        assert_eq!(seen.len(), 2);
    }

    #[test]
    fn a_city_without_a_site_cancels_the_flight() {
        assert!(flight_of(&flyers(), 7, &|_| None).is_none());
    }
}
