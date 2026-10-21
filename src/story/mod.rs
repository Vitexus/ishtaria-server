//! Story datadisks: NPCs, places, dialogue trees and quests loaded from data.
//!
//! The server owns every outcome. A client only names a choice; conditions, effects and
//! rewards are evaluated here, in one transaction, from structured data (no scripting).

pub mod api;
pub mod checkpoint;
pub mod cli;
pub mod disk;
pub mod eggs;
pub mod engine;
pub mod placement;
pub mod settlements;
pub mod trails;
pub mod walking;
pub mod world;

#[cfg(test)]
mod tests;

use anyhow::{anyhow, ensure, Result};
use disk::{Condition, Disk, Effect, Npc, Place, Quest};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
};

/// A place that world generation must find a spot for.
#[derive(Clone, Debug)]
pub struct Anchor {
    /// Fully qualified id, `<disk>:<place>`.
    pub id: String,
    pub place: Place,
}

/// A datadisk as announced to clients.
#[derive(Clone, Debug, serde::Serialize)]
pub struct DiskInfo {
    pub id: String,
    pub version: String,
    pub name: String,
    /// URL path of the cover image, when the disk has one.
    pub cover: Option<String>,
    /// URL path of the cover/logo jingle, when the disk has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_music: Option<MusicRef>,
    /// URL path of the default travel track outside place music radii.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ambient_music: Option<MusicRef>,
    /// The character new players of the world are given, when the disk decides it.
    pub player_character: Option<String>,
    /// URL path of the model clients draw for that character instead of its pack look.
    pub player_model: Option<String>,
    /// How tall and tough that character is, when the disk says.
    pub player_body: Option<disk::PlayerBody>,
}

/// A music track announced with a disk (cover jingle or ambient).
#[derive(Clone, Debug, serde::Serialize)]
pub struct MusicRef {
    pub url: String,
    #[serde(rename = "loop")]
    pub looped: bool,
}

/// Several datadisks composed into one world's story. Every id is `<disk>:<id>`.
#[derive(Clone, Debug, Default)]
pub struct Story {
    /// The disks in application order with their content hashes.
    pub disks: Vec<(String, String, String)>,
    /// What clients are told about each disk, in application order.
    pub infos: Vec<DiskInfo>,
    pub anchors: Vec<Anchor>,
    pub npcs: BTreeMap<String, Npc>,
    pub dialogues: HashMap<String, disk::Dialogue>,
    pub quests: BTreeMap<String, Quest>,
    /// Tracks by qualified id: (file as `<disk>/<path>`, title key, loops).
    pub music: BTreeMap<String, (String, String, bool)>,
    /// Served media: `<disk>/<path>` -> (absolute file, sha256).
    pub media: BTreeMap<String, (PathBuf, String)>,
    /// Spoken lines: (language, qualified text key) -> served media `<disk>/<path>`.
    pub voices: BTreeMap<(String, String), String>,
    /// Language -> qualified text key -> text.
    pub strings: BTreeMap<String, BTreeMap<String, String>>,
    /// Easter-egg unlocks from every disk (npc ids and flags already qualified).
    pub easter_eggs: Vec<disk::EasterEgg>,
    /// Knowledge cards by qualified id; `image` is the media key (`<disk>/media/knowledge/…`).
    pub knowledge: BTreeMap<String, disk::KnowledgeCard>,
    /// The real planet one of the disks defines, when any.
    pub world: Option<WorldPlan>,
    /// Items granted once at character creation (`item_id`, quantity), from every disk.
    pub starting_items: Vec<(String, i64)>,
    /// Quests begun at character creation: (qualified quest id, start stage).
    pub auto_start_quests: Vec<(String, String)>,
}

