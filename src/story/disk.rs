//! Loading and validating story datadisks (see `ishtaria-protocol/schemas/datadisk.schema.json`).
//!
//! A datadisk is a directory of YAML files. Every id is local to its disk in the files and
//! becomes `<disk>:<id>` once several disks are composed into one [`Story`].

use anyhow::{anyhow, bail, ensure, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

/// Biome names a datadisk may require, as numbered by `environment::generate`.
pub const BIOMES: [(&str, u8); 8] = [
    ("ocean", 0),
    ("lake", 1),
    ("river", 2),
    ("lowland", 3),
    ("grassland", 4),
    ("temperate_forest", 5),
    ("mountain", 6),
    ("polar", 7),
];

const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_FILES: usize = 512;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // Mirrors the file format; not every field is used by the server yet.
pub struct Manifest {
    pub id: String,
    pub version: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub requires_ruleset: String,
    pub license: String,
    pub attribution: String,
    #[serde(default)]
    pub rating: Option<String>,
    /// Image below `media/` that clients show when they connect to a server using the disk.
    #[serde(default)]
    pub cover: Option<String>,
    /// Track id from `music/*.yaml` played once while the cover is shown (logo jingle).
    #[serde(default)]
    pub cover_music: Option<String>,
    /// Track id played as the default background outside every place music radius.
    #[serde(default)]
    pub ambient_music: Option<String>,
    /// The character every new player of a world using this disk plays (`<pack>/<skin>` as in the
    /// character creator); players then choose only their nickname. Without it they choose a look.
    #[serde(default)]
    pub player_character: Option<String>,
    /// A glTF binary below `media/models/` (with `Idle` and `Walk` clips) that clients draw for that
    /// character instead of the pack look, which stays the fallback. Needs `player_character`.
    #[serde(default)]
    pub player_model: Option<String>,
    /// The body of that character: how tall it stands and how much more than a man it endures. Needs
    /// `player_character`.
    #[serde(default)]
    pub player_body: Option<PlayerBody>,
    #[serde(default = "default_languages")]
    pub languages: Vec<String>,
    #[serde(default)]
    pub requires: Vec<Requirement>,
    #[serde(default)]
    pub conflicts: Vec<String>,
    /// The real planet a world using this disk is generated on. Only one selected
    /// disk may define it; `ishtaria-worldgen --earth` strips it onto the cube-sphere.
    #[serde(default)]
    pub world: Option<World>,
}

/// The real planet of a datadisk: equirectangular relief and water mask that
/// replace the procedural noise when a world is generated with the disk.
#[derive(Clone, Debug, Deserialize, serde::Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct World {
    pub terrain: WorldTerrain,
}

/// Two P5 PGMs of the same size (`width == 2 * height`, row 0 at +90 degrees):
/// `heightmap` is 16-bit elevation in metres (0 is sea level, land positive,
/// bathymetry negative), `water` an 8-bit mask (0 land, 255 water) that decides
/// the coastline, holds the rivers and may reconstruct historical shorelines.
#[derive(Clone, Debug, Deserialize, serde::Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WorldTerrain {
    pub heightmap: String,
    pub water: String,
}

/// The body of the character a datadisk gives its players (a demigod is taller and tougher than a man).
#[derive(Clone, Copy, Debug, Deserialize, serde::Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PlayerBody {
    /// Height in metres; the character creator's people are 1.8 m.
    pub height_m: f64,
    /// How many times less a fall hurts than it would hurt a man of the same size (1 = as a man).
    #[serde(default = "default_toughness")]
    pub toughness: f64,
}

fn default_toughness() -> f64 {
    1.0
}

/// Height of the people of the character creator, in metres.
pub const MAN_HEIGHT_M: f64 = 1.8;

