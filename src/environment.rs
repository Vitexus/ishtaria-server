//! Environment and spawning: the coarse grid of elevation, water and biomes derived from the
//! imported height strip (which already carries its local relief), and where a new character may
//! appear (dry, temperate land away from water).

use rand::{rngs::OsRng, seq::IteratorRandom};
use serde::Serialize;
use std::{cmp::Reverse, collections::BinaryHeap};

pub const MAX_FACE_SIZE: usize = 64;
pub const DRY: i32 = -8001;

/// Ocean (0) and lake (1) cells hold open water over their whole area; a river cell (2) only where its
/// water stands above the coarse ground (the client's `_detail_water_km` reads the same numbers).
pub fn is_open_water(biome: u8) -> bool {
    biome <= 1
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Environment {
    pub version: u32,
    pub heightmap_sha256: String,
    pub seed: String,
    pub face_size: usize,
    pub elevation_m: Vec<i32>,
    pub water_m: Vec<i32>,
    pub downstream: Vec<i32>,
    pub flow: Vec<u32>,
    pub biomes: Vec<u8>,
}

fn cube(face: usize, horizontal: f64, vertical: f64) -> [f64; 3] {
    match face {
        0 => [1.0, vertical, -horizontal],
        1 => [-1.0, vertical, horizontal],
        2 => [horizontal, 1.0, -vertical],
        3 => [horizontal, -1.0, vertical],
        4 => [horizontal, vertical, 1.0],
        _ => [-horizontal, vertical, -1.0],
    }
}

fn neighbor(index: usize, horizontal: isize, vertical: isize, size: usize) -> usize {
    let face = index / (size * size);
    let column = (index % size) as isize + horizontal;
    let row = ((index / size) % size) as isize + vertical;
    let point = cube(
        face,
        (column as f64 + 0.5) * 2.0 / size as f64 - 1.0,
        1.0 - (row as f64 + 0.5) * 2.0 / size as f64,
    );
    let [x_axis, y_axis, z_axis] = point.map(f64::abs);
    let (target, horizontal, vertical) = if x_axis >= y_axis && x_axis >= z_axis {
        if point[0] > 0.0 {
            (0, -point[2] / x_axis, point[1] / x_axis)
        } else {
            (1, point[2] / x_axis, point[1] / x_axis)
        }
    } else if y_axis >= z_axis {
        if point[1] > 0.0 {
            (2, point[0] / y_axis, -point[2] / y_axis)
        } else {
            (3, point[0] / y_axis, point[2] / y_axis)
        }
    } else if point[2] > 0.0 {
        (4, point[0] / z_axis, point[1] / z_axis)
    } else {
        (5, -point[0] / z_axis, point[1] / z_axis)
    };
    let column = (((horizontal + 1.0) * 0.5 * size as f64) as usize).min(size - 1);
    let row = (((1.0 - vertical) * 0.5 * size as f64) as usize).min(size - 1);
    target * size * size + row * size + column
}

/// Hash of a lattice corner in `[0, 1)`.
fn lattice(seed: u64, corner: [i64; 3]) -> f64 {
    let mut mixed = seed;
    for coordinate in corner {
        mixed ^= (coordinate as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    }
    (mixed >> 11) as f64 / (1u64 << 53) as f64
}

/// Smooth fractal value noise in `[0, 1]`, the way Luanti's mapgens draw their heat and humidity fields:
/// slowly varying maps whose neighbourhoods agree, instead of independent patches.
fn climate_noise(seed: u64, point: [f64; 3], frequency: f64, octaves: u32) -> f64 {
    let (mut sum, mut amplitude, mut norm, mut scale) = (0.0, 1.0, 0.0, frequency);
    for octave in 0..octaves {
        let scaled = point.map(|axis| axis * scale);
        let base = scaled.map(f64::floor);
        let fraction = [0, 1, 2].map(|axis| {
            let t = scaled[axis] - base[axis];
            t * t * (3.0 - 2.0 * t)
        });
        let corner = |dx: i64, dy: i64, dz: i64| {
            lattice(
                seed.wrapping_add(u64::from(octave) * 0x1F3D_5B79),
                [
                    base[0] as i64 + dx,
                    base[1] as i64 + dy,
                    base[2] as i64 + dz,
                ],
            )
        };
        let lerp = |a: f64, b: f64, t: f64| a + (b - a) * t;
        let x00 = lerp(corner(0, 0, 0), corner(1, 0, 0), fraction[0]);
        let x10 = lerp(corner(0, 1, 0), corner(1, 1, 0), fraction[0]);
        let x01 = lerp(corner(0, 0, 1), corner(1, 0, 1), fraction[0]);
        let x11 = lerp(corner(0, 1, 1), corner(1, 1, 1), fraction[0]);
        sum += amplitude
            * lerp(
                lerp(x00, x10, fraction[1]),
                lerp(x01, x11, fraction[1]),
                fraction[2],
            );
        norm += amplitude;
        amplitude *= 0.5;
        scale *= 2.0;
    }
    sum / norm
}

/// The land biome of a cell from a heat and a humidity field (as in Luanti's biome selection) with ragged
/// borders: heat falls with latitude and height, humidity is a smooth noise raised near water, and the
/// polar, tree and lowland limits are shifted by noise so that bands of biomes are not straight lines.
fn land_biome(seed: u64, point: [f64; 3], latitude: f64, elevation: i32, near_water: bool) -> u8 {
    let ragged = |salt: u64| climate_noise(seed.wrapping_add(salt), point, 6.0, 2) - 0.5;
    let height = f64::from(elevation);
    let heat = 1.0 - latitude.abs() - height / 6000.0 + ragged(11) * 0.12;
    if heat < 0.28 {
        return 7;
    }
    if height > 1600.0 + ragged(23) * 500.0 {
        return 6;
    }
    if height < 150.0 + ragged(37) * 100.0 {
        return 3;
    }
    let humidity = climate_noise(seed, point, 1.6, 3) + if near_water { 0.12 } else { 0.0 }
        - (heat - 0.7).max(0.0) * 0.3;
    if humidity > 0.42 + (heat - 0.6) * 0.2 {
        5
    } else {
        4
    }
}

/// Land biomes a datadisk paints over the planet (`world.biomes`): an equirectangular 8-bit raster in the frame
/// of the world rasters (row 0 at +90 degrees, column 0 at -180 degrees). A pixel is `0` where the generator
/// decides, otherwise the number of a land biome (3 lowland ... 7 polar, see `story::disk::BIOMES`). Water is
/// never painted: the sea, lakes and rivers follow the relief.
#[derive(Clone, Debug, PartialEq)]
pub struct BiomeMap {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

impl BiomeMap {
    /// Parses a binary P5 PGM (maxval 255, `width == 2 * height`) whose pixels are 0 or land biomes.
    pub fn from_pgm(bytes: &[u8]) -> Result<Self, String> {
        let mut cursor = 0usize;
        let mut tokens = Vec::with_capacity(4);
        while tokens.len() < 4 {
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if bytes.get(cursor) == Some(&b'#') {
                while cursor < bytes.len() && bytes[cursor] != b'\n' {
                    cursor += 1;
                }
                continue;
            }
            let start = cursor;
            while cursor < bytes.len() && !bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if start == cursor {
                return Err("truncated PGM header".into());
            }
            tokens.push(String::from_utf8_lossy(&bytes[start..cursor]).into_owned());
        }
        if tokens[0] != "P5" {
            return Err("expected a binary P5 PGM".into());
        }
        let number = |index: usize| {
            tokens[index]
                .parse::<usize>()
                .map_err(|_| "invalid PGM header")
        };
        let (width, height, maxval) = (number(1)?, number(2)?, number(3)?);
        if width == 0 || height == 0 || width != 2 * height || maxval != 255 {
            return Err("a biome map must be 8-bit with width = 2 x height".into());
        }
        // Exactly one whitespace byte separates the header from the pixels.
        let pixels = bytes
            .get(cursor + 1..)
            .filter(|rest| rest.len() == width * height)
            .ok_or("the biome map does not hold exactly width x height pixels")?;
        if let Some(bad) = pixels
            .iter()
            .find(|&&value| value != 0 && !(3..=7).contains(&value))
        {
            return Err(format!(
                "pixel value {bad} is not 0 (generator decides) or a land biome 3..7"
            ));
        }
        Ok(Self {
            width,
            height,
            pixels: pixels.to_vec(),
        })
    }

    /// The painted land biome at a direction from the planet's centre, or `None` where the generator decides.
    pub fn at(&self, direction: [f64; 3]) -> Option<u8> {
        let length = direction.iter().map(|axis| axis * axis).sum::<f64>().sqrt();
        if length == 0.0 {
            return None;
        }
        let latitude = (direction[1] / length).clamp(-1.0, 1.0).asin().to_degrees();
        let longitude = (-direction[2]).atan2(direction[0]).to_degrees();
        let column =
            (((longitude + 180.0) / 360.0 * self.width as f64) as usize).min(self.width - 1);
        let row = (((90.0 - latitude) / 180.0 * self.height as f64) as usize).min(self.height - 1);
        match self.pixels[row * self.width + column] {
            0 => None,
            biome => Some(biome),
        }
    }
}

#[cfg(test)]
pub fn generate(size: usize, pixels: &[u16], seed: &str, sha256: &str) -> Option<Environment> {
    generate_with(size, pixels, seed, sha256, None)
}

/// Like [`generate`], with the land biomes a datadisk paints taking the place of the derived ones.
pub fn generate_with(
    size: usize,
    pixels: &[u16],
    seed: &str,
    sha256: &str,
    painted: Option<&BiomeMap>,
) -> Option<Environment> {
    if size == 0 || size > 1024 || pixels.len() != size * size * 6 {
        return None;
    }
    let resolution = size.min(MAX_FACE_SIZE);
    let count = resolution * resolution * 6;
    let mut elevation_m = Vec::with_capacity(count);
    for face in 0..6 {
        for row in 0..resolution {
            for column in 0..resolution {
                let source_row = ((row * 2 + 1) * size / (resolution * 2)).min(size - 1);
                let source_column = ((column * 2 + 1) * size / (resolution * 2)).min(size - 1);
                let value = pixels[source_row * size * 6 + face * size + source_column];
                elevation_m.push(crate::terrain::metres(value).round() as i32);
            }
        }
    }
    let mut filled = elevation_m.clone();
    let mut visited = vec![false; count];
    let mut downstream = vec![-1; count];
    let mut queue = BinaryHeap::new();
    for index in 0..count {
        if elevation_m[index] <= 0 {
            visited[index] = true;
            queue.push(Reverse((0, index)));
            filled[index] = 0;
        }
    }
    if queue.is_empty() {
        let index = (0..count).min_by_key(|&index| elevation_m[index])?;
        visited[index] = true;
        queue.push(Reverse((elevation_m[index], index)));
    }
    let mut order = Vec::with_capacity(count);
    while let Some(Reverse((level, index))) = queue.pop() {
        order.push(index);
        for (horizontal, vertical) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            let adjacent = neighbor(index, horizontal, vertical, resolution);
            if visited[adjacent] {
                continue;
            }
            visited[adjacent] = true;
            filled[adjacent] = elevation_m[adjacent].max(level);
            downstream[adjacent] = index as i32;
            queue.push(Reverse((filled[adjacent], adjacent)));
        }
    }
    let mut flow = vec![1u32; count];
    for &index in order.iter().rev() {
        if downstream[index] >= 0 {
            flow[downstream[index] as usize] += flow[index];
        }
    }
    let mut water_m = vec![DRY; count];
    let mut biomes = vec![0; count];
    let numeric_seed = seed.parse::<u64>().ok()?;
    let is_water = |index: usize| {
        elevation_m[index] <= 0
            || filled[index] > elevation_m[index] + 32
            || (flow[index] >= 18 && downstream[index] >= 0)
    };
    for index in 0..count {
        let face = index / (resolution * resolution);
        let horizontal =
            (index % resolution) as f64 * 2.0 / resolution as f64 + 1.0 / resolution as f64 - 1.0;
        let vertical = 1.0
            - ((index / resolution) % resolution) as f64 * 2.0 / resolution as f64
            - 1.0 / resolution as f64;
        let point = cube(face, horizontal, vertical);
        let latitude = point[1]
            * (1.0 - point[0].powi(2) / 2.0 - point[2].powi(2) / 2.0
                + point[0].powi(2) * point[2].powi(2) / 3.0)
                .sqrt();
        let elevation = elevation_m[index];
        let derived = if elevation <= 0 {
            water_m[index] = 0;
            0
        } else if filled[index] > elevation + 32 {
            water_m[index] = filled[index];
            1
        } else if flow[index] >= 18 && downstream[index] >= 0 {
            water_m[index] = filled[index];
            2
        } else {
            let near_water =
                [(1, 0), (-1, 0), (0, 1), (0, -1)]
                    .into_iter()
                    .any(|(horizontal, vertical)| {
                        is_water(neighbor(index, horizontal, vertical, resolution))
                    });
            land_biome(numeric_seed, point, latitude, elevation, near_water)
        };
        // A datadisk paints land only; water and the rivers stay what the relief makes them.
        biomes[index] = match painted.and_then(|map| map.at(point)) {
            Some(biome) if (3..=7).contains(&derived) => biome,
            _ => derived,
        };
    }
    Some(Environment {
        version: 3,
        heightmap_sha256: sha256.to_owned(),
        seed: seed.to_owned(),
        face_size: resolution,
        elevation_m,
        water_m,
        downstream,
        flow,
        biomes,
    })
}

#[cfg(test)]
pub fn spawn_position(size: usize, pixels: &[u16], seed: &str, sha256: &str) -> Option<[f64; 3]> {
    spawn_position_with(size, pixels, seed, sha256, None)
}

pub fn spawn_position_with(
    size: usize,
    pixels: &[u16],
    seed: &str,
    sha256: &str,
    painted: Option<&BiomeMap>,
) -> Option<[f64; 3]> {
    let environment = generate_with(size, pixels, seed, sha256, painted)?;
    let resolution = environment.face_size;
    let radius = 6_371_000.0;
    (0..environment.biomes.len())
        .filter_map(|index| {
            let column = index % resolution;
            let row = (index / resolution) % resolution;
            if column == 0 || row == 0 || column + 1 == resolution || row + 1 == resolution {
                return None;
            }
            for vertical in -1..=1 {
                for horizontal in -1..=1 {
                    let adjacent = neighbor(index, horizontal, vertical, resolution);
                    if !matches!(environment.biomes[adjacent], 4 | 5)
                        || environment.water_m[adjacent] != DRY
                    {
                        return None;
                    }
                }
            }
            let face = index / (resolution * resolution);
            let source_column = (column * 2 + 1) * size / (resolution * 2);
            let source_row = (row * 2 + 1) * size / (resolution * 2);
            let mut minimum = 8000.0_f64;
            let mut maximum = -8000.0_f64;
            for sample_row in source_row.saturating_sub(1)..=(source_row + 1).min(size - 1) {
                for sample_column in
                    source_column.saturating_sub(1)..=(source_column + 1).min(size - 1)
                {
                    let height = crate::terrain::metres(
                        pixels[sample_row * size * 6 + face * size + sample_column],
                    );
                    minimum = minimum.min(height);
                    maximum = maximum.max(height);
                }
            }
            if minimum < 150.0
                || maximum > 1600.0
                || (maximum - minimum) / (radius / size as f64) > 0.2
            {
                return None;
            }
            let horizontal = (column as f64 + 0.5) * size as f64 / resolution as f64 - 0.5;
            let vertical = (row as f64 + 0.5) * size as f64 / resolution as f64 - 0.5;
            let height_at = |sample_column: usize, sample_row: usize| {
                crate::terrain::metres(pixels[sample_row * size * 6 + face * size + sample_column])
            };
            let left = horizontal.floor() as usize;
            let top = vertical.floor() as usize;
            let fraction_x = horizontal.fract();
            let fraction_y = vertical.fract();
            let upper =
                height_at(left, top) * (1.0 - fraction_x) + height_at(left + 1, top) * fraction_x;
            let lower = height_at(left, top + 1) * (1.0 - fraction_x)
                + height_at(left + 1, top + 1) * fraction_x;
            let elevation = upper * (1.0 - fraction_y) + lower * fraction_y;
            let point = cube(
                face,
                (column as f64 + 0.5) * 2.0 / resolution as f64 - 1.0,
                1.0 - (row as f64 + 0.5) * 2.0 / resolution as f64,
            );
            let direction = [
                point[0]
                    * (1.0 - point[1].powi(2) / 2.0 - point[2].powi(2) / 2.0
                        + point[1].powi(2) * point[2].powi(2) / 3.0)
                        .sqrt(),
                point[1]
                    * (1.0 - point[0].powi(2) / 2.0 - point[2].powi(2) / 2.0
                        + point[0].powi(2) * point[2].powi(2) / 3.0)
                        .sqrt(),
                point[2]
                    * (1.0 - point[0].powi(2) / 2.0 - point[1].powi(2) / 2.0
                        + point[0].powi(2) * point[1].powi(2) / 3.0)
                        .sqrt(),
            ];
            Some(direction.map(|coordinate| coordinate * (radius + elevation)))
        })
        .choose(&mut OsRng)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terrain::legacy_sample;

    #[test]
    fn land_biomes_form_coherent_regions_with_both_vegetation_kinds() {
        let size = 32;
        let pixels = vec![legacy_sample(150); size * size * 6];
        let environment = generate(size, &pixels, "42", "test").unwrap();
        let biomes = &environment.biomes;
        assert!(biomes.contains(&4) && biomes.contains(&5), "{biomes:?}");
        let resolution = environment.face_size;
        let mut pairs = 0;
        let mut differing = 0;
        for index in 0..biomes.len() {
            if index % resolution + 1 < resolution {
                pairs += 1;
                differing += usize::from(biomes[index] != biomes[index + 1]);
            }
        }
        assert!(
            differing * 6 < pairs,
            "neighbouring cells should mostly agree: {differing} of {pairs}"
        );
        let again = generate(size, &pixels, "42", "test").unwrap();
        assert_eq!(again.biomes, biomes.clone());
        let other = generate(size, &pixels, "43", "test").unwrap();
        assert_ne!(other.biomes, *biomes);
    }

    fn pgm(width: usize, height: usize, pixels: &[u8]) -> Vec<u8> {
        let mut bytes = format!("P5\n# painted\n{width} {height}\n255\n").into_bytes();
        bytes.extend_from_slice(pixels);
        bytes
    }

    #[test]
    fn a_biome_map_reads_pgm_and_rejects_bad_values() {
        let map = BiomeMap::from_pgm(&pgm(4, 2, &[0, 0, 5, 0, 0, 0, 5, 0])).unwrap();
        assert_eq!((map.width, map.height), (4, 2));
        assert!(BiomeMap::from_pgm(&pgm(4, 2, &[0, 1, 0, 0, 0, 0, 0, 0])).is_err());
        assert!(BiomeMap::from_pgm(&pgm(4, 2, &[0, 8, 0, 0, 0, 0, 0, 0])).is_err());
        assert!(BiomeMap::from_pgm(&pgm(4, 3, &[0; 12])).is_err());
        assert!(BiomeMap::from_pgm(&pgm(4, 2, &[0; 7])).is_err());
        assert!(BiomeMap::from_pgm(b"P6\n4 2\n255\n").is_err());
    }

    #[test]
    fn a_biome_map_is_read_in_the_frame_of_the_world_rasters() {
        // Only the column of 0..90 E is painted. East of Greenwich is towards -Z.
        let map = BiomeMap::from_pgm(&pgm(4, 2, &[0, 0, 5, 0, 0, 0, 5, 0])).unwrap();
        assert_eq!(map.at([1.0, 0.0, -1.0]), Some(5));
        assert_eq!(map.at([1.0, 0.0, 1.0]), None);
        assert_eq!(map.at([0.0, 0.0, 0.0]), None);
    }

    #[test]
    fn painted_biomes_replace_land_biomes_but_never_water() {
        let size = 16;
        let mut pixels = vec![legacy_sample(144); size * size * 6];
        // The first cells of the first face are sea.
        for value in pixels.iter_mut().take(size * 4) {
            *value = 0;
        }
        let plain = generate(size, &pixels, "42", "test").unwrap();
        let polar = BiomeMap::from_pgm(&pgm(4, 2, &[7; 8])).unwrap();
        let painted = generate_with(size, &pixels, "42", "test", Some(&polar)).unwrap();
        assert_eq!(plain.biomes.len(), painted.biomes.len());
        assert!(plain.biomes.contains(&0));
        for (before, after) in plain.biomes.iter().zip(&painted.biomes) {
            if (3..=7).contains(before) {
                assert_eq!(*after, 7);
            } else {
                assert_eq!(after, before, "water is not painted");
            }
        }
        assert_eq!(plain.water_m, painted.water_m);
    }

    #[test]
    fn spawns_only_on_dry_temperate_lowlands() {
        let size = 16;
        let pixels = vec![legacy_sample(144); size * size * 6];
        for _ in 0..32 {
            let position = spawn_position(size, &pixels, "42", "test").unwrap();
            let radius = position.iter().map(|axis| axis * axis).sum::<f64>().sqrt();
            let base = crate::terrain::metres(legacy_sample(144));
            assert!((radius - (6_371_000.0 + base)).abs() < 0.000001);
            assert!(position[1].abs() / radius + (radius - 6_371_000.0) / 6000.0 <= 0.72);
        }
        for value in [0, 127, 128, 160, 255].map(legacy_sample) {
            assert!(spawn_position(size, &vec![value; size * size * 6], "42", "test").is_none());
        }
    }

    #[test]
    fn spawns_away_from_ocean_and_lakes() {
        let size = 16;
        let mut pixels = vec![legacy_sample(144); size * size * 6];
        for row in 0..size {
            pixels[row * size * 6..row * size * 6 + size].fill(legacy_sample(80));
        }
        pixels[8 * size * 6 + 4 * size + 8] = legacy_sample(135);
        let environment = generate(size, &pixels, "42", "test").unwrap();
        assert!(environment.biomes.contains(&0));
        assert!(environment.biomes.contains(&1));
        for _ in 0..32 {
            let position = spawn_position(size, &pixels, "42", "test").unwrap();
            assert!(position[0] < position[1].abs().max(position[2].abs()));
            let radius = position.iter().map(|axis| axis * axis).sum::<f64>().sqrt();
            let base = crate::terrain::metres(legacy_sample(144));
            assert!((radius - (6_371_000.0 + base)).abs() < 0.000001);
            if position[2] > position[0].abs().max(position[1].abs()) {
                let horizontal = position[0] / radius;
                let vertical = position[1] / radius;
                assert!(
                    !(-0.045..=0.134).contains(&horizontal)
                        || !(-0.134..=0.045).contains(&vertical)
                );
            }
        }
    }

    #[test]
    fn cube_neighbors_connect_faces_bidirectionally() {
        let size = 8;
        for index in 0..size * size * 6 {
            for (horizontal, vertical) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                let adjacent = neighbor(index, horizontal, vertical, size);
                assert_ne!(index, adjacent);
                assert!([(1, 0), (-1, 0), (0, 1), (0, -1)].iter().any(
                    |&(horizontal, vertical)| neighbor(adjacent, horizontal, vertical, size)
                        == index
                ));
            }
        }
    }

    #[test]
    fn fills_lakes_routes_rivers_and_never_cycles() {
        let size = 8;
        let mut pixels = vec![legacy_sample(160); size * size * 6];
        pixels[0] = legacy_sample(80);
        pixels[4 * size * 6 + 4] = legacy_sample(140);
        let map = generate(size, &pixels, "42", "test").unwrap();
        assert_eq!(map, generate(size, &pixels, "42", "test").unwrap());
        assert!(map.biomes.contains(&0));
        assert!(map.biomes.contains(&1));
        assert!(map.biomes.contains(&2));
        for start in 0..map.downstream.len() {
            let mut index = start;
            let mut steps = 0;
            while map.downstream[index] >= 0 {
                let target = map.downstream[index] as usize;
                assert!(
                    map.elevation_m[target].max(map.water_m[target])
                        <= map.elevation_m[index].max(map.water_m[index])
                );
                index = target;
                steps += 1;
                assert!(steps < map.downstream.len());
            }
        }
    }

    #[test]
    fn bounds_work_and_rejects_invalid_maps() {
        assert!(generate(0, &[], "42", "test").is_none());
        assert!(generate(1025, &[], "42", "test").is_none());
        assert!(generate(1, &[0; 6], "bad", "test").is_none());
        let map = generate(128, &vec![legacy_sample(180); 128 * 128 * 6], "42", "test").unwrap();
        assert_eq!(map.face_size, MAX_FACE_SIZE);
        assert_eq!(map.biomes.len(), MAX_FACE_SIZE * MAX_FACE_SIZE * 6);
        assert!(!map.biomes.contains(&0));
    }
}