/// The real planet of a story: which disk defines it and the two equirectangular
/// rasters below that disk (`ishtaria-worldgen --earth` strips them onto the
/// cube-sphere instead of the procedural noise).
#[derive(Clone, Debug, PartialEq)]
pub struct WorldPlan {
    pub disk: String,
    pub terrain: disk::WorldTerrain,
    /// The land biomes the disk paints, when it does.
    pub biomes: Option<std::sync::Arc<crate::environment::BiomeMap>>,
}

fn qualify(disk: &str, reference: &str) -> String {
    if reference.contains(':') {
        reference.to_owned()
    } else {
        format!("{disk}:{reference}")
    }
}

fn qualify_condition(condition: &mut Condition, disk: &str) {
    for child in condition
        .all
        .iter_mut()
        .chain(condition.any.iter_mut())
        .flatten()
        .chain(condition.not.as_deref_mut())
    {
        qualify_condition(child, disk);
    }
    if let Some(flag) = &mut condition.flag {
        *flag = qualify(disk, flag);
    }
    if let Some(stage) = &mut condition.quest_stage {
        stage.quest = qualify(disk, &stage.quest);
    }
}

fn qualify_effects(effects: &mut [Effect], disk: &str) {
    for effect in effects {
        if let Some(flag) = &mut effect.set_flag {
            *flag = qualify(disk, flag);
        }
        if let Some(stage) = &mut effect.set_stage {
            stage.quest = qualify(disk, &stage.quest);
        }
        if let Some(once) = &mut effect.once {
            once.key = format!("{disk}:{}", once.key);
            qualify_effects(&mut once.effects, disk);
        }
        if let Some(playable) = &mut effect.set_playable {
            if playable.model.is_some() {
                playable.disk = Some(disk.to_owned());
            }
            if let Some(npc) = &mut playable.hide_npc {
                *npc = qualify(disk, npc);
            }
        }
        if let Some(place) = &mut effect.teleport_place {
            *place = qualify(disk, place);
        }
    }
}

fn qualify_egg_condition(condition: &mut disk::EggCondition, disk: &str) {
    for child in condition
        .all
        .iter_mut()
        .chain(condition.any.iter_mut())
        .flatten()
        .chain(condition.not.as_deref_mut())
    {
        qualify_egg_condition(child, disk);
    }
    if let Some(flag) = &mut condition.flag {
        *flag = qualify(disk, flag);
    }
    if let Some(stage) = &mut condition.quest_stage {
        stage.quest = qualify(disk, &stage.quest);
    }
}

impl Story {
    /// Composes loaded disks (any order) into one story. Cross-disk references must name
    /// a declared dependency of the referring disk.
    pub fn compose(disks: Vec<Disk>) -> Result<Self> {
        let disks = disk::order(disks)?;
        let mut story = Story::default();
        for disk in disks {
            story.add_disk(disk)?;
        }
        story.check_references()?;
        Ok(story)
    }

