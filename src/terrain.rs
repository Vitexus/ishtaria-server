//! Height maps: reading, validating and hashing the imported faces of the planet.

use anyhow::{bail, Context, Result};
use image::{ColorType, ImageFormat, ImageReader, Limits};
use sha2::{Digest, Sha256};
use std::io::Cursor;

pub const MAX_MAP_BYTES: u64 = 16 * 1024 * 1024;

/// Six square faces of 16-bit height samples: `FLOOR_M` plus `STEP_M` per sample (see the
/// worldgen `strip` module, which writes them with the relief already in).
pub const FLOOR_M: f64 = -8000.0;
pub const STEP_M: f64 = 0.25;

pub struct Heightmap {
    pub face_size: i32,
    pub pixels: Vec<u16>,
    pub sha256: String,
}

/// Height in metres of one strip sample.
pub fn metres(sample: u16) -> f64 {
    f64::from(sample) * STEP_M + FLOOR_M
}

/// The strip sample whose height equals the old 8-bit encoding of `value` (for test fixtures).
#[cfg(test)]
pub fn legacy_sample(value: u8) -> u16 {
    let metres = f64::from(value) * 16000.0 / 255.0 - 8000.0;
    ((metres - FLOOR_M) / STEP_M).round() as u16
}

/// The samples of a stored strip, read back from their two-byte big-endian form.
pub fn samples_from_bytes(bytes: &[u8]) -> Vec<u16> {
    bytes
        .chunks_exact(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect()
}

/// The samples as stored in the database: two bytes each, big-endian, like the PGM.
pub fn sample_bytes(pixels: &[u16]) -> Vec<u8> {
    pixels
        .iter()
        .flat_map(|sample| sample.to_be_bytes())
        .collect()
}

impl Heightmap {
    pub fn parse(pgm: &[u8]) -> Result<Self> {
        if pgm.len() as u64 > MAX_MAP_BYTES || !pgm.starts_with(b"P5") {
            bail!("expected a binary P5 PGM no larger than 16 MiB");
        }
        let mut reader = ImageReader::with_format(Cursor::new(pgm), ImageFormat::Pnm);
        let mut limits = Limits::default();
        limits.max_image_width = Some(6144);
        limits.max_image_height = Some(1024);
        limits.max_alloc = Some(64 * 1024 * 1024);
        reader.limits(limits);
        let image = reader.decode().context("invalid PGM heightmap")?;
        let face_size = image.height();
        if face_size == 0 || image.width() != face_size * 6 || image.color() != ColorType::L16 {
            bail!("heightmap must contain six square faces in a 16-bit grayscale strip");
        }
        Ok(Self {
            face_size: face_size as i32,
            pixels: image.into_luma16().into_raw(),
            sha256: format!("{:x}", Sha256::digest(pgm)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_binary_samples_including_whitespace() {
        let pgm = b"P5\n# cube faces\n6 1\n65535\n\x0a\x20\x00\x80\xfe\xff\x00\x01\x7d\x00\x00\x09";
        let map = Heightmap::parse(pgm).unwrap();
        assert_eq!(map.face_size, 1);
        assert_eq!(map.pixels, [0x0a20, 0x0080, 0xfeff, 0x0001, 0x7d00, 0x0009]);
        assert_eq!(map.sha256.len(), 64);
        assert_eq!(sample_bytes(&map.pixels)[..4], [0x0a, 0x20, 0x00, 0x80]);
    }

    #[test]
    fn samples_decode_to_metres() {
        assert_eq!(metres(32000), 0.0);
        assert_eq!(metres(0), -8000.0);
        assert_eq!(metres(32004), 1.0);
    }

    #[test]
    fn rejects_invalid_maps() {
        for pgm in [
            b"P5\n6 1\n255\n\x00".as_slice(),
            b"P5\n1 1\n255\n\x00".as_slice(),
            b"P2\n6 1\n255\n0 0 0 0 0 0".as_slice(),
            b"P5\n6 1\n255\n\x00\x00\x00\x00\x00\x00".as_slice(),
        ] {
            assert!(Heightmap::parse(pgm).is_err());
        }
    }
}