fn default_languages() -> Vec<String> {
    vec!["en".into()]
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requirement {
    pub disk: String,
    pub version: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Near {
    pub place: String,
    pub min_m: f64,
    pub max_m: f64,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeightRange {
    pub min: Option<f64>,
    pub max: Option<f64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requires {
    #[serde(default)]
    pub biome: Vec<String>,
    pub height_m: Option<HeightRange>,
    pub max_slope_deg: Option<f64>,
    /// `true`: the sea is within a few hundred metres (a harbour place); `false`: it is not.
    pub coast: Option<bool>,
    #[serde(default)]
    pub near: Vec<Near>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Place {
    pub id: String,
    pub kind: String,
    pub name_key: String,
    pub parent: Option<String>,
    pub radius_m: Option<f64>,
    /// Minimum distance to other places of the same kind.
    pub spacing_m: Option<f64>,
    /// Buildings and props the engine builds at the place.
    pub scenery: Option<Scenery>,
    /// A model of the datadisk standing at the place (a building the engine does not generate).
    pub structure: Option<Structure>,
    /// A landscape patch anchored at the place (ADR 0013): real-world relief around the site.
    pub terrain: Option<Terrain>,
    /// New characters of the world appear here (the first such place of the first disk wins).
    #[serde(default)]
    pub spawn: bool,
    /// A fixed spot of the parent's walled town: `gate` or `alley` (see `scenery::town_spot`).
    pub at: Option<String>,
    /// Where on the planet the place lies, from another place: the real geography of a story.
    pub locate: Option<Locate>,
    /// The historical site itself on the real planet of the disk, when the disk defines one.
    pub geo: Option<Geo>,
    /// Track (id of the disk's music) that plays while the player is within `music_radius_m`.
    pub music: Option<String>,
    /// Distance from the place where its music is heard; defaults to `radius_m`.
    pub music_radius_m: Option<f64>,
    /// Historical flora and fauna catalog weights inside `wildlife.radius_m`.
    pub wildlife: Option<Wildlife>,
    #[serde(default)]
    pub requires: Requires,
}

/// Weighted scenery/fauna catalogs that replace the global biome table near a place.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Wildlife {
    #[serde(default)]
    pub flora: Vec<WildlifeEntry>,
    #[serde(default)]
    pub fauna: Vec<WildlifeEntry>,
    pub radius_m: Option<f64>,
    pub density: Option<f64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WildlifeEntry {
    pub id: String,
    pub weight: f64,
}

/// A place put where history says: `distance_m` from `from` in the direction `bearing_deg` of the map of `from`
/// (the frame of its landscape patch and buildings: 0 is north, up the map, 90 is east). The site is the nearest
/// that satisfies the `requires` of the place within `tolerance_m` of that point, since the planet is not
/// the Earth and its coasts and mountains lie elsewhere. A `tolerance_m` of zero is a surveyed point: the
/// place stands exactly there (its `requires` are advice for the author, logged at placement).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Locate {
    pub from: String,
    pub bearing_deg: f64,
    pub distance_m: f64,
    /// How far from the exact point a site may lie; default a twentieth of the distance (at least 30 m).
    /// Zero means the survey decided: no search, the place stands exactly at the point.
    pub tolerance_m: Option<f64>,
}

impl Locate {
    pub fn tolerance(&self) -> f64 {
        self.tolerance_m
            .unwrap_or((self.distance_m / 20.0).max(30.0))
    }
}

/// A place standing at its historical site on the real planet the disk defines (`world.terrain`):
/// `latitude_deg` and `longitude_deg` in the frame of the rasters (`latitude = asin(y)`,
/// `longitude = atan2(z, x)`). The place stands exactly there: history has already decided,
/// so `requires` is advice for the author (its violations are logged at placement), not a
/// constraint to search around. Places around it locate themselves from it as usual.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Geo {
    pub latitude_deg: f64,
    pub longitude_deg: f64,
}

impl Geo {
    /// Unit direction of the site, in the frame shared with `ishtaria-worldgen --earth`.
    pub fn direction(&self) -> [f64; 3] {
        let latitude = self.latitude_deg.to_radians();
        let longitude = self.longitude_deg.to_radians();
        [
            latitude.cos() * longitude.cos(),
            latitude.sin(),
            latitude.cos() * longitude.sin(),
        ]
    }
}

/// A generated layout of models from the bundled Kenney kits.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenery {
    /// `town` (houses, plaza, walls; harbour when by the sea), `graveyard` or `fortress`.
    pub preset: String,
    /// `hamlet`, `village` or `town` (presets that build settlements).
    pub size: Option<String>,
}

/// A building of the datadisk itself: a glTF binary drawn at the place and walkable by its mesh.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Structure {
    /// A `.glb` below `media/models/` in metres, standing on the ground at its origin, +Y up.
    pub model: String,
    /// A simpler `.glb` used for collision and walking; defaults to `model`.
    pub collision: Option<String>,
    /// Metres per model unit (default 1).
    pub scale_m: Option<f64>,
    /// Turn about the vertical axis in degrees (default 0).
    pub yaw_deg: Option<f64>,
    /// Shift from the place's anchor, in metres east and north.
    pub offset_m: Option<[f64; 2]>,
    /// The point of the model (x and z in model units, before the turn) that stands on the ground, such as the foot of
    /// its stairs; the model's height is set by the terrain there. Defaults to the model's origin.
    pub ground_m: Option<[f64; 2]>,
    /// Distance up to which the server announces the structure (default 600).
    pub visible_m: Option<f64>,
}

/// A landscape patch of a datadisk place (ADR 0013): real-world relief around the site, with the
/// river returned to its historical course. Heights are relative to the anchor's placed height.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Terrain {
    /// A 16-bit greyscale `.png` below `media/terrain/`; 0x8000 is the anchor's height, other
    /// pixels decimetres above it.
    pub heightmap: String,
    /// An equally sized `.png`: 0 is dry land, other pixels the water surface in decimetres
    /// above the anchor (0x8000 = the anchor).
    pub water: Option<String>,
    /// A `.png` (any size, 8 or 16 bit greyscale) over the whole rectangle: pixels above zero are
    /// built or tilled ground (streets, houses, squares, fields) where no natural scenery stands.
    pub clear: Option<String>,
    /// The size of the rectangle in the client's local x and z frame, centred on `offset_m`.
    pub extent_m: [f64; 2],
    /// The centre of the rectangle from the anchor, in metres along the local x and z axes.
    pub offset_m: Option<[f64; 2]>,
    /// Width of the margin blending the patch into the base terrain (default 100).
    pub feather_m: Option<f64>,
    /// Distance from the patch edge up to which the server announces it (default 1200).
    pub visible_m: Option<f64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Character {
    pub pack: String,
    pub skin: String,
    /// A glTF binary below `media/models/` drawn instead of the pack character;
    /// `pack`/`skin` stay the fallback when a client cannot load it.
    pub model: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // Mirrors the file format; not every field is used by the server yet.
pub struct Npc {
    pub id: String,
    pub name_key: String,
    pub place: String,
    pub character: Character,
    pub dialogue: String,
    pub bio_key: Option<String>,
    /// Image below `media/` shown beside the dialogue text.
    pub portrait: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// How many times taller than the model as authored the character stands (a god towers over men).
    pub size: Option<f64>,
    /// A character that walks instead of standing at its place.
    pub movement: Option<Movement>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MovementKind {
    /// Walks to spots near its place, standing between walks.
    Wander,
    /// Walks the loop of `waypoints`.
    Patrol,
    /// Drives a flock from its fold (the place) to the one pasture in `waypoints` and home again (`herd`). Only the
    /// settlements generated for a world have shepherds; a datadisk cannot ask for this.
    #[serde(skip_deserializing)]
    Herd,
}

/// How a character of a datadisk walks (see `story::walking`).
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Movement {
    pub kind: MovementKind,
    /// How far from its place a wanderer goes.
    #[serde(default = "default_radius")]
    pub radius_m: f64,
    #[serde(default = "default_speed")]
    pub speed_m_s: f64,
    /// How long it stands between walks.
    #[serde(default = "default_pause")]
    pub pause_s: f64,
    /// The loop of a patrol: metres east and south of the place, as `offset_m` of a building.
    #[serde(default)]
    pub waypoints: Vec<[f64; 2]>,
    /// How far into its cycle a shepherd's flock is at time zero (seconds); a herd only.
    #[serde(default)]
    pub phase_s: f64,
}

fn default_radius() -> f64 {
    25.0
}

fn default_speed() -> f64 {
    1.0
}

fn default_pause() -> f64 {
    6.0
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestStageRef {
    pub quest: String,
    pub stage: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemQty {
    pub item: String,
    pub qty: Option<i64>,
}

impl ItemQty {
    pub fn quantity(&self) -> i64 {
        self.qty.unwrap_or(1)
    }
}

/// A structured condition: exactly one field is set.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Condition {
    pub all: Option<Vec<Condition>>,
    pub any: Option<Vec<Condition>>,
    pub not: Option<Box<Condition>>,
    pub flag: Option<String>,
    pub quest_stage: Option<QuestStageRef>,
    pub has_item: Option<ItemQty>,
    pub gold_at_least: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Once {
    pub key: String,
    pub effects: Vec<Effect>,
}

/// A structured effect: exactly one field is set.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Effect {
    pub give_item: Option<ItemQty>,
    pub take_item: Option<ItemQty>,
    pub gold: Option<i64>,
    pub set_flag: Option<String>,
    pub set_stage: Option<QuestStageRef>,
    pub once: Option<Once>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    pub text_key: String,
    #[serde(rename = "if")]
    pub condition: Option<Condition>,
    #[serde(default)]
    pub effects: Vec<Effect>,
    pub goto: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Branch {
    #[serde(rename = "if")]
    pub condition: Option<Condition>,
    pub goto: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    pub text_key: Option<String>,
    pub branch: Option<Vec<Branch>>,
    pub choices: Option<Vec<Choice>>,
    #[serde(default)]
    pub effects: Vec<Effect>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // Mirrors the file format; not every field is used by the server yet.
pub struct Dialogue {
    pub id: String,
    pub status: Option<String>,
    pub generated_by: Option<String>,
    /// Track from `music/*.yaml` played during the conversation.
    pub music: Option<String>,
    pub start: String,
    pub nodes: BTreeMap<String, Node>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stage {
    pub text_key: String,
    #[serde(default)]
    pub final_stage: bool,
    pub reach: Option<Reach>,
    /// Place the player is pointed to while this stage is current (defaults to the reach place).
    pub guide: Option<String>,
}

/// Moves the quest on when the player comes within `radius_m` of a place.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reach {
    pub place: String,
    pub radius_m: f64,
    pub goto: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageFile {
    pub text_key: String,
    #[serde(rename = "final", default)]
    pub final_stage: bool,
    pub reach: Option<Reach>,
    pub guide: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestFile {
    pub id: String,
    pub title_key: String,
    pub start_stage: String,
    /// Place where the quest begins: shown as the next story point until the quest is started.
    pub start_place: Option<String>,
    pub stages: BTreeMap<String, StageFile>,
}

#[derive(Clone, Debug)]
pub struct Quest {
    pub id: String,
    pub title_key: String,
    pub start_stage: String,
    pub start_place: Option<String>,
    pub stages: BTreeMap<String, Stage>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // Mirrors the file format; not every field is used by the server yet.
pub struct Lore {
    pub id: String,
    pub category: Option<String>,
    pub title_key: String,
    pub text_key: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Music {
    pub id: String,
    pub file: String,
    pub title_key: String,
    #[serde(rename = "loop", default)]
    pub looped: bool,
}

/// Largest portrait / track a datadisk may ship.
pub const MAX_PORTRAIT_BYTES: u64 = 1 << 20;
pub const MAX_MUSIC_BYTES: u64 = 8 << 20;
/// Largest character model (`.glb`) a datadisk may ship.
pub const MAX_MODEL_BYTES: u64 = 4 << 20;
/// Largest landscape patch (16-bit `.png`) a datadisk may ship.
pub const MAX_TERRAIN_BYTES: u64 = 4 << 20;
/// Largest equirectangular world raster (16-bit `.pgm`) a datadisk may ship.
pub const MAX_WORLD_TERRAIN_BYTES: u64 = 160 << 20;
/// Largest spoken line and most spoken lines per language a datadisk may ship.
pub const MAX_VOICE_BYTES: u64 = 2 << 20;
pub const MAX_VOICES_PER_LANGUAGE: usize = 4000;

/// Content type for a media path, by extension (the only types a disk may ship).
pub fn media_type(path: &str) -> Option<&'static str> {
    match path.rsplit_once('.')?.1 {
        "png" => Some("image/png"),
        "jpg" => Some("image/jpeg"),
        "ogg" => Some("audio/ogg"),
        "glb" => Some("model/gltf-binary"),
        "pgm" => Some("image/x-portable-graymap"),
        _ => None,
    }
}

/// Parses the header of an equirectangular world raster (a binary P5 PGM of
/// `width == 2 * height`, the `maximum` maxval) and checks that the file holds
/// the whole raster. Only the header is read: `ishtaria-worldgen --earth`, not
/// the server, consumes the pixels.
fn world_pgm_header(path: &Path, maximum: u16) -> Result<(usize, usize)> {
    let name = path.display();
    let mut file = fs::File::open(path).with_context(|| format!("cannot read {name}"))?;
    let mut head = [0u8; 128];
    let read =
        std::io::Read::read(&mut file, &mut head).with_context(|| format!("cannot read {name}"))?;
    let length = fs::metadata(path)?.len();
    let mut cursor = 0usize;
    let mut tokens = Vec::with_capacity(4);
    while tokens.len() < 4 && cursor < read {
        if head[cursor] == b'#' {
            while cursor < read && head[cursor] != b'\n' {
                cursor += 1;
            }
            continue;
        }
        if (head[cursor] as char).is_ascii_whitespace() {
            cursor += 1;
            continue;
        }
        let start = cursor;
        while cursor < read && !(head[cursor] as char).is_ascii_whitespace() && head[cursor] != b'#'
        {
            cursor += 1;
        }
        tokens.push(String::from_utf8_lossy(&head[start..cursor]).into_owned());
    }
    let invalid = |message: &str| anyhow!("{name}: {message}");
    if tokens.first().map(String::as_str) != Some("P5") {
        bail!("{}", invalid("expected a binary P5 PGM"));
    }
    let (width, height, maxval): (usize, usize, u16) = match (
        tokens.get(1).and_then(|t| t.parse().ok()),
        tokens.get(2).and_then(|t| t.parse().ok()),
        tokens.get(3).and_then(|t| t.parse().ok()),
    ) {
        (Some(width), Some(height), Some(maxval)) => (width, height, maxval),
        _ => bail!("{}", invalid("invalid PGM header")),
    };
    if width == 0 || height == 0 || width != 2 * height || maxval != maximum {
        bail!(
            "{}",
            invalid(&format!(
                "equirectangular map must be width = 2 x height with maxval {maximum}"
            ))
        );
    }
    let sample = u64::from(maxval > 255) + 1;
    let offset = u64::try_from(cursor + 1).context("raster offset overflow")?;
    ensure!(
        length >= offset + width as u64 * height as u64 * sample,
        "{}",
        invalid("raster is truncated")
    );
    Ok((width, height))
}

/// A media path is `media/<dirs>/<name>.<ext>` of lowercase ASCII, with no traversal.
pub fn is_media_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("media/") else {
        return false;
    };
    media_type(path).is_some()
        && path.len() <= 160
        && !rest.is_empty()
        && rest.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with('.')
                && part.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-')
                })
        })
}

/// One disk exactly as read from its directory (ids still local).
#[derive(Clone, Debug)]
pub struct Disk {
    pub manifest: Manifest,
    pub sha256: String,
    pub places: Vec<Place>,
    pub npcs: Vec<Npc>,
    pub dialogues: Vec<Dialogue>,
    pub quests: Vec<Quest>,
    pub lore: Vec<Lore>,
    pub music: Vec<Music>,
    /// Media files (path below the disk -> absolute file) with their sha256.
    pub media: BTreeMap<String, (PathBuf, String)>,
    /// Spoken lines: (language, local text key) -> media path, from `media/voice/<language>/<key>.ogg`.
    pub voices: BTreeMap<(String, String), String>,
    pub strings: BTreeMap<String, BTreeMap<String, String>>,
}

fn is_id(text: &str, max: usize) -> bool {
    !text.is_empty()
        && text.len() <= max
        && text.starts_with(|c: char| c.is_ascii_lowercase())
        && text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn is_disk_id(text: &str) -> bool {
    text.len() >= 2 && is_id(text, 32)
}

fn yaml_files(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "yaml") && path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn read_limited(path: &Path, hasher: &mut Sha256, root: &Path) -> Result<String> {
    let meta = fs::metadata(path)?;
    ensure!(
        meta.len() <= MAX_FILE_BYTES,
        "{} is larger than {MAX_FILE_BYTES} bytes",
        path.display()
    );
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let relative = path.strip_prefix(root).unwrap_or(path).to_string_lossy();
    hasher.update((relative.len() as u64).to_le_bytes());
    hasher.update(relative.as_bytes());
    hasher.update((text.len() as u64).to_le_bytes());
    hasher.update(text.as_bytes());
    Ok(text)
}

fn parse<T: for<'de> Deserialize<'de>>(path: &Path, text: &str) -> Result<T> {
    serde_yaml::from_str(text).with_context(|| format!("invalid {}", path.display()))
}

impl Disk {
    /// Reads and checks one disk. `allow_draft` admits unreviewed generated dialogues.
    pub fn load(dir: &Path, allow_draft: bool) -> Result<Self> {
        let mut hasher = Sha256::new();
        let manifest_path = dir.join("datadisk.yaml");
        let manifest: Manifest = parse(
            &manifest_path,
            &read_limited(&manifest_path, &mut hasher, dir)?,
        )?;
        ensure!(is_disk_id(&manifest.id), "invalid disk id {}", manifest.id);
        parse_version(&manifest.version)?;
        let mut count = 1;
        let mut load_list = |sub: &str| -> Result<Vec<(PathBuf, String)>> {
            let mut out = Vec::new();
            for path in yaml_files(&dir.join(sub))? {
                count += 1;
                ensure!(count <= MAX_FILES, "too many files in the datadisk");
                let text = read_limited(&path, &mut hasher, dir)?;
                out.push((path, text));
            }
            Ok(out)
        };
        let mut places = Vec::new();
        for (path, text) in load_list("places")? {
            places.extend(parse::<Vec<Place>>(&path, &text)?);
        }
        let mut npcs = Vec::new();
        for (path, text) in load_list("npcs")? {
            npcs.extend(parse::<Vec<Npc>>(&path, &text)?);
        }
        let mut lore = Vec::new();
        for (path, text) in load_list("lore")? {
            lore.extend(parse::<Vec<Lore>>(&path, &text)?);
        }
        let mut music = Vec::new();
        for (path, text) in load_list("music")? {
            music.extend(parse::<Vec<Music>>(&path, &text)?);
        }
        let mut dialogues = Vec::new();
        for (path, text) in load_list("dialogues")? {
            dialogues.push(parse::<Dialogue>(&path, &text)?);
        }
        let mut quests = Vec::new();
        for (path, text) in load_list("quests")? {
            let file: QuestFile = parse(&path, &text)?;
            quests.push(Quest {
                id: file.id,
                title_key: file.title_key,
                start_stage: file.start_stage,
                start_place: file.start_place,
                stages: file
                    .stages
                    .into_iter()
                    .map(|(id, stage)| {
                        (
                            id,
                            Stage {
                                text_key: stage.text_key,
                                final_stage: stage.final_stage,
                                reach: stage.reach,
                                guide: stage.guide,
                            },
                        )
                    })
                    .collect(),
            });
        }
        let mut strings = BTreeMap::new();
        for language in &manifest.languages {
            ensure!(
                language.len() == 2 && language.chars().all(|c| c.is_ascii_lowercase()),
                "invalid language {language}"
            );
            let path = dir.join("i18n").join(format!("{language}.yaml"));
            ensure!(path.is_file(), "i18n/{language}.yaml is missing");
            let text = read_limited(&path, &mut hasher, dir)?;
            let table: BTreeMap<String, String> = parse(&path, &text)?;
            ensure!(
                table.values().all(|value| value.len() <= 2000),
                "{language}: a string is longer than 2000 bytes"
            );
            strings.insert(language.clone(), table);
        }
        // Only files that an NPC or a track names are shipped; they are part of the hash.
        let mut wanted: Vec<&str> = npcs.iter().filter_map(|n| n.portrait.as_deref()).collect();
        wanted.extend(npcs.iter().filter_map(|n| n.character.model.as_deref()));
        for structure in places.iter().filter_map(|place| place.structure.as_ref()) {
            wanted.push(structure.model.as_str());
            wanted.extend(structure.collision.as_deref());
        }
        for terrain in places.iter().filter_map(|place| place.terrain.as_ref()) {
            wanted.push(terrain.heightmap.as_str());
            wanted.extend(terrain.water.as_deref());
            wanted.extend(terrain.clear.as_deref());
        }
        wanted.extend(music.iter().map(|m| m.file.as_str()));
        wanted.extend(manifest.cover.as_deref());
        wanted.extend(manifest.player_model.as_deref());
        let world_files: Vec<&str> = manifest
            .world
            .iter()
            .flat_map(|w| [w.terrain.heightmap.as_str(), w.terrain.water.as_str()])
            .collect();
        wanted.extend(world_files.iter().copied());
        // Spoken lines: a file `media/voice/<language>/<text key>.ogg` for a key a dialogue node shows.
        let spoken: std::collections::BTreeSet<&str> = dialogues
            .iter()
            .flat_map(|d| d.nodes.values())
            .filter_map(|n| n.text_key.as_deref())
            .collect();
        let mut voices = BTreeMap::new();
        let mut voice_paths = Vec::new();
        for language in &manifest.languages {
            let folder = dir.join("media").join("voice").join(language);
            if !folder.is_dir() {
                continue;
            }
            for key in &spoken {
                let relative = format!("media/voice/{language}/{key}.ogg");
                if fs::symlink_metadata(dir.join(&relative)).is_ok() {
                    voices.insert((language.clone(), (*key).to_owned()), relative.clone());
                    voice_paths.push(relative);
                }
            }
            ensure!(
                voices.len() <= MAX_VOICES_PER_LANGUAGE,
                "{language}: too many spoken lines"
            );
        }
        wanted.extend(voice_paths.iter().map(String::as_str));
        wanted.sort_unstable();
        wanted.dedup();
        let mut media = BTreeMap::new();
        for relative in wanted {
            ensure!(is_media_path(relative), "invalid media path {relative}");
            let path = dir.join(relative);
            let limit = if world_files.contains(&relative) {
                MAX_WORLD_TERRAIN_BYTES
            } else if relative.starts_with("media/voice/") {
                MAX_VOICE_BYTES
            } else if relative.starts_with("media/terrain/") {
                MAX_TERRAIN_BYTES
            } else if relative.ends_with(".ogg") {
                MAX_MUSIC_BYTES
            } else if relative.ends_with(".glb") {
                MAX_MODEL_BYTES
            } else {
                MAX_PORTRAIT_BYTES
            };
            let meta = fs::symlink_metadata(&path)
                .with_context(|| format!("media file {relative} is missing"))?;
            ensure!(meta.is_file(), "media {relative} is not a regular file");
            ensure!(
                meta.len() <= limit,
                "media {relative} is larger than {limit} bytes"
            );
            let bytes = fs::read(&path)?;
            let digest = format!("{:x}", Sha256::digest(&bytes));
            hasher.update((relative.len() as u64).to_le_bytes());
            hasher.update(relative.as_bytes());
            hasher.update(digest.as_bytes());
            media.insert(relative.to_owned(), (path, digest));
        }
        let disk = Self {
            manifest,
            sha256: format!("{:x}", hasher.finalize()),
            places,
            npcs,
            dialogues,
            quests,
            lore,
            music,
            media,
            voices,
            strings,
        };
        disk.check(allow_draft)?;
        Ok(disk)
    }

    fn check(&self, allow_draft: bool) -> Result<()> {
        let id = &self.manifest.id;
        if let Some(world) = &self.manifest.world {
            let heightmap = world_pgm_header(
                self.media
                    .get(&world.terrain.heightmap)
                    .map(|(path, _)| path.as_path())
                    .context("the world heightmap is missing from the media list")?,
                65535,
            )
            .with_context(|| format!("{id}: invalid world heightmap"))?;
            let water = world_pgm_header(
                self.media
                    .get(&world.terrain.water)
                    .map(|(path, _)| path.as_path())
                    .context("the world water mask is missing from the media list")?,
                255,
            )
            .with_context(|| format!("{id}: invalid world water mask"))?;
            ensure!(
                heightmap == water,
                "{id}: the world water mask must match the heightmap size {}x{}",
                heightmap.0,
                heightmap.1
            );
        }
        let places: HashSet<&str> = unique(self.places.iter().map(|p| p.id.as_str()), "place")?;
        unique(self.npcs.iter().map(|n| n.id.as_str()), "npc")?;
        let dialogues: HashSet<&str> =
            unique(self.dialogues.iter().map(|d| d.id.as_str()), "dialogue")?;
        let quests: HashMap<&str, &Quest> =
            self.quests.iter().map(|q| (q.id.as_str(), q)).collect();
        ensure!(
            quests.len() == self.quests.len(),
            "{id}: duplicate quest id"
        );
        let local = |reference: &str| !reference.contains(':');
        let key_ok = |key: &str| -> Result<()> {
            for (language, table) in &self.strings {
                ensure!(
                    table.contains_key(key),
                    "{id}: text key {key} missing in {language}"
                );
            }
            Ok(())
        };
        for place in &self.places {
            ensure!(is_id(&place.id, 48), "{id}: invalid place id {}", place.id);
            key_ok(&place.name_key)?;
            if let Some(parent) = &place.parent {
                ensure!(
                    !local(parent) || places.contains(parent.as_str()),
                    "{id}: place {} has unknown parent {parent}",
                    place.id
                );
            }
            if let Some(locate) = &place.locate {
                ensure!(
                    !local(&locate.from)
                        || (places.contains(locate.from.as_str()) && locate.from != place.id),
                    "{id}: place {} is located from unknown {}",
                    place.id,
                    locate.from
                );
                ensure!(
                    locate.bearing_deg.is_finite()
                        && (0.0..360.0).contains(&locate.bearing_deg)
                        && locate.distance_m.is_finite()
                        && (1.0..=20_000_000.0).contains(&locate.distance_m)
                        && locate
                            .tolerance_m
                            .map_or(true, |t| t.is_finite() && (0.0..=5_000_000.0).contains(&t)),
                    "{id}: place {} has an invalid location (bearing 0..360, distance 1 m to 20000 km)",
                    place.id
                );
                ensure!(
                    place.at.is_none(),
                    "{id}: place {} cannot be both located and pinned",
                    place.id
                );
            }
            if let Some(geo) = &place.geo {
                ensure!(
                    self.manifest.world.is_some(),
                    "{id}: place {} is pinned to a historical site, but the disk does not define the world's real planet",
                    place.id
                );
                ensure!(
                    geo.latitude_deg.is_finite()
                        && (-90.0..=90.0).contains(&geo.latitude_deg)
                        && geo.longitude_deg.is_finite()
                        && (-180.0..=180.0).contains(&geo.longitude_deg),
                    "{id}: place {} has an invalid historical position",
                    place.id
                );
                for (what, wrong) in [
                    ("a parent", place.parent.is_some()),
                    ("a town spot", place.at.is_some()),
                    ("a location", place.locate.is_some()),
                    ("near places", !place.requires.near.is_empty()),
                ] {
                    ensure!(
                        !wrong,
                        "{id}: a place pinned to a historical site cannot also have {what}",
                    );
                }
            }
            for near in &place.requires.near {
                ensure!(
                    !local(&near.place) || places.contains(near.place.as_str()),
                    "{id}: place {} is near unknown {}",
                    place.id,
                    near.place
                );
                ensure!(
                    near.min_m >= 0.0 && near.max_m > near.min_m && near.max_m.is_finite(),
                    "{id}: place {} has an invalid distance range",
                    place.id
                );
            }
            for biome in &place.requires.biome {
                ensure!(
                    BIOMES.iter().any(|(name, _)| name == biome),
                    "{id}: unknown biome {biome}"
                );
            }
            if let Some(scenery) = &place.scenery {
                ensure!(
                    matches!(scenery.preset.as_str(), "town" | "graveyard" | "fortress"),
                    "{id}: unknown scenery preset {}",
                    scenery.preset
                );
                ensure!(
                    scenery
                        .size
                        .as_deref()
                        .map_or(true, |size| ["hamlet", "village", "town"].contains(&size)),
                    "{id}: unknown settlement size"
                );
            }
            if let Some(structure) = &place.structure {
                ensure!(
                    place.scenery.is_none(),
                    "{id}: place {} has both scenery and a structure",
                    place.id
                );
                for path in std::iter::once(&structure.model).chain(structure.collision.iter()) {
                    ensure!(
                        path.starts_with("media/models/") && path.ends_with(".glb"),
                        "{id}: structure model {path} must be a .glb below media/models/"
                    );
                }
                let in_range = |value: Option<f64>, low: f64, high: f64| {
                    value.map_or(true, |value| {
                        value.is_finite() && (low..=high).contains(&value)
                    })
                };
                ensure!(
                    in_range(structure.scale_m, 0.05, 20.0)
                        && in_range(structure.yaw_deg, -360.0, 360.0)
                        && in_range(structure.visible_m, 50.0, 5000.0)
                        && structure.offset_m.map_or(true, |offset| offset
                            .iter()
                            .all(|v| v.is_finite() && v.abs() <= 2000.0))
                        && structure.ground_m.map_or(true, |point| point
                            .iter()
                            .all(|v| v.is_finite() && v.abs() <= 1000.0)),
                    "{id}: place {} has a structure value out of range",
                    place.id
                );
            }
            if let Some(terrain) = &place.terrain {
                for path in std::iter::once(&terrain.heightmap)
                    .chain(terrain.water.iter())
                    .chain(terrain.clear.iter())
                {
                    ensure!(
                        path.starts_with("media/terrain/") && path.ends_with(".png"),
                        "{id}: terrain patch {path} must be a .png below media/terrain/"
                    );
                }
                let in_range = |value: f64, low: f64, high: f64| {
                    value.is_finite() && (low..=high).contains(&value)
                };
                ensure!(
                    terrain
                        .extent_m
                        .iter()
                        .all(|&v| in_range(v, 100.0, 10000.0)),
                    "{id}: place {} has a terrain extent out of range",
                    place.id
                );
                ensure!(
                    terrain
                        .offset_m
                        .map_or([0.0, 0.0], |o| o)
                        .iter()
                        .all(|&v| { v.is_finite() && v.abs() <= 5000.0 }),
                    "{id}: place {} has a terrain offset out of range",
                    place.id
                );
                let feather = terrain.feather_m.unwrap_or(100.0);
                let visible = terrain.visible_m.unwrap_or(1200.0);
                ensure!(
                    in_range(feather, 10.0, 500.0) && in_range(visible, 100.0, 5000.0),
                    "{id}: place {} has a terrain value out of range",
                    place.id
                );
                let [east, north] = terrain.offset_m.unwrap_or([0.0, 0.0]);
                ensure!(
                    (east.abs() + feather) <= terrain.extent_m[0] / 2.0
                        && (north.abs() + feather) <= terrain.extent_m[1] / 2.0,
                    "{id}: place {} keeps its anchor outside the feathered terrain core",
                    place.id
                );
            }
            if let Some(at) = &place.at {
                ensure!(
                    ["gate", "alley"].contains(&at.as_str()) && place.parent.is_some(),
                    "{id}: place {} is pinned to unknown spot {at} or has no parent town",
                    place.id
                );
            }
            ensure!(
                matches!(
                    place.kind.as_str(),
                    "city" | "building" | "graveyard" | "landmark" | "camp"
                ),
                "{id}: unknown place kind {}",
                place.kind
            );
        }
        ensure!(
            self.places.iter().filter(|place| place.spawn).count() <= 1,
            "{id}: at most one place may be the spawn point"
        );
        // Parent chains must end.
        let parent_of: HashMap<&str, &str> = self
            .places
            .iter()
            .filter_map(|p| {
                p.parent
                    .as_deref()
                    .filter(|r| local(r))
                    .map(|r| (p.id.as_str(), r))
            })
            .collect();
        for place in &self.places {
            let mut current = place.id.as_str();
            for _ in 0..=self.places.len() {
                match parent_of.get(current) {
                    Some(next) => current = next,
                    None => break,
                }
            }
            ensure!(
                !parent_of.contains_key(current),
                "{id}: place parent cycle at {}",
                place.id
            );
        }
        for npc in &self.npcs {
            ensure!(is_id(&npc.id, 48), "{id}: invalid npc id {}", npc.id);
            key_ok(&npc.name_key)?;
            if let Some(bio) = &npc.bio_key {
                key_ok(bio)?;
            }
            ensure!(
                !local(&npc.place) || places.contains(npc.place.as_str()),
                "{id}: npc {} stands at unknown place {}",
                npc.id,
                npc.place
            );
            ensure!(
                !local(&npc.dialogue) || dialogues.contains(npc.dialogue.as_str()),
                "{id}: npc {} has unknown dialogue {}",
                npc.id,
                npc.dialogue
            );
            ensure!(
                matches!(
                    npc.character.pack.as_str(),
                    "protagonists" | "retro" | "survivors"
                ),
                "{id}: unknown character pack {}",
                npc.character.pack
            );
            if let Some(model) = &npc.character.model {
                ensure!(
                    model.starts_with("media/models/") && model.ends_with(".glb"),
                    "{id}: model {model} must be a .glb below media/models/"
                );
            }
        }
        let tracks: HashSet<&str> = unique(self.music.iter().map(|m| m.id.as_str()), "music")?;
        for track in &self.music {
            key_ok(&track.title_key)?;
            ensure!(
                track.file.ends_with(".ogg"),
                "{id}: track {} must be an .ogg file",
                track.id
            );
        }
        if let Some(cover) = &self.manifest.cover {
            ensure!(
                cover.ends_with(".png") || cover.ends_with(".jpg"),
                "{id}: cover {cover} must be a png or jpg"
            );
        }
        for (label, track) in [
            ("cover_music", self.manifest.cover_music.as_deref()),
            ("ambient_music", self.manifest.ambient_music.as_deref()),
        ] {
            if let Some(track) = track {
                ensure!(
                    tracks.contains(track),
                    "{id}: {label} names unknown music {track}"
                );
            }
        }
        for place in &self.places {
            if let Some(wildlife) = &place.wildlife {
                ensure!(
                    !wildlife.flora.is_empty() || !wildlife.fauna.is_empty(),
                    "{id}: place {} wildlife needs flora or fauna entries",
                    place.id
                );
                for entry in wildlife.flora.iter().chain(wildlife.fauna.iter()) {
                    ensure!(
                        (0.0..=1000.0).contains(&entry.weight) && entry.weight > 0.0,
                        "{id}: place {} wildlife weight must be > 0 and <= 1000",
                        place.id
                    );
                }
                if let Some(radius) = wildlife.radius_m {
                    ensure!(
                        (1.0..=20000.0).contains(&radius),
                        "{id}: place {} wildlife radius_m must be 1..20000",
                        place.id
                    );
                }
                if let Some(density) = wildlife.density {
                    ensure!(
                        (0.0..=8.0).contains(&density) && density > 0.0,
                        "{id}: place {} wildlife density must be > 0 and <= 8",
                        place.id
                    );
                }
            }
        }
        if let Some(character) = &self.manifest.player_character {
            ensure!(
                crate::players::valid_character(character),
                "{id}: player_character {character} is not a character of the creator"
            );
        }
        if let Some(body) = &self.manifest.player_body {
            ensure!(
                self.manifest.player_character.is_some(),
                "{id}: player_body needs player_character, the character it describes"
            );
            ensure!(
                (0.5..=8.0).contains(&body.height_m) && (1.0..=20.0).contains(&body.toughness),
                "{id}: player_body height_m must be 0.5..8 and toughness 1..20"
            );
        }
        if let Some(model) = &self.manifest.player_model {
            ensure!(
                self.manifest.player_character.is_some(),
                "{id}: player_model needs player_character, the look it replaces"
            );
            ensure!(
                model.starts_with("media/models/") && model.ends_with(".glb"),
                "{id}: player_model {model} must be a .glb below media/models/"
            );
        }
        for npc in &self.npcs {
            if let Some(movement) = &npc.movement {
                ensure!(
                    (1.0..=200.0).contains(&movement.radius_m)
                        && (0.2..=8.0).contains(&movement.speed_m_s)
                        && (0.0..=120.0).contains(&movement.pause_s),
                    "{id}: npc {} movement needs radius_m 1..200, speed_m_s 0.2..8 and pause_s 0..120",
                    npc.id
                );
                match movement.kind {
                    MovementKind::Patrol => ensure!(
                        (2..=16).contains(&movement.waypoints.len())
                            && movement
                                .waypoints
                                .iter()
                                .all(|spot| spot[0].abs() <= 1000.0 && spot[1].abs() <= 1000.0),
                        "{id}: npc {} patrol needs 2 to 16 waypoints within 1000 m",
                        npc.id
                    ),
                    MovementKind::Wander => ensure!(
                        movement.waypoints.is_empty(),
                        "{id}: npc {} wanders: waypoints belong to a patrol",
                        npc.id
                    ),
                    MovementKind::Herd => ensure!(
                        movement.waypoints.len() == 1,
                        "{id}: npc {} drives a herd to exactly one pasture",
                        npc.id
                    ),
                }
                ensure!(
                    !npc.tags.iter().any(|tag| tag.starts_with("shop:")),
                    "{id}: npc {} keeps a shop and cannot walk away from it",
                    npc.id
                );
            }
            if let Some(size) = npc.size {
                ensure!(
                    (0.5..=4.0).contains(&size),
                    "{id}: npc {} size must be 0.5..4",
                    npc.id
                );
            }
            if let Some(portrait) = &npc.portrait {
                ensure!(
                    portrait.ends_with(".png") || portrait.ends_with(".jpg"),
                    "{id}: portrait {portrait} must be a png or jpg"
                );
            }
        }
        for dialogue in &self.dialogues {
            if let Some(track) = &dialogue.music {
                ensure!(
                    tracks.contains(track.as_str()),
                    "{id}: dialogue {} plays unknown music {track}",
                    dialogue.id
                );
            }
        }
        for entry in &self.lore {
            key_ok(&entry.title_key)?;
            key_ok(&entry.text_key)?;
        }
        for quest in self.quests.iter() {
            key_ok(&quest.title_key)?;
            ensure!(
                quest.stages.contains_key(&quest.start_stage),
                "{id}: quest {} has an unknown start stage",
                quest.id
            );
            for place in quest.start_place.iter() {
                ensure!(
                    !local(place) || places.contains(place.as_str()),
                    "{id}: quest {} starts at unknown place {place}",
                    quest.id
                );
            }
            for stage in quest.stages.values() {
                key_ok(&stage.text_key)?;
                for place in stage.guide.iter() {
                    ensure!(
                        !local(place) || places.contains(place.as_str()),
                        "{id}: quest {} guides to unknown place {place}",
                        quest.id
                    );
                }
                if let Some(reach) = &stage.reach {
                    ensure!(
                        quest.stages.contains_key(&reach.goto),
                        "{id}: quest {} reaches unknown stage {}",
                        quest.id,
                        reach.goto
                    );
                    ensure!(
                        !local(&reach.place) || places.contains(reach.place.as_str()),
                        "{id}: quest {} reaches unknown place {}",
                        quest.id,
                        reach.place
                    );
                    ensure!(
                        reach.radius_m >= 5.0 && reach.radius_m <= 2000.0,
                        "{id}: quest {} has an invalid reach radius",
                        quest.id
                    );
                }
            }
        }
        let stage_ok = |reference: &QuestStageRef| -> Result<()> {
            if local(&reference.quest) {
                let quest = quests
                    .get(reference.quest.as_str())
                    .ok_or_else(|| anyhow!("{id}: unknown quest {}", reference.quest))?;
                ensure!(
                    quest.stages.contains_key(&reference.stage),
                    "{id}: quest {} has no stage {}",
                    reference.quest,
                    reference.stage
                );
            }
            Ok(())
        };
        for dialogue in &self.dialogues {
            if dialogue.status.as_deref() == Some("draft") {
                ensure!(
                    allow_draft,
                    "{id}: dialogue {} is an unreviewed draft",
                    dialogue.id
                );
            }
            ensure!(
                dialogue.nodes.contains_key(&dialogue.start),
                "{id}: dialogue {} has an unknown start node",
                dialogue.id
            );
            ensure!(
                dialogue.nodes.len() <= 200,
                "{id}: dialogue {} has too many nodes",
                dialogue.id
            );
            let mut edges: HashMap<&str, Vec<&str>> = HashMap::new();
            for (name, node) in &dialogue.nodes {
                ensure!(is_id(name, 48), "{id}: invalid node id {name}");
                let shape = (
                    node.text_key.is_some(),
                    node.branch.is_some(),
                    node.choices.is_some(),
                );
                ensure!(
                    matches!(shape, (true, false, _) | (false, true, false)),
                    "{id}: node {}/{name} must hold either text (with optional choices) or a branch",
                    dialogue.id
                );
                if let Some(text) = &node.text_key {
                    key_ok(text)?;
                }
                let mut targets = Vec::new();
                if let Some(choices) = &node.choices {
                    ensure!(
                        !choices.is_empty() && choices.len() <= 8,
                        "{id}: node {name} needs 1-8 choices"
                    );
                    for choice in choices {
                        key_ok(&choice.text_key)?;
                        if let Some(condition) = &choice.condition {
                            check_condition(condition, &stage_ok)?;
                        }
                        check_effects(&choice.effects, &stage_ok)?;
                        targets.extend(choice.goto.as_deref());
                    }
                }
                check_effects(&node.effects, &stage_ok)?;
                if let Some(branch) = &node.branch {
                    ensure!(!branch.is_empty(), "{id}: node {name} has an empty branch");
                    ensure!(
                        branch.last().is_some_and(|entry| entry.condition.is_none()),
                        "{id}: the last branch entry of {name} needs no condition"
                    );
                    for entry in branch {
                        if let Some(condition) = &entry.condition {
                            check_condition(condition, &stage_ok)?;
                        }
                        targets.push(&entry.goto);
                    }
                }
                for target in &targets {
                    ensure!(
                        dialogue.nodes.contains_key(*target),
                        "{id}: node {name} jumps to unknown node {target}"
                    );
                }
                edges.insert(name, targets);
            }
            let mut reachable = HashSet::new();
            let mut todo = vec![dialogue.start.as_str()];
            while let Some(current) = todo.pop() {
                if reachable.insert(current) {
                    todo.extend(edges[current].iter().copied());
                }
            }
            for name in dialogue.nodes.keys() {
                ensure!(
                    reachable.contains(name.as_str()),
                    "{id}: node {}/{name} is unreachable",
                    dialogue.id
                );
            }
            // A chain of branch nodes must reach a node with text.
            for (name, node) in &dialogue.nodes {
                if node.branch.is_none() {
                    continue;
                }
                let mut seen = HashSet::new();
                let mut current = name.as_str();
                while let Some(branch) = dialogue.nodes[current].branch.as_ref() {
                    ensure!(
                        seen.insert(current),
                        "{id}: branch cycle at {}/{name}",
                        dialogue.id
                    );
                    current = &branch.last().expect("checked above").goto;
                }
            }
        }
        Ok(())
    }
}

fn unique<'a>(ids: impl Iterator<Item = &'a str>, what: &str) -> Result<HashSet<&'a str>> {
    let mut set = HashSet::new();
    for id in ids {
        ensure!(set.insert(id), "duplicate {what} id {id}");
    }
    Ok(set)
}

fn check_condition(
    condition: &Condition,
    stage_ok: &dyn Fn(&QuestStageRef) -> Result<()>,
) -> Result<()> {
    let set = [
        condition.all.is_some(),
        condition.any.is_some(),
        condition.not.is_some(),
        condition.flag.is_some(),
        condition.quest_stage.is_some(),
        condition.has_item.is_some(),
        condition.gold_at_least.is_some(),
    ]
    .iter()
    .filter(|set| **set)
    .count();
    ensure!(set == 1, "a condition must have exactly one key");
    for child in condition
        .all
        .iter()
        .chain(condition.any.iter())
        .flatten()
        .chain(condition.not.as_deref())
    {
        check_condition(child, stage_ok)?;
    }
    if let Some(stage) = &condition.quest_stage {
        stage_ok(stage)?;
    }
    if let Some(item) = &condition.has_item {
        ensure!(
            (1..=1_000_000).contains(&item.quantity()),
            "invalid item quantity"
        );
    }
    if let Some(gold) = condition.gold_at_least {
        ensure!((0..=1_000_000_000).contains(&gold), "invalid gold amount");
    }
    Ok(())
}

fn check_effects(
    effects: &[Effect],
    stage_ok: &dyn Fn(&QuestStageRef) -> Result<()>,
) -> Result<()> {
    for effect in effects {
        let set = [
            effect.give_item.is_some(),
            effect.take_item.is_some(),
            effect.gold.is_some(),
            effect.set_flag.is_some(),
            effect.set_stage.is_some(),
            effect.once.is_some(),
        ]
        .iter()
        .filter(|set| **set)
        .count();
        ensure!(set == 1, "an effect must have exactly one key");
        for item in [&effect.give_item, &effect.take_item].into_iter().flatten() {
            ensure!(
                (1..=1_000_000).contains(&item.quantity()),
                "invalid item quantity"
            );
        }
        if let Some(gold) = effect.gold {
            ensure!(
                gold != 0 && gold.abs() <= 1_000_000_000,
                "invalid gold amount"
            );
        }
        if let Some(stage) = &effect.set_stage {
            stage_ok(stage)?;
        }
        if let Some(once) = &effect.once {
            ensure!(is_id(&once.key, 48), "invalid once key {}", once.key);
            ensure!(
                once.effects.iter().all(|inner| inner.once.is_none()),
                "once effects cannot nest"
            );
            check_effects(&once.effects, stage_ok)?;
        }
    }
    Ok(())
}

pub fn parse_version(text: &str) -> Result<(u64, u64, u64)> {
    let parts: Vec<&str> = text.split('.').collect();
    if let [major, minor, patch] = parts[..] {
        if let (Ok(a), Ok(b), Ok(c)) = (major.parse(), minor.parse(), patch.parse()) {
            return Ok((a, b, c));
        }
    }
    bail!("invalid version {text}")
}

/// Orders disks by dependencies, then by id, and rejects missing dependencies,
/// conflicts, duplicates and incompatible versions.
pub fn order(disks: Vec<Disk>) -> Result<Vec<Disk>> {
    let mut by_id: BTreeMap<String, Disk> = BTreeMap::new();
    for disk in disks {
        let id = disk.manifest.id.clone();
        ensure!(
            by_id.insert(id.clone(), disk).is_none(),
            "datadisk {id} is selected twice"
        );
    }
    for disk in by_id.values() {
        let id = &disk.manifest.id;
        for requirement in &disk.manifest.requires {
            let other = by_id
                .get(&requirement.disk)
                .ok_or_else(|| anyhow!("datadisk {id} requires {}", requirement.disk))?;
            let have = parse_version(&other.manifest.version)?;
            let need = parse_version(&requirement.version)?;
            ensure!(
                have.0 == need.0 && have >= need,
                "datadisk {id} needs {} {} (same major)",
                requirement.disk,
                requirement.version
            );
        }
        for conflict in &disk.manifest.conflicts {
            ensure!(
                !by_id.contains_key(conflict),
                "datadisk {id} conflicts with {conflict}"
            );
        }
    }
    let mut ordered = Vec::new();
    let mut placed: BTreeSet<String> = BTreeSet::new();
    while ordered.len() < by_id.len() {
        let next = by_id
            .iter()
            .find(|(id, disk)| {
                !placed.contains(*id)
                    && disk
                        .manifest
                        .requires
                        .iter()
                        .all(|requirement| placed.contains(&requirement.disk))
            })
            .map(|(id, _)| id.clone())
            .ok_or_else(|| anyhow!("datadisk dependencies form a cycle"))?;
        placed.insert(next.clone());
        ordered.push(next);
    }
    Ok(ordered
        .into_iter()
        .map(|id| by_id.remove(&id).expect("listed above"))
        .collect())
}