    /// Adds one disk to the story. Used by [`Story::compose`] and for content generated after
    /// the world's places have been placed.
    pub fn add_disk(&mut self, disk: Disk) -> Result<()> {
        let story = self;
        let id = disk.manifest.id.clone();
        match (&story.world, &disk.manifest.world) {
            (Some(existing), Some(_)) => {
                anyhow::bail!(
                    "two datadisks define the world's planet: {} and {id}",
                    existing.disk
                )
            }
            (None, Some(world)) => {
                story.world = Some(WorldPlan {
                    disk: id.clone(),
                    terrain: world.terrain.clone(),
                    biomes: disk.world_biome_map()?.map(std::sync::Arc::new),
                });
            }
            _ => {}
        }
        story.disks.push((
            id.clone(),
            disk.manifest.version.clone(),
            disk.sha256.clone(),
        ));
        let track_ref = |local: &Option<String>| -> Option<MusicRef> {
            let track_id = local.as_deref()?;
            let track = disk.music.iter().find(|track| track.id == track_id)?;
            Some(MusicRef {
                url: world::media_url(&format!("{id}/{}", track.file)),
                looped: track.looped,
            })
        };
        story.infos.push(DiskInfo {
            id: id.clone(),
            version: disk.manifest.version.clone(),
            name: disk.manifest.name.clone(),
            cover: disk
                .manifest
                .cover
                .as_deref()
                .map(|cover| world::media_url(&format!("{id}/{cover}"))),
            cover_music: track_ref(&disk.manifest.cover_music),
            ambient_music: track_ref(&disk.manifest.ambient_music),
            player_character: disk.manifest.player_character.clone(),
            player_body: disk.manifest.player_body,
            player_model: disk
                .manifest
                .player_model
                .as_deref()
                .map(|model| world::media_url(&format!("{id}/{model}"))),
        });
        let depends: Vec<&str> = disk
            .manifest
            .requires
            .iter()
            .map(|r| r.disk.as_str())
            .collect();
        let foreign_ok = |reference: &str| -> Result<()> {
            if let Some((other, _)) = reference.split_once(':') {
                ensure!(
                    other == id || depends.contains(&other),
                    "{id}: reference {reference} names a disk that is not a declared dependency"
                );
            }
            Ok(())
        };
        for place in disk.places {
            if let Some(parent) = &place.parent {
                foreign_ok(parent)?;
            }
            if let Some(locate) = &place.locate {
                foreign_ok(&locate.from)?;
            }
            for near in &place.requires.near {
                foreign_ok(&near.place)?;
            }
            let mut place = place;
            if let Some(locate) = &mut place.locate {
                locate.from = qualify(&id, &locate.from);
            }
            place.parent = place.parent.map(|p| qualify(&id, &p));
            for near in &mut place.requires.near {
                near.place = qualify(&id, &near.place);
            }
            place.name_key = qualify(&id, &place.name_key);
            place.music = place.music.take().map(|m| qualify(&id, &m));
            story.anchors.push(Anchor {
                id: qualify(&id, &place.id),
                place,
            });
        }
        for mut npc in disk.npcs {
            foreign_ok(&npc.place)?;
            foreign_ok(&npc.dialogue)?;
            let qualified = qualify(&id, &npc.id);
            npc.place = qualify(&id, &npc.place);
            npc.dialogue = qualify(&id, &npc.dialogue);
            npc.name_key = qualify(&id, &npc.name_key);
            npc.bio_key = npc.bio_key.map(|k| qualify(&id, &k));
            npc.about = npc.about.map(|k| qualify(&id, &k));
            npc.portrait = npc.portrait.map(|p| format!("{id}/{p}"));
            // A model below `media/models/` is served like a portrait: under the id of its disk.
            npc.character.model = npc.character.model.map(|m| format!("{id}/{m}"));
            npc.id = qualified.clone();
            story.npcs.insert(qualified, npc);
        }
        for mut dialogue in disk.dialogues {
            let qualified = qualify(&id, &dialogue.id);
            dialogue.id = qualified.clone();
            dialogue.music = dialogue.music.take().map(|m| qualify(&id, &m));
            for node in dialogue.nodes.values_mut() {
                node.text_key = node.text_key.take().map(|k| qualify(&id, &k));
                qualify_effects(&mut node.effects, &id);
                for choice in node.choices.iter_mut().flatten() {
                    choice.text_key = qualify(&id, &choice.text_key);
                    if let Some(condition) = &mut choice.condition {
                        qualify_condition(condition, &id);
                    }
                    qualify_effects(&mut choice.effects, &id);
                }
                for entry in node.branch.iter_mut().flatten() {
                    if let Some(condition) = &mut entry.condition {
                        qualify_condition(condition, &id);
                    }
                }
            }
            story.dialogues.insert(qualified, dialogue);
        }
        for track in disk.music {
            story.music.insert(
                qualify(&id, &track.id),
                (
                    format!("{id}/{}", track.file),
                    qualify(&id, &track.title_key),
                    track.looped,
                ),
            );
        }
        for (path, file) in disk.media {
            story.media.insert(format!("{id}/{path}"), file);
        }
        for ((language, key), path) in disk.voices {
            story
                .voices
                .insert((language, qualify(&id, &key)), format!("{id}/{path}"));
        }
        for item in &disk.manifest.starting_items {
            ensure!(
                (1..=1_000_000).contains(&item.quantity()),
                "{id}: invalid starting item quantity for {}",
                item.item
            );
            story
                .starting_items
                .push((item.item.clone(), item.quantity()));
        }
        for mut quest in disk.quests {
            let qualified = qualify(&id, &quest.id);
            quest.id = qualified.clone();
            quest.title_key = qualify(&id, &quest.title_key);
            if let Some(place) = &quest.start_place {
                foreign_ok(place)?;
                quest.start_place = Some(qualify(&id, place));
            }
            if let Some(excerpt) = &mut quest.excerpt {
                excerpt.paraphrase_key = qualify(&id, &excerpt.paraphrase_key);
                if let Some(akkadian) = &mut excerpt.akkadian_key {
                    *akkadian = qualify(&id, akkadian);
                }
            }
            for stage in quest.stages.values_mut() {
                stage.text_key = qualify(&id, &stage.text_key);
                if let Some(place) = &stage.guide {
                    foreign_ok(place)?;
                    stage.guide = Some(qualify(&id, place));
                }
                if let Some(reach) = &mut stage.reach {
                    foreign_ok(&reach.place)?;
                    reach.place = qualify(&id, &reach.place);
                }
                if let Some(restart) = &mut stage.fail_restart {
                    foreign_ok(&restart.place)?;
                    restart.place = qualify(&id, &restart.place);
                }
            }
            story.quests.insert(qualified, quest);
        }
        for quest_id in &disk.manifest.auto_start_quests {
            let qualified = qualify(&id, quest_id);
            let stage = story
                .quests
                .get(&qualified)
                .map(|quest| quest.start_stage.clone())
                .ok_or_else(|| {
                    anyhow::anyhow!("{id}: auto_start_quests names unknown quest {quest_id}")
                })?;
            story.auto_start_quests.push((qualified, stage));
        }
        for (language, table) in disk.strings {
            let target = story.strings.entry(language).or_default();
            for (key, text) in table {
                target.insert(qualify(&id, &key), text);
            }
        }
        for mut card in disk.knowledge {
            card.id = qualify(&id, &card.id);
            card.title_key = qualify(&id, &card.title_key);
            card.text_key = qualify(&id, &card.text_key);
            card.credit_key = qualify(&id, &card.credit_key);
            card.image = format!("{id}/{}", card.image);
            story.knowledge.insert(card.id.clone(), card);
        }
        for mut egg in disk.easter_eggs {
            egg.id = format!("{id}:{}", egg.id);
            egg.npc = qualify(&id, &egg.npc);
            if let Some(condition) = &mut egg.condition {
                qualify_egg_condition(condition, &id);
            }
            story.easter_eggs.push(egg);
        }
        Ok(())
    }

    /// Cross-disk references resolve to something that exists.
    fn check_references(&self) -> Result<()> {
        let places: Vec<&str> = self.anchors.iter().map(|a| a.id.as_str()).collect();
        for anchor in &self.anchors {
            let near = anchor.place.requires.near.iter().map(|n| n.place.as_str());
            let located = anchor.place.locate.iter().map(|l| l.from.as_str());
            for reference in anchor
                .place
                .parent
                .as_deref()
                .into_iter()
                .chain(near)
                .chain(located)
            {
                ensure!(
                    places.contains(&reference),
                    "{}: unknown place {reference}",
                    anchor.id
                );
            }
        }
        for npc in self.npcs.values() {
            ensure!(
                places.contains(&npc.place.as_str()),
                "{}: unknown place {}",
                npc.id,
                npc.place
            );
            ensure!(
                self.dialogues.contains_key(&npc.dialogue),
                "{}: unknown dialogue {}",
                npc.id,
                npc.dialogue
            );
        }
        for npc in self.npcs.values() {
            if let Some(about) = &npc.about {
                ensure!(
                    self.knowledge.contains_key(about),
                    "{}: unknown knowledge card {about}",
                    npc.id
                );
            }
        }
        for egg in &self.easter_eggs {
            ensure!(
                self.npcs.contains_key(&egg.npc),
                "{}: unknown easter-egg npc {}",
                egg.id,
                egg.npc
            );
        }
        for quest in self.quests.values() {
            let guides = quest
                .stages
                .values()
                .filter_map(|stage| stage.guide.as_ref());
            for place in quest.start_place.iter().chain(guides) {
                ensure!(
                    places.contains(&place.as_str()),
                    "{}: unknown place {place}",
                    quest.id
                );
            }
        }
        for anchor in &self.anchors {
            if let Some(track) = &anchor.place.music {
                ensure!(
                    self.music.contains_key(track),
                    "{}: unknown music {track}",
                    anchor.id
                );
            }
        }
        for dialogue in self.dialogues.values() {
            if let Some(track) = &dialogue.music {
                ensure!(
                    self.music.contains_key(track),
                    "{}: unknown music {track}",
                    dialogue.id
                );
            }
            for node in dialogue.nodes.values() {
                let mut stages = Vec::new();
                for choice in node.choices.iter().flatten() {
                    collect_condition_stages(choice.condition.as_ref(), &mut stages);
                    collect_effect_stages(&choice.effects, &mut stages);
                }
                for entry in node.branch.iter().flatten() {
                    collect_condition_stages(entry.condition.as_ref(), &mut stages);
                }
                collect_effect_stages(&node.effects, &mut stages);
                for (quest, stage) in stages {
                    let found = self
                        .quests
                        .get(&quest)
                        .ok_or_else(|| anyhow!("{}: unknown quest {quest}", dialogue.id))?;
                    ensure!(
                        found.stages.contains_key(&stage),
                        "{}: quest {quest} has no stage {stage}",
                        dialogue.id
                    );
                }
            }
        }
        Ok(())
    }

    /// Checks that every reference between the parts of the story resolves.
    pub fn check(&self) -> Result<()> {
        self.check_references()
    }

    /// Loads disk directories and composes them together with built-in `extra` disks.
    pub fn load_with(dirs: &[PathBuf], extra: Vec<Disk>, allow_draft: bool) -> Result<Self> {
        let mut disks = dirs
            .iter()
            .map(|dir| Disk::load(dir, allow_draft))
            .collect::<Result<Vec<_>>>()?;
        disks.extend(extra);
        Self::compose(disks)
    }

    /// Loads disk directories and composes them.
    pub fn load(dirs: &[PathBuf], allow_draft: bool) -> Result<Self> {
        let disks = dirs
            .iter()
            .map(|dir| Disk::load(dir, allow_draft))
            .collect::<Result<Vec<_>>>()?;
        Self::compose(disks)
    }
}

fn collect_condition_stages(condition: Option<&Condition>, out: &mut Vec<(String, String)>) {
    let Some(condition) = condition else { return };
    for child in condition
        .all
        .iter()
        .chain(condition.any.iter())
        .flatten()
        .chain(condition.not.as_deref())
    {
        collect_condition_stages(Some(child), out);
    }
    if let Some(stage) = &condition.quest_stage {
        out.push((stage.quest.clone(), stage.stage.clone()));
    }
}

fn collect_effect_stages(effects: &[Effect], out: &mut Vec<(String, String)>) {
    for effect in effects {
        if let Some(stage) = &effect.set_stage {
            out.push((stage.quest.clone(), stage.stage.clone()));
        }
        if let Some(once) = &effect.once {
            collect_effect_stages(&once.effects, out);
        }
    }
}
