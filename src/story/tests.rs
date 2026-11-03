use super::engine::{self, Choose, Op, PlayerState};
use super::placement::Ground;
use super::{
    disk::{Disk, MovementKind},
    Story,
};
use std::{
    fs,
    path::{Path, PathBuf},
};

fn write(root: &Path, path: &str, text: &str) {
    let file = root.join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, text).unwrap();
}

fn scratch(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let number = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "ishtaria-story-{}-{number}-{name}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn base(root: &Path, id: &str, version: &str, extra: &str) {
    write(
        root,
        "datadisk.yaml",
        &format!(
            "id: {id}\nversion: {version}\nname: Test\nrequires_ruleset: \"1\"\nlicense: CC0-1.0\nattribution: Test\nlanguages: [en, cs]\n{extra}"
        ),
    );
    write(
        root,
        "i18n/en.yaml",
        "place.hut: Hut\nnpc.hermit: Hermit\nhi: Hi\nbye: Bye\nq: Quest\nq.s1: One\nq.s2: Two\n",
    );
    write(root, "i18n/cs.yaml", "place.hut: Chata\nnpc.hermit: Poustevnik\nhi: Ahoj\nbye: Nashle\nq: Ukol\nq.s1: Jedna\nq.s2: Dva\n");
    write(
        root,
        "places/p.yaml",
        "- {id: hut, kind: building, name_key: place.hut}\n",
    );
    write(
        root,
        "npcs/n.yaml",
        "- {id: hermit, name_key: npc.hermit, place: hut, character: {pack: retro, skin: humanMaleA}, dialogue: hermit}\n",
    );
    write(
        root,
        "quests/q.yaml",
        "id: q\ntitle_key: q\nstart_stage: s1\nstages:\n  s1: {text_key: q.s1}\n  s2: {text_key: q.s2, final: true}\n",
    );
    write(
        root,
        "dialogues/hermit.yaml",
        "id: hermit\nstart: hi\nnodes:\n  hi:\n    text_key: hi\n    choices:\n      - {text_key: bye, if: {gold_at_least: 5}, effects: [{gold: -5}, {once: {key: reward, effects: [{gold: 20}, {set_stage: {quest: q, stage: s2}}]}}, {set_flag: met}]}\n      - {text_key: bye, if: {not: {flag: met}}}\n",
    );
}

#[test]
fn composes_disks_in_dependency_order_with_qualified_ids() {
    let a = scratch("a");
    let b = scratch("b");
    base(&a, "zeta", "1.2.0", "");
    base(
        &b,
        "alpha",
        "1.0.0",
        "requires:\n  - {disk: zeta, version: 1.0.0}\n",
    );
    let story = Story::load(&[b.clone(), a.clone()], false).unwrap();
    let order: Vec<&str> = story.disks.iter().map(|d| d.0.as_str()).collect();
    assert_eq!(order, ["zeta", "alpha"], "dependencies come first");
    assert!(story.npcs.contains_key("zeta:hermit") && story.npcs.contains_key("alpha:hermit"));
    assert_eq!(story.strings["en"]["zeta:place.hut"], "Hut");
    assert_eq!(story.strings["cs"]["alpha:npc.hermit"], "Poustevnik");
    assert_eq!(story.npcs["alpha:hermit"].dialogue, "alpha:hermit");
    assert_eq!(story.disks[0].2.len(), 64);
}

#[test]
fn announces_the_cover_of_a_disk_and_rejects_a_missing_one() {
    let a = scratch("cover");
    base(&a, "covered", "1.0.0", "cover: media/cover/art.jpg\n");
    assert!(
        Story::load(std::slice::from_ref(&a), false).is_err(),
        "a cover that is not shipped must be rejected"
    );
    write(&a, "media/cover/art.jpg", "jpeg bytes");
    let story = Story::load(&[a], false).unwrap();
    assert_eq!(story.infos.len(), 1);
    assert_eq!(story.infos[0].id, "covered");
    assert_eq!(story.infos[0].name, "Test");
    assert_eq!(
        story.infos[0].cover.as_deref(),
        Some("/story/media/covered/media/cover/art.jpg")
    );
    assert!(story.media.contains_key("covered/media/cover/art.jpg"));
}

#[test]
fn a_disk_may_decide_the_character_of_new_players() {
    let a = scratch("hero");
    base(&a, "heroic", "1.0.0", "player_character: nowhere/nobody\n");
    assert!(
        Story::load(std::slice::from_ref(&a), false).is_err(),
        "a character the creator does not know must be rejected"
    );
    let b = scratch("hero2");
    base(
        &b,
        "heroic",
        "1.0.0",
        "player_character: protagonists/criminalMaleA\n",
    );
    let story = Story::load(&[b], false).unwrap();
    assert_eq!(
        story.infos[0].player_character.as_deref(),
        Some("protagonists/criminalMaleA")
    );
    // A model of the disk replaces the look: it must be shipped, and needs the character it stands for.
    let d = scratch("hero4");
    base(
        &d,
        "heroic",
        "1.0.0",
        "player_model: media/models/hero.glb\n",
    );
    assert!(
        Story::load(std::slice::from_ref(&d), false).is_err(),
        "no player_character"
    );
    let e = scratch("hero5");
    base(
        &e,
        "heroic",
        "1.0.0",
        "player_character: protagonists/criminalMaleA\nplayer_model: media/models/hero.glb\n",
    );
    assert!(
        Story::load(std::slice::from_ref(&e), false).is_err(),
        "model not shipped"
    );
    write(&e, "media/models/hero.glb", "glTF");
    let story = Story::load(&[e], false).unwrap();
    assert_eq!(
        story.infos[0].player_model.as_deref(),
        Some("/story/media/heroic/media/models/hero.glb")
    );
    let f = scratch("hero6");
    base(
        &f,
        "heroic",
        "1.0.0",
        "player_body: {height_m: 3.0, toughness: 1.5}\n",
    );
    assert!(
        Story::load(std::slice::from_ref(&f), false).is_err(),
        "no player_character"
    );
    let g = scratch("hero7");
    base(
        &g,
        "heroic",
        "1.0.0",
        "player_character: protagonists/criminalMaleA\nplayer_body: {height_m: 3.0, toughness: 1.5}\n",
    );
    let body = Story::load(&[g], false).unwrap().infos[0]
        .player_body
        .unwrap();
    assert!((body.height_m - 3.0).abs() < 1e-9 && (body.toughness - 1.5).abs() < 1e-9);
    let h = scratch("hero8");
    base(
        &h,
        "heroic",
        "1.0.0",
        "player_character: protagonists/criminalMaleA\nplayer_body: {height_m: 30.0}\n",
    );
    assert!(Story::load(&[h], false).is_err(), "a thirty-metre hero");
    let c = scratch("hero3");
    base(&c, "plain", "1.0.0", "");
    assert_eq!(
        Story::load(&[c], false).unwrap().infos[0].player_character,
        None
    );
}

#[test]
fn a_disk_lists_buildings_around_a_place_and_they_stand_and_block() {
    use super::placement::place_anchors;
    use super::settlements;
    let a = scratch("buildings");
    base(&a, "zeta", "1.0.0", "");
    write(&a, "media/models/architecture/house.glb", "glTF");
    let list = |items: &str, place: &str| {
        write(
            &a,
            "buildings/h.yaml",
            &format!("place: {place}\nitems:\n{items}"),
        );
    };
    let house = "  - {model: media/models/architecture/house.glb, x: 10.0, z: -20.0, yaw_deg: 90.0, scale: [0.5, 0.3, 0.4], footprint: [8.0, 4.0]}\n  - {model: media/models/architecture/house.glb, x: 30.0, z: -20.0, yaw_deg: 0.0, scale: [0.5, 0.3, 0.4], footprint: [6.0, 6.0]}\n";
    list(house, "hut");
    let mut story = Story::load(std::slice::from_ref(&a), false).unwrap();
    assert_eq!(story.buildings.len(), 1);
    assert_eq!(story.buildings[0].0, "zeta:hut");
    assert!(story
        .media
        .contains_key("zeta/media/models/architecture/house.glb"));
    let mut sites: std::collections::HashMap<_, _> = place_anchors(&story, &Flat, "42", &[])
        .unwrap()
        .into_iter()
        .map(|site| (site.id.clone(), site))
        .collect();
    let built = settlements::finish(&mut story, &mut sites, &Flat, "42").unwrap();
    let houses: Vec<_> = built
        .props
        .iter()
        .filter(|prop| prop.model == "disk:zeta:media/models/architecture/house.glb")
        .collect();
    assert_eq!(houses.len(), 2);
    // Bearing 90 (east): the front +Z points east, so the model is turned a quarter turn; bearing 0 faces north.
    assert!((houses[0].yaw - std::f64::consts::FRAC_PI_2).abs() < 1e-9);
    assert!((houses[1].yaw - std::f64::consts::PI).abs() < 1e-9);
    assert!(houses
        .iter()
        .all(|house| (0.0..std::f64::consts::TAU).contains(&house.yaw)));
    assert_eq!(houses[0].scale_xyz, Some([0.5, 0.3, 0.4]));
    assert!((houses[0].scale_m - 0.5).abs() < 1e-9);
    // The 8 x 4 plot is two round colliders of 2 m, the 6 x 6 plot one of 3 m.
    let blocks: Vec<_> = built
        .colliders
        .iter()
        .filter(|object| object.id.contains("#building"))
        .collect();
    assert_eq!(blocks.len(), 3);
    assert!(blocks
        .iter()
        .all(|object| object.model.is_empty() && object.collision_radius_m >= 2.0 - 1e-9));
    let reach = |a: &crate::movement::WorldObject, b: &crate::movement::WorldObject| {
        (0..3)
            .map(|k| (a.position[k] - b.position[k]).powi(2))
            .sum::<f64>()
            .sqrt()
    };
    assert!((reach(blocks[0], blocks[1]) - 4.0).abs() < 0.05);
    // Rejected: an unknown place, a bearing off the compass, a model the disk does not ship.
    list(house, "nowhere");
    assert!(Story::load(std::slice::from_ref(&a), false).is_err());
    list(&house.replace("yaw_deg: 90.0", "yaw_deg: 400.0"), "hut");
    assert!(Story::load(std::slice::from_ref(&a), false).is_err());
    list(&house.replace("house.glb", "gone.glb"), "hut");
    assert!(Story::load(std::slice::from_ref(&a), false).is_err());
}

#[test]
fn a_disk_names_the_ground_set_clients_draw() {
    let a = scratch("ground1");
    base(&a, "dusty", "1.0.0", "ground: alluvial\n");
    assert_eq!(
        Story::load(&[a], false).unwrap().infos[0].ground.as_deref(),
        Some("alluvial")
    );
    let b = scratch("ground2");
    base(&b, "dusty", "1.0.0", "ground: lava\n");
    assert!(Story::load(&[b], false).is_err(), "an unknown ground set");
    let c = scratch("ground3");
    base(&c, "plain", "1.0.0", "");
    assert_eq!(Story::load(&[c], false).unwrap().infos[0].ground, None);
}

#[test]
fn a_disk_may_set_the_sky_of_its_world() {
    let a = scratch("sky1");
    base(
        &a,
        "ancient",
        "1.0.0",
        "astronomy: {year: -2699, sidereal_day_seconds: 86164.0}\n",
    );
    let story = Story::load(&[a], false).unwrap();
    let sky = story.infos[0].astronomy.expect("the sky is kept");
    assert_eq!(sky.year, Some(-2699.0));
    assert_eq!(sky.obliquity_deg, None);
    for (name, bad) in [
        ("sky2", "astronomy: {year: -20000}\n"),
        ("sky3", "astronomy: {obliquity_deg: 75}\n"),
        ("sky4", "astronomy: {sidereal_day_seconds: 10}\n"),
        ("sky5", "astronomy: {epoch: 3}\n"),
    ] {
        let dir = scratch(name);
        base(&dir, "ancient", "1.0.0", bad);
        assert!(Story::load(&[dir], false).is_err(), "{bad}");
    }
    let plain = scratch("sky6");
    base(&plain, "plain", "1.0.0", "");
    assert!(Story::load(&[plain], false).unwrap().infos[0]
        .astronomy
        .is_none());
}

#[test]
fn a_character_may_walk_and_the_movement_is_validated() {
    let a = scratch("walk");
    base(&a, "walker", "1.0.0", "");
    let with = |movement: &str| {
        write(
            &a,
            "npcs/n.yaml",
            &format!("- {{id: hermit, name_key: npc.hermit, place: hut, character: {{pack: retro, skin: humanMaleA}}, dialogue: hermit, movement: {movement}}}\n"),
        );
        Story::load(std::slice::from_ref(&a), false)
    };
    let story = with("{kind: wander, radius_m: 30, speed_m_s: 1.2}").unwrap();
    let movement = story.npcs["walker:hermit"].movement.clone().unwrap();
    assert_eq!(movement.kind, super::disk::MovementKind::Wander);
    assert!((movement.pause_s - 6.0).abs() < 1e-9, "default pause");
    assert!(with("{kind: patrol, waypoints: [[0, 0], [20, 0], [20, 20]]}").is_ok());
    assert!(
        with("{kind: patrol, waypoints: [[0, 0]]}").is_err(),
        "one waypoint is no beat"
    );
    assert!(
        with("{kind: wander, waypoints: [[0, 0], [5, 5]]}").is_err(),
        "a wanderer has no beat"
    );
    assert!(with("{kind: wander, speed_m_s: 30}").is_err(), "too fast");
    assert!(with("{kind: fly}").is_err(), "unknown kind");
    assert!(with("{kind: wander, radius_m: 900}").is_err(), "too far");
}

#[test]
fn a_character_tells_a_knowledge_card_and_the_card_is_validated() {
    let a = scratch("knowledge");
    base(&a, "wise", "1.0.0", "");
    write(&a, "i18n/en.yaml", "place.hut: Hut\nnpc.hermit: Hermit\nhi: Hi\nbye: Bye\nq: Quest\nq.s1: One\nq.s2: Two\nk.t: Hermits\nk.x: Hermits live alone.\nk.c: A painting, public domain.\n");
    write(&a, "i18n/cs.yaml", "place.hut: Chata\nnpc.hermit: Poustevnik\nhi: Ahoj\nbye: Nashle\nq: Ukol\nq.s1: Jedna\nq.s2: Dva\nk.t: Poustevnici\nk.x: Poustevnici zijí o samotě.\nk.c: Obraz, volné dílo.\n");
    write(&a, "media/knowledge/hermit.jpg", "not really a picture");
    let with = |wikipedia: &str, image: &str, about: &str| {
        write(
            &a,
            "knowledge.yaml",
            &format!("- {{id: hermit, title_key: k.t, text_key: k.x, image: {image}, credit_key: k.c, wikipedia: {wikipedia}}}\n"),
        );
        write(
            &a,
            "npcs/n.yaml",
            &format!("- {{id: hermit, name_key: npc.hermit, place: hut, character: {{pack: retro, skin: humanMaleA}}, dialogue: hermit, about: {about}}}\n"),
        );
        Story::load(std::slice::from_ref(&a), false)
    };
    let story = with(
        "{en: Hermit, cs: Poustevník}",
        "media/knowledge/hermit.jpg",
        "hermit",
    )
    .unwrap();
    let card = &story.knowledge["wise:hermit"];
    assert_eq!(card.text_key, "wise:k.x");
    assert_eq!(card.image, "wise/media/knowledge/hermit.jpg");
    assert_eq!(
        story.npcs["wise:hermit"].about.as_deref(),
        Some("wise:hermit")
    );
    assert!(story.media.contains_key("wise/media/knowledge/hermit.jpg"));
    assert!(
        with("{en: Hermit}", "media/knowledge/hermit.jpg", "nobody").is_err(),
        "an unknown card"
    );
    assert!(
        with("{cs: Poustevník}", "media/knowledge/hermit.jpg", "hermit").is_err(),
        "the English title is required"
    );
    for bad in [
        "{en: Two words}",
        "{en: 'a?b=c'}",
        "{en: 'a#b'}",
        "{en: a/b}",
        "{en: _x}",
        "{en: Hermit, czech: Poustevník}",
    ] {
        assert!(
            with(bad, "media/knowledge/hermit.jpg", "hermit").is_err(),
            "{bad}"
        );
    }
    assert!(
        with("{en: Hermit}", "media/portraits/hermit.jpg", "hermit").is_err(),
        "pictures live below media/knowledge/"
    );
    assert!(
        with("{en: Hermit}", "media/knowledge/missing.jpg", "hermit").is_err(),
        "a missing picture"
    );
}

#[test]
fn an_npc_model_is_shipped_validated_and_announced() {
    let a = scratch("model");
    base(&a, "zeta", "1.0.0", "");
    let with_model = |model: &str| {
        write(
            &a,
            "npcs/n.yaml",
            &format!("- {{id: hermit, name_key: npc.hermit, place: hut, character: {{pack: retro, skin: humanMaleA, model: {model}}}, dialogue: hermit}}\n"),
        );
    };
    with_model("media/models/hermit.glb");
    assert!(
        Story::load(std::slice::from_ref(&a), false).is_err(),
        "a model that is not shipped must be rejected"
    );
    write(&a, "media/models/hermit.glb", "glTF");
    let story = Story::load(std::slice::from_ref(&a), false).unwrap();
    assert!(story.media.contains_key("zeta/media/models/hermit.glb"));
    let big = vec![0u8; (super::disk::MAX_MODEL_BYTES + 1) as usize];
    fs::write(a.join("media/models/hermit.glb"), big).unwrap();
    assert!(
        Story::load(std::slice::from_ref(&a), false).is_err(),
        "oversized model"
    );
    fs::write(a.join("media/models/hermit.glb"), "glTF").unwrap();
    write(&a, "media/other/hermit.glb", "glTF");
    with_model("media/other/hermit.glb");
    assert!(
        Story::load(std::slice::from_ref(&a), false).is_err(),
        "models belong below media/models/"
    );
}

#[test]
fn spoken_lines_are_found_by_language_and_text_key_and_limited_in_size() {
    let a = scratch("voice");
    base(&a, "zeta", "1.0.0", "");
    write(&a, "media/voice/cs/hi.ogg", "OggS");
    write(&a, "media/voice/cs/unused.ogg", "OggS");
    let story = Story::load(std::slice::from_ref(&a), false).unwrap();
    assert_eq!(
        story.voices[&("cs".to_owned(), "zeta:hi".to_owned())],
        "zeta/media/voice/cs/hi.ogg"
    );
    assert!(
        !story
            .voices
            .contains_key(&("en".to_owned(), "zeta:hi".to_owned())),
        "no English recording"
    );
    assert!(
        story.media.contains_key("zeta/media/voice/cs/hi.ogg")
            && !story.media.contains_key("zeta/media/voice/cs/unused.ogg"),
        "only lines a node shows are served"
    );
    let big = vec![0u8; (super::disk::MAX_VOICE_BYTES + 1) as usize];
    fs::write(a.join("media/voice/cs/hi.ogg"), big).unwrap();
    assert!(
        Story::load(std::slice::from_ref(&a), false).is_err(),
        "oversized line"
    );
}

#[test]
fn spoken_lines_do_not_change_the_pinned_datadisk_identity() {
    let a = scratch("voice-pin");
    base(&a, "zeta", "1.0.0", "");
    let without = Story::load(std::slice::from_ref(&a), false).unwrap();
    write(&a, "media/voice/cs/hi.ogg", "OggS spoken");
    let with = Story::load(std::slice::from_ref(&a), false).unwrap();
    assert_eq!(
        without.disks[0].2, with.disks[0].2,
        "adding a Piper dub must not invalidate an already-generated world"
    );
    assert!(with
        .voices
        .contains_key(&("cs".to_owned(), "zeta:hi".to_owned())));
}

#[test]
fn rejects_missing_dependencies_conflicts_and_drafts() {
    let a = scratch("dep");
    base(
        &a,
        "needy",
        "1.0.0",
        "requires:\n  - {disk: ghost, version: 1.0.0}\n",
    );
    assert!(Story::load(std::slice::from_ref(&a), false)
        .unwrap_err()
        .to_string()
        .contains("requires ghost"));

    let b = scratch("c1");
    let c = scratch("c2");
    base(&b, "one", "1.0.0", "conflicts: [two]\n");
    base(&c, "two", "1.0.0", "");
    assert!(Story::load(&[b, c], false)
        .unwrap_err()
        .to_string()
        .contains("conflicts"));

    let d = scratch("old");
    let e = scratch("new");
    base(&d, "lib", "1.0.0", "");
    base(
        &e,
        "app",
        "1.0.0",
        "requires:\n  - {disk: lib, version: 2.0.0}\n",
    );
    assert!(Story::load(&[d, e], false)
        .unwrap_err()
        .to_string()
        .contains("same major"));

    let f = scratch("draft");
    base(&f, "drafty", "1.0.0", "");
    let text = fs::read_to_string(f.join("dialogues/hermit.yaml")).unwrap();
    write(
        &f,
        "dialogues/hermit.yaml",
        &text.replace("start: hi", "status: draft\nstart: hi"),
    );
    assert!(Disk::load(&f, false)
        .unwrap_err()
        .to_string()
        .contains("draft"));
    assert!(Disk::load(&f, true).is_ok());
}

#[test]
fn rejects_broken_references_and_cross_disk_leaks() {
    let a = scratch("goto");
    base(&a, "broken", "1.0.0", "");
    let text = fs::read_to_string(a.join("dialogues/hermit.yaml")).unwrap();
    write(
        &a,
        "dialogues/hermit.yaml",
        &text.replace(
            "{text_key: bye, if: {not: {flag: met}}}",
            "{text_key: bye, goto: nowhere}",
        ),
    );
    assert!(Disk::load(&a, false)
        .unwrap_err()
        .to_string()
        .contains("unknown node"));

    let b = scratch("leak");
    base(&b, "leaky", "1.0.0", "");
    write(&b, "npcs/n.yaml", "- {id: hermit, name_key: npc.hermit, place: \"other:hut\", character: {pack: retro, skin: humanMaleA}, dialogue: hermit}\n");
    let error = Story::load(&[b], false).unwrap_err().to_string();
    assert!(error.contains("not a declared dependency"), "{error}");
}

fn dialogue_story() -> Story {
    let a = scratch("play");
    base(&a, "play", "1.0.0", "");
    Story::load(&[a], false).unwrap()
}

#[test]
fn choices_apply_effects_once_and_hide_unavailable_options() {
    let story = dialogue_story();
    let dialogue = &story.dialogues["play:hermit"];
    let mut state = PlayerState::default();
    state.items.insert("gold".into(), 100);

    let mut ops = Vec::new();
    let shown = engine::enter(dialogue, "hi", &mut state, &mut ops)
        .unwrap()
        .unwrap();
    assert_eq!(shown.choices.len(), 2);

    let result = engine::choose(dialogue, "hi", 0, &mut state, &mut ops);
    assert_eq!(result, Choose::End);
    assert_eq!(state.items["gold"], 115, "-5 then +20 once");
    assert_eq!(state.stages["play:q"], "s2");
    assert!(ops.contains(&Op::Once("play:reward".into())));

    // Repeating the choice pays 5 again but grants the once-reward only the first time.
    let mut again = Vec::new();
    assert_eq!(
        engine::choose(dialogue, "hi", 0, &mut state, &mut again),
        Choose::End
    );
    assert_eq!(state.items["gold"], 110);
    assert!(!again.iter().any(|op| matches!(op, Op::Once(_))));

    // The second choice is hidden once the flag is set.
    let mut ops = Vec::new();
    let shown = engine::enter(dialogue, "hi", &mut state, &mut ops)
        .unwrap()
        .unwrap();
    assert_eq!(
        shown.choices.iter().map(|c| c.index).collect::<Vec<_>>(),
        [0]
    );
    assert_eq!(
        engine::choose(dialogue, "hi", 1, &mut state, &mut ops),
        Choose::Unavailable
    );
    assert_eq!(
        engine::choose(dialogue, "hi", 9, &mut state, &mut ops),
        Choose::Unavailable
    );
}

#[test]
fn unaffordable_effects_change_nothing() {
    let story = dialogue_story();
    let dialogue = &story.dialogues["play:hermit"];
    let mut state = PlayerState::default();
    state.items.insert("gold".into(), 3);
    let mut ops = Vec::new();
    // Choice 0 needs 5 gold to be visible at all.
    assert_eq!(
        engine::choose(dialogue, "hi", 0, &mut state, &mut ops),
        Choose::Unavailable
    );
    assert_eq!(state.items["gold"], 3);
    assert!(ops.is_empty());
}

#[test]
fn the_gilgamesh_disk_lists_the_houses_of_uruk() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ishtaria-datadisk-gilgamesh");
    if !path.join("buildings/uruk.yaml").is_file() {
        return;
    }
    let story = Story::load(&[path], true).unwrap();
    let (place, disk, items) = story
        .buildings
        .iter()
        .find(|(place, _, _)| place == "gilgamesh:uruk")
        .expect("Uruk lists its houses");
    assert!(story
        .buildings
        .iter()
        .any(|(place, _, items)| place == "gilgamesh:hunter_village" && !items.is_empty()));
    assert_eq!(
        (place.as_str(), disk.as_str()),
        ("gilgamesh:uruk", "gilgamesh")
    );
    assert!(items.len() > 1000);
    assert!(items.iter().all(|item| story
        .media
        .contains_key(&format!("gilgamesh/{}", item.model))));
}

#[test]
fn the_gilgamesh_disk_loads_wildlife_crowd_and_cover_music() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ishtaria-datadisk-gilgamesh");
    if !path.join("datadisk.yaml").is_file() {
        return;
    }
    let story = Story::load(std::slice::from_ref(&path), false).unwrap();
    let info = story
        .infos
        .iter()
        .find(|info| info.id == "gilgamesh")
        .expect("gilgamesh announced");
    assert!(info.cover_music.is_some(), "logo jingle");
    assert!(info.ambient_music.is_some(), "plains ambient");
    assert!(
        story
            .easter_eggs
            .iter()
            .any(|egg| egg.id == "gilgamesh:anu.dance" && egg.npc == "gilgamesh:anu"),
        "anu night dance egg"
    );
    assert!(
        story.dialogues.values().any(|dialogue| {
            dialogue.nodes.values().any(|node| {
                node.choices.as_ref().is_some_and(|choices| {
                    choices
                        .iter()
                        .any(|c| c.effects.iter().any(|e| e.flourish.is_some()))
                }) || node.effects.iter().any(|e| e.flourish.is_some())
            })
        }),
        "gilgamesh dialogues include flourish set-pieces"
    );
    let uruk = story
        .anchors
        .iter()
        .find(|a| a.id == "gilgamesh:uruk")
        .expect("uruk place");
    let wildlife = uruk.place.wildlife.as_ref().expect("uruk wildlife overlay");
    assert!(
        wildlife
            .flora
            .iter()
            .any(|entry| entry.id == "nature.tree_palmTall"),
        "date palms in the overlay"
    );
    assert!(
        wildlife
            .fauna
            .iter()
            .any(|entry| entry.id == "animal.duck" || entry.id == "animal.goose"),
        "waterfowl in the overlay"
    );
    assert!(
        story.npcs.contains_key("gilgamesh:crowd_fisherman")
            && story.npcs["gilgamesh:crowd_fisherman"].place == "gilgamesh:river_bank",
        "fisherman on the river bank"
    );
    assert!(
        story.npcs.contains_key("gilgamesh:crowd_old_man")
            && story.npcs.contains_key("gilgamesh:crowd_boy_8"),
        "crowd NPCs with age-appropriate walks"
    );
    assert_eq!(
        story.npcs["gilgamesh:crowd_old_man"]
            .movement
            .as_ref()
            .map(|m| m.speed_m_s),
        Some(0.48)
    );
    assert!(
        story.npcs["gilgamesh:crowd_boy_8"]
            .movement
            .as_ref()
            .map(|m| m.speed_m_s)
            .unwrap_or(0.0)
            > 1.5
    );
    assert!(story.music.contains_key("gilgamesh:logo"));
    assert!(story.music.contains_key("gilgamesh:city"));
}

#[test]
fn the_endland_disk_is_valid_and_its_graveyard_job_plays_through() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ishtaria-datadisk-endland");
    if !path.join("datadisk.yaml").is_file() {
        return;
    }
    let story = Story::load(&[path], false).unwrap();
    assert!(story
        .anchors
        .iter()
        .any(|a| a.id == "endland:old_graveyard"));
    let mut state = PlayerState::default();
    state.items.insert("gold".into(), 100);
    let mut ops = Vec::new();

    let faust = &story.dialogues["endland:faust"];
    let shown = engine::enter(faust, "route", &mut state, &mut ops)
        .unwrap()
        .unwrap();
    assert_eq!(shown.node, "greet");
    engine::choose(faust, "greet", 0, &mut state, &mut ops); // ask for work
    engine::choose(faust, "work", 0, &mut state, &mut ops); // sent to Fawn
    assert_eq!(state.stages["endland:graveyard_job"], "heard_rumour");

    let fawn = &story.dialogues["endland:fawn"];
    let shown = engine::enter(fawn, "route", &mut state, &mut ops)
        .unwrap()
        .unwrap();
    assert_eq!(shown.node, "offer");
    assert!(matches!(
        engine::choose(fawn, "offer", 0, &mut state, &mut ops),
        Choose::Next(_)
    ));
    assert_eq!(state.items["gold"], 90);
    assert_eq!(state.stages["endland:graveyard_job"], "have_key");

    let shown = engine::enter(faust, "route", &mut state, &mut ops)
        .unwrap()
        .unwrap();
    assert_eq!(shown.node, "has_key");
    assert!(matches!(
        engine::choose(faust, "has_key", 0, &mut state, &mut ops),
        Choose::Next(_)
    ));
    assert_eq!(state.items["gold"], 115);
    assert_eq!(state.items["cheese"], 2);
    assert_eq!(state.stages["endland:graveyard_job"], "paid_off");
}

/// A planet whose northern hemisphere is gentle grassland, the rest sea.
struct Flat;

impl super::placement::Ground for Flat {
    fn land_site(&self, d: [f64; 3]) -> Option<(u8, f64)> {
        (d[1] > 0.1).then_some((if d[0] > 0.0 { 4 } else { 5 }, 50.0 + 30.0 * d[1]))
    }
    fn height(&self, d: [f64; 3]) -> f64 {
        50.0 + 30.0 * d[1]
    }
    fn is_sea(&self, d: [f64; 3]) -> bool {
        d[1] <= 0.1
    }
}

#[test]
fn placement_is_deterministic_and_honours_distances() {
    use super::placement::{distance_m, place_anchors};
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ishtaria-datadisk-endland");
    if !path.join("datadisk.yaml").is_file() {
        return;
    }
    let story = Story::load(&[path], false).unwrap();
    let first = place_anchors(&story, &Flat, "42", &[]).unwrap();
    assert_eq!(first, place_anchors(&story, &Flat, "42", &[]).unwrap());
    assert_ne!(first, place_anchors(&story, &Flat, "43", &[]).unwrap());
    let site = |id: &str| first.iter().find(|p| p.id == id).unwrap();
    let city = site("endland:udrury");
    let tavern = site("endland:blackhorn_tavern");
    let graveyard = site("endland:old_graveyard");
    assert!(distance_m(city.direction, tavern.direction) <= 130.0 * 0.8 * 1.0001);
    let away = distance_m(city.direction, graveyard.direction);
    assert!(
        (1000.0..=3000.0).contains(&away),
        "graveyard is {away} m from the city"
    );

    // An already placed place never moves; only the rest is added.
    let kept = place_anchors(&story, &Flat, "42", &first[..1]).unwrap();
    assert_eq!(kept, first[1..]);
}

#[test]
fn a_located_place_lies_where_history_puts_it() {
    use super::placement::{distance_m, place_anchors};
    let a = scratch("located");
    base(&a, "geo", "1.0.0", "");
    write(
        &a,
        "places/p.yaml",
        "- {id: hut, kind: building, name_key: place.hut}\n\
         - {id: forest, kind: landmark, name_key: place.hut, locate: {from: hut, bearing_deg: 90, distance_m: 2500000, tolerance_m: 150000}}\n\
         - {id: north, kind: landmark, name_key: place.hut, locate: {from: hut, bearing_deg: 0, distance_m: 1000000, tolerance_m: 100000}}\n\
         - {id: shrine, kind: building, name_key: place.hut, locate: {from: forest, bearing_deg: 180, distance_m: 400000}}\n",
    );
    let story = Story::load(&[a], false).unwrap();
    let sites = place_anchors(&story, &Flat, "7", &[]).unwrap();
    let site = |id: &str| sites.iter().find(|p| p.id == id).unwrap();
    let (hut, forest, north, shrine) = (
        site("geo:hut"),
        site("geo:forest"),
        site("geo:north"),
        site("geo:shrine"),
    );
    // Distance and bearing in the frame of the map of the place they are measured from.
    let measure = |from: &super::placement::Placed, to: &super::placement::Placed| {
        let (axis_x, axis_z) = crate::scenery::client_axes(from.direction);
        let dot = |a: [f64; 3], b: [f64; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        let (east, north) = (dot(to.direction, axis_x), -dot(to.direction, axis_z));
        let bearing = east.atan2(north).to_degrees().rem_euclid(360.0);
        (distance_m(from.direction, to.direction), bearing)
    };
    let (distance, bearing) = measure(hut, forest);
    assert!(
        (2_350_000.0..=2_650_000.0).contains(&distance),
        "forest {distance} m"
    );
    assert!((bearing - 90.0).abs() < 10.0, "forest at bearing {bearing}");
    let (distance, bearing) = measure(hut, north);
    assert!(
        (900_000.0..=1_100_000.0).contains(&distance),
        "north {distance} m"
    );
    assert!(
        !(12.0..=348.0).contains(&bearing),
        "north at bearing {bearing}"
    );
    let (distance, bearing) = measure(forest, shrine);
    assert!(
        (380_000.0..=420_000.0).contains(&distance),
        "shrine {distance} m"
    );
    assert!((bearing - 180.0).abs() < 8.0, "shrine at bearing {bearing}");
}

#[test]
fn a_located_place_is_validated() {
    let a = scratch("badloc");
    base(&a, "geo", "1.0.0", "");
    let with = |places: &str| {
        write(&a, "places/p.yaml", places);
        Story::load(std::slice::from_ref(&a), false)
    };
    let ok = "- {id: hut, kind: building, name_key: place.hut}\n";
    assert!(with(&format!("{ok}- {{id: far, kind: landmark, name_key: place.hut, locate: {{from: hut, bearing_deg: 10, distance_m: 5000}}}}\n")).is_ok());
    assert!(with(&format!("{ok}- {{id: far, kind: landmark, name_key: place.hut, locate: {{from: nowhere, bearing_deg: 10, distance_m: 5000}}}}\n")).is_err(), "unknown place");
    assert!(with(&format!("{ok}- {{id: far, kind: landmark, name_key: place.hut, locate: {{from: hut, bearing_deg: 400, distance_m: 5000}}}}\n")).is_err(), "bearing");
    assert!(with(&format!("{ok}- {{id: far, kind: landmark, name_key: place.hut, locate: {{from: hut, bearing_deg: 10, distance_m: 90000000}}}}\n")).is_err(), "further than the planet is wide");
    assert!(with(&format!("{ok}- {{id: far, kind: landmark, name_key: place.hut, locate: {{from: hut, bearing_deg: 10, distance_m: 5000, tolerance_m: 0}}}}\n")).is_ok(), "a zero tolerance is a surveyed point");
    assert!(with("- {id: hut, kind: building, name_key: place.hut, locate: {from: hut, bearing_deg: 10, distance_m: 5000}}\n").is_err(), "from itself");
}

#[test]
fn a_place_pinned_to_its_historical_site_stands_exactly_there() {
    use super::placement::{distance_m, place_anchors};
    let a = scratch("geo-pin");
    base(
        &a,
        "geopin",
        "1.0.0",
        "world:\n  terrain: {heightmap: media/world/height.pgm, water: media/world/water.pgm}\n",
    );
    write_world_rasters(&a, (16, 8), (16, 8), true);
    write(
        &a,
        "places/p.yaml",
        "- {id: hut, kind: building, name_key: place.hut}\n\
         - {id: uruk, kind: city, name_key: place.hut, geo: {latitude_deg: 31.3228, longitude_deg: 45.6389}, requires: {biome: [temperate_forest]}}\n\
         - {id: gate, kind: building, name_key: place.hut, parent: uruk, locate: {from: uruk, bearing_deg: 181, distance_m: 1370}}\n",
    );
    let story = Story::load(std::slice::from_ref(&a), false).unwrap();
    let sites = place_anchors(&story, &Flat, "7", &[]).unwrap();
    let site = |id: &str| sites.iter().find(|p| p.id == id).unwrap();
    let (uruk, gate) = (site("geopin:uruk"), site("geopin:gate"));
    // The frame of the world rasters: latitude = asin(y), longitude = atan2(-z, x).
    let latitude = 31.3228_f64.to_radians();
    let longitude = 45.6389_f64.to_radians();
    let expected = [
        latitude.cos() * longitude.cos(),
        latitude.sin(),
        -latitude.cos() * longitude.sin(),
    ];
    assert!(
        distance_m(uruk.direction, expected) < 1.0,
        "the historical site is where its coordinates say"
    );
    // History decides, so a requires the ground does not satisfy is advice, not a constraint.
    assert_eq!(uruk.height_m, 50.0 + 30.0 * expected[1]);
    let away = distance_m(uruk.direction, gate.direction);
    assert!(
        (1200.0..=1500.0).contains(&away),
        "places around it locate themselves from it: {away} m"
    );
}

#[test]
fn a_surveyed_point_stands_exactly_where_it_was_measured() {
    use super::placement::{distance_m, offset_local, place_anchors};
    let a = scratch("survey");
    base(
        &a,
        "survey",
        "1.0.0",
        "world:\n  terrain: {heightmap: media/world/height.pgm, water: media/world/water.pgm}\n",
    );
    write_world_rasters(&a, (16, 8), (16, 8), true);
    write(
        &a,
        "places/p.yaml",
        "- {id: hut, kind: building, name_key: place.hut}\n\
         - {id: uruk, kind: city, name_key: place.hut, geo: {latitude_deg: 31.3228, longitude_deg: 45.6389}}\n\
         - {id: harbour, kind: landmark, name_key: place.hut, locate: {from: uruk, bearing_deg: 180, distance_m: 3500000, tolerance_m: 0}, requires: {biome: [grassland], height_m: {min: 100, max: 200}}}\n",
    );
    let story = Story::load(std::slice::from_ref(&a), false).unwrap();
    let sites = place_anchors(&story, &Flat, "7", &[]).unwrap();
    let site = |id: &str| sites.iter().find(|p| p.id == id).unwrap();
    let (uruk, harbour) = (site("survey:uruk"), site("survey:harbour"));
    // A zero tolerance is a surveyed point: no search, not even for water it may not find.
    let expected = offset_local(uruk.direction, 3_500_000.0, 180.0);
    assert!(
        distance_m(harbour.direction, expected) < 1.0,
        "the surveyed point is exactly where it was measured"
    );
    assert_eq!(harbour.height_m, 50.0 + 30.0 * expected[1]);
}

#[test]
fn a_historical_pin_is_validated() {
    let a = scratch("geo-bad");
    base(
        &a,
        "geobad",
        "1.0.0",
        "world:\n  terrain: {heightmap: media/world/height.pgm, water: media/world/water.pgm}\n",
    );
    write_world_rasters(&a, (16, 8), (16, 8), true);
    let with = |places: &str| {
        write(&a, "places/p.yaml", places);
        Story::load(std::slice::from_ref(&a), false)
    };
    let pinned = "- {id: hut, kind: building, name_key: place.hut}\n- {id: uruk, kind: city, name_key: place.hut, geo: {latitude_deg: 31.32, longitude_deg: 45.64}}\n";
    assert!(with(pinned).is_ok(), "a pinned site on the real planet");
    let without_planet = scratch("geo-noplanet");
    base(&without_planet, "geonone", "1.0.0", "");
    write(
        &without_planet,
        "places/p.yaml",
        "- {id: hut, kind: building, name_key: place.hut}\n- {id: uruk, kind: city, name_key: place.hut, geo: {latitude_deg: 31.32, longitude_deg: 45.64}}\n",
    );
    let error = Story::load(std::slice::from_ref(&without_planet), false).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("does not define the world's real planet"),
        "{error}"
    );
    for (what, places) in [
        ("also located", "- {id: hut, kind: building, name_key: place.hut}\n- {id: uruk, kind: city, name_key: place.hut, geo: {latitude_deg: 31.32, longitude_deg: 45.64}, locate: {from: hut, bearing_deg: 10, distance_m: 5000}}\n"),
        ("also near", "- {id: hut, kind: building, name_key: place.hut}\n- {id: uruk, kind: city, name_key: place.hut, geo: {latitude_deg: 31.32, longitude_deg: 45.64}, requires: {near: [{place: hut, min_m: 1, max_m: 2}]}}\n"),
        ("out of range", "- {id: hut, kind: building, name_key: place.hut}\n- {id: uruk, kind: city, name_key: place.hut, geo: {latitude_deg: 140.0, longitude_deg: 45.64}}\n"),
    ] {
        assert!(with(places).is_err(), "{what}");
    }
}

#[test]
fn placement_fails_clearly_when_nothing_fits() {
    use super::placement::place_anchors;
    struct Sea;
    impl super::placement::Ground for Sea {
        fn land_site(&self, _: [f64; 3]) -> Option<(u8, f64)> {
            None
        }
        fn height(&self, _: [f64; 3]) -> f64 {
            0.0
        }
        fn is_sea(&self, _: [f64; 3]) -> bool {
            true
        }
    }
    let story = dialogue_story();
    let error = place_anchors(&story, &Sea, "1", &[])
        .unwrap_err()
        .to_string();
    assert!(error.contains("play:hut"), "{error}");
}

#[test]
fn the_endland_intro_asks_the_name_and_hands_out_the_aetherglass_once() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ishtaria-datadisk-endland");
    if !path.join("datadisk.yaml").is_file() {
        return;
    }
    let story = Story::load(&[path], false).unwrap();
    let faust = &story.dialogues["endland:faust_intro"];
    for name_choice in 0..2 {
        let mut state = PlayerState::default();
        let mut ops = Vec::new();
        let shown = engine::enter(faust, "route", &mut state, &mut ops)
            .unwrap()
            .unwrap();
        assert_eq!(shown.node, "wake");
        assert_eq!(state.stages["endland:intro"], "woke");
        assert_eq!(shown.choices.len(), 4, "four ways to answer the wake-up");
        engine::choose(faust, "wake", 3, &mut state, &mut ops); // I wish I had stayed dead
        engine::choose(faust, "wish_dead", 0, &mut state, &mut ops);
        assert!(matches!(
            engine::choose(faust, "ask_name", name_choice, &mut state, &mut ops),
            Choose::Next(_)
        ));
        assert_eq!(state.stages["endland:intro"], "named");
        let reply = ["refused", "told"][name_choice];
        engine::choose(faust, reply, 0, &mut state, &mut ops);
        engine::choose(faust, "aether", 1, &mut state, &mut ops);
        assert_eq!(state.items["aetherglass"], 1);
        assert_eq!(state.stages["endland:intro"], "got_aether");
        // Meeting him again only gives directions, and never a second tablet.
        let shown = engine::enter(faust, "route", &mut state, &mut ops)
            .unwrap()
            .unwrap();
        assert_eq!(shown.node, "after");
        let veil_first = engine::choose(faust, "after", 0, &mut state, &mut ops);
        assert_eq!(veil_first, Choose::End);
        assert_eq!(state.items["aetherglass"], 1);
    }
}

#[test]
fn every_world_grows_towns_with_graveyards_and_harbours_with_shipwrights() {
    use super::{placement::place_anchors, settlements};
    let disk = settlements::world_disk("42", 6);
    let gates = disk.places.iter().filter(|p| p.at.is_some()).count();
    assert_eq!(
        disk.places.len(),
        13 + gates,
        "a town and a graveyard each, a fortress for every fourth town and a gate for each walled one"
    );
    assert_eq!(disk.npcs.len(), gates, "a guard at every gate");
    let mut story = Story::compose(vec![disk]).unwrap();
    assert!(
        story
            .anchors
            .iter()
            .filter(|a| a.place.kind == "graveyard")
            .all(|a| a.place.music.as_deref() == Some("world:graveyard")),
        "every generated graveyard plays the server's default track"
    );
    assert!(story.media.contains_key("world/media/music/graveyard.ogg"));
    let mut sites: std::collections::HashMap<_, _> = place_anchors(&story, &Flat, "42", &[])
        .unwrap()
        .into_iter()
        .map(|site| (site.id.clone(), site))
        .collect();
    assert_eq!(sites.len(), 13 + gates);
    let built = settlements::finish(&mut story, &mut sites, &Flat, "42").unwrap();
    let props = &built.props;
    assert!(
        built.colliders.len() > 100 && built.colliders.iter().all(|c| c.collision_radius_m > 0.0)
    );
    assert!(props.iter().any(|p| p.model == "graveyard.crypt-large"));
    assert!(props.iter().any(|p| p.model == "town.fountain-round"));
    assert!(props.iter().any(|p| p.model == "quaternius.castle_gate"));
    assert!(props.iter().any(|p| p.model == "quaternius.temple"));
    // The generator lays footpaths where no datadisk defines roads: stones of the road module, away from the plazas.
    let stones: Vec<_> = props
        .iter()
        .filter(|p| p.id.starts_with("trail:"))
        .collect();
    assert!(
        stones.len() > 20,
        "towns are joined to their graveyards: {}",
        stones.len()
    );
    assert!(stones
        .iter()
        .all(|p| p.model == "town.road" && p.scale_m == 2.5));
    let mut again_story = Story::compose(vec![settlements::world_disk("42", 6)]).unwrap();
    let mut again_sites = sites.clone();
    let again = settlements::finish(&mut again_story, &mut again_sites, &Flat, "42").unwrap();
    assert_eq!(
        again
            .props
            .iter()
            .filter(|p| p.id.starts_with("trail:"))
            .count(),
        stones.len(),
        "a rebuild of the story gives the same trails"
    );
    assert_eq!(
        story.strings["cs"]["world:fort_00.name"].split(' ').next(),
        Some("Pevnost")
    );
    let shipwrights: Vec<_> = story
        .npcs
        .keys()
        .filter(|id| id.contains("shipwright"))
        .collect();
    assert!(!shipwrights.is_empty(), "coastal towns have a shipwright");
    assert!(story.dialogues.contains_key("harbours:shipwright"));
    // Every town has a trader at its plaza who keeps the general shop.
    let traders: Vec<_> = story
        .npcs
        .values()
        .filter(|npc| npc.id.contains("trader"))
        .collect();
    assert!(!traders.is_empty(), "towns have a trader");
    assert!(traders
        .iter()
        .all(|npc| npc.tags.contains(&"shop:general".to_owned())));
    assert!(story.dialogues.contains_key("markets:trader"));
    assert_eq!(story.strings["cs"]["markets:trader.name"], "Obchodník");
    let smiths: Vec<_> = story
        .npcs
        .values()
        .filter(|npc| npc.id.contains("blacksmith"))
        .collect();
    assert!(!smiths.is_empty(), "villages and towns have a blacksmith");
    assert!(smiths
        .iter()
        .all(|npc| npc.tags.contains(&"shop:smith".to_owned())));
    assert_eq!(story.strings["cs"]["forges:blacksmith.name"], "Kovář");
    // Every town on land has a shepherd who drives its flock of sheep and goats to a pasture and home.
    let shepherds: Vec<_> = story
        .npcs
        .values()
        .filter(|npc| npc.id.contains("shepherd"))
        .collect();
    assert!(!shepherds.is_empty(), "towns have a shepherd");
    assert!(shepherds.iter().all(|npc| npc
        .movement
        .as_ref()
        .is_some_and(
            |movement| movement.kind == MovementKind::Herd && movement.waypoints.len() == 1
        )));
    assert!(story.dialogues.contains_key("shepherds:shepherd"));
    // A datadisk character tagged `look_shepherd` lends its look to the generated shepherds, and only to them.
    let mut lender = shepherds[0].clone();
    lender.id = "disk:crowd_shepherd".to_owned();
    lender.tags = vec!["look_shepherd".to_owned()];
    lender.character.model = Some("disk/media/models/crowd_shepherd.glb".to_owned());
    let trader_look = traders[0].character.model.clone();
    let shepherd_ids: Vec<String> = shepherds.iter().map(|npc| npc.id.clone()).collect();
    let mut dressed = story.clone();
    dressed.npcs.insert(lender.id.clone(), lender);
    settlements::wear_disk_looks(&mut dressed);
    assert!(shepherd_ids
        .iter()
        .all(|id| dressed.npcs[id].character.model.as_deref()
            == Some("disk/media/models/crowd_shepherd.glb")));
    assert!(dressed
        .npcs
        .values()
        .filter(|npc| npc.id.starts_with("markets:trader_"))
        .all(|npc| npc.character.model == trader_look));
    // The characters of generated towns tell their knowledge cards, with a picture and an article to read.
    for npc in story.npcs.values().filter(|npc| {
        ["trader", "shepherd", "guard", "shipwright"]
            .iter()
            .any(|role| npc.id.contains(role))
    }) {
        let card = &story.knowledge[npc.about.as_ref().expect("a card to tell")];
        assert!(story.media.contains_key(&card.image), "{}", card.image);
        assert!(card.wikipedia.contains_key("en") && card.wikipedia.contains_key("cs"));
        for language in ["en", "cs"] {
            for key in [&card.title_key, &card.text_key, &card.credit_key] {
                assert!(
                    story.strings[language].contains_key(key),
                    "{language} {key}"
                );
            }
        }
    }
    assert_eq!(story.strings["cs"]["shepherds:shepherd.name"], "Pastevec");
    assert_eq!(
        built
            .farms
            .iter()
            .filter(|farm| farm.pasture.is_some())
            .count(),
        shepherds.len()
    );
    assert_eq!(story.strings["cs"]["harbours:shipwright.name"], "Loďmistr");
    // The shop is plain dialogue: ships for gold, hidden while the player is poor.
    let dialogue = &story.dialogues["harbours:shipwright"];
    let mut poor = PlayerState::default();
    poor.items.insert("gold".into(), 100);
    let shown = engine::enter(dialogue, "greet", &mut poor, &mut Vec::new())
        .unwrap()
        .unwrap();
    assert_eq!(shown.choices.len(), 2, "a small boat and leaving");
    let mut rich = PlayerState::default();
    rich.items.insert("gold".into(), 1000);
    let mut ops = Vec::new();
    assert!(matches!(
        engine::choose(dialogue, "greet", 0, &mut rich, &mut ops),
        Choose::Next(_)
    ));
    assert_eq!(rich.items["gold"], 940);
    assert_eq!(rich.items["boat_row_small"], 1);
}

#[test]
fn the_spawn_point_of_a_disk_is_a_free_spot_inside_its_place() {
    use super::{
        placement::place_anchors,
        settlements,
        world::{find_spawn, npc_descriptors},
    };
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ishtaria-datadisk-endland");
    if !path.join("datadisk.yaml").is_file() {
        return;
    }
    let mut story = Story::load(&[path], false).unwrap();
    let mut sites: std::collections::HashMap<_, _> = place_anchors(&story, &Flat, "42", &[])
        .unwrap()
        .into_iter()
        .map(|site| (site.id.clone(), site))
        .collect();
    let built = settlements::finish(&mut story, &mut sites, &Flat, "42").unwrap();
    let npcs = npc_descriptors(&story, &sites, &Flat);
    // The model of Faust is announced under the id of its disk, like his portrait, or the client
    // asks for a file that does not exist and keeps the pack character.
    let faust = npcs
        .iter()
        .find(|npc| npc.id == "endland:faust_graveyard")
        .expect("Faust stands in the graveyard");
    assert_eq!(
        faust.model.as_deref(),
        Some("/story/media/endland/media/models/faust.glb")
    );
    assert_eq!(
        faust.portrait.as_deref(),
        Some("/story/media/endland/media/portraits/faust.jpg")
    );
    let spawn =
        find_spawn(&story, &sites, &built.colliders, &npcs, &Flat, "42").expect("a spawn place");
    let distance =
        |a: [f64; 3], b: [f64; 3]| (0..3).map(|i| (a[i] - b[i]).powi(2)).sum::<f64>().sqrt();
    let site = &sites["endland:old_graveyard"];
    let centre = crate::scenery::world_from_local(
        site.direction,
        0.0,
        0.0,
        |d| Flat.height(d).max(0.0),
        crate::movement::RADIUS,
    )
    .1;
    assert!(
        distance(spawn, centre) < 13.0,
        "inside the fence: {} m",
        distance(spawn, centre)
    );
    for obstacle in &built.colliders {
        assert!(
            distance(spawn, obstacle.position) > obstacle.collision_radius_m + 1.0,
            "clear of {}",
            obstacle.id
        );
    }
    for npc in &npcs {
        assert!(distance(spawn, npc.position) > 1.9, "clear of {}", npc.id);
    }
    // The same world always gives the same spot.
    assert_eq!(
        Some(spawn),
        find_spawn(&story, &sites, &built.colliders, &npcs, &Flat, "42")
    );
}

#[test]
fn markers_point_at_the_places_of_active_quests_only() {
    use super::{api::markers_of, placement::place_anchors};
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ishtaria-datadisk-endland");
    if !path.join("datadisk.yaml").is_file() {
        return;
    }
    let story = Story::load(&[path], false).unwrap();
    let sites: std::collections::HashMap<_, _> = place_anchors(&story, &Flat, "42", &[])
        .unwrap()
        .into_iter()
        .map(|site| (site.id.clone(), site))
        .collect();
    let row = |quest: &str, stage: &str| (format!("endland:{quest}"), stage.to_owned());

    // Nothing started: the job waits at the north gate, where Faust stands.
    let found = markers_of(&story, &sites, &[row("intro", "got_aether")]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, "endland:udrury_gate");
    assert!(found[0].next);

    // The rumour sends the player to Fawn in the back alley, the key back to Faust at the gate.
    let found = markers_of(&story, &sites, &[row("graveyard_job", "heard_rumour")]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, "endland:udrury_alley");
    assert!(!found[0].next);
    let found = markers_of(&story, &sites, &[row("graveyard_job", "have_key")]);
    assert_eq!(found[0].id, "endland:udrury_gate");

    // A finished quest points nowhere.
    assert!(markers_of(&story, &sites, &[row("graveyard_job", "paid_off")]).is_empty());
}

#[test]
fn hunter_fifth_snare_points_at_the_fathers_camp() {
    use super::{api::markers_of, placement::place_anchors};
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ishtaria-datadisk-gilgamesh");
    if !path.join("datadisk.yaml").is_file() {
        return;
    }
    let story = Story::load(&[path], true).unwrap();
    let sites: std::collections::HashMap<_, _> = place_anchors(&story, &Flat, "42", &[])
        .unwrap()
        .into_iter()
        .map(|site| (site.id.clone(), site))
        .collect();
    assert!(sites.contains_key("gilgamesh:hunter_village"));
    let quest = "gilgamesh:the_hunter_and_the_wild_man";
    for stage in ["snare_ruined_5", "ask_father"] {
        let found = markers_of(&story, &sites, &[(quest.to_owned(), stage.to_owned())]);
        let guide = found
            .iter()
            .find(|marker| marker.quest == quest)
            .expect("active hunter quest has a guide");
        assert_eq!(guide.id, "gilgamesh:hunter_village");
        assert!(!guide.next);
    }
    // On the way to Uruk the water-hole is no goal any more but stays on the map as a known place.
    let rows = [(quest.to_owned(), "seek_gilgamesh".to_owned())];
    // Without the aetherglass only the goals of begun quests are shown.
    let mut current = markers_of(&story, &sites, &rows);
    current.retain(|marker| !marker.next);
    assert!(current
        .iter()
        .all(|marker| marker.id != "gilgamesh:hunter_waterhole"));
    let known = [("gilgamesh:hunter_waterhole".to_owned(), quest.to_owned())];
    let kept = super::api::known_markers(&story, &sites, &known, &current);
    assert_eq!(kept.len(), 1);
    assert!(kept[0].known && !kept[0].next);
    // A known place that is the current goal again is not drawn twice.
    let back = markers_of(
        &story,
        &sites,
        &[(quest.to_owned(), "find_enkidu".to_owned())],
    );
    assert!(super::api::known_markers(&story, &sites, &known, &back).is_empty());
}

#[test]
fn the_endland_characters_stand_in_different_parts_of_the_town() {
    use super::{
        placement::{distance_m, place_anchors},
        world::npc_descriptors,
    };
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ishtaria-datadisk-endland");
    if !path.join("datadisk.yaml").is_file() {
        return;
    }
    let story = Story::load(&[path], false).unwrap();
    let sites: std::collections::HashMap<_, _> = place_anchors(&story, &Flat, "42", &[])
        .unwrap()
        .into_iter()
        .map(|site| (site.id.clone(), site))
        .collect();
    let npcs = npc_descriptors(&story, &sites, &Flat);
    let town: Vec<_> = npcs
        .iter()
        .filter(|npc| npc.id != "endland:faust_graveyard")
        .collect();
    assert_eq!(town.len(), 5);
    // Faust and the guard wait at the north gate; Fawn hides in a back alley far away, out of the
    // guard's sight and hearing; the others stand elsewhere in the town.
    let at_gate = |id: &str| ["endland:faust", "endland:gate_guard"].contains(&id);
    let metres = |first: &[f64; 3], second: &[f64; 3]| {
        ((0..3).map(|a| (first[a] - second[a]).powi(2)).sum::<f64>()).sqrt()
    };
    for (index, first) in town.iter().enumerate() {
        for second in &town[index + 1..] {
            if at_gate(&first.id) && at_gate(&second.id) {
                assert!(metres(&first.position, &second.position) < 30.0);
                continue;
            }
            let apart = metres(&first.position, &second.position);
            assert!(
                apart > 10.0,
                "{} and {} stand {apart} m apart",
                first.id,
                second.id
            );
        }
    }
    let city = &sites["endland:udrury"];
    for place in [
        "udrury_market",
        "udrury_alley",
        "udrury_gate",
        "blackhorn_tavern",
    ] {
        assert!(
            distance_m(city.direction, sites[&format!("endland:{place}")].direction)
                <= 130.0 * 0.8 * 1.0001
        );
    }
}

#[test]
fn every_walled_town_has_a_guard_standing_at_its_gate() {
    use super::{placement::place_anchors, settlements, world::npc_descriptors};
    let disk = settlements::world_disk("42", 40);
    let story = Story::compose(vec![disk]).unwrap();
    let sites: std::collections::HashMap<_, _> = place_anchors(&story, &Flat, "42", &[])
        .unwrap()
        .into_iter()
        .map(|site| (site.id.clone(), site))
        .collect();
    let guards: Vec<_> = story
        .npcs
        .keys()
        .filter(|id| id.contains("guard_"))
        .collect();
    assert!(!guards.is_empty(), "some of forty settlements are towns");
    let npcs = npc_descriptors(&story, &sites, &Flat);
    for id in guards {
        let town = id.replace("guard_", "town_");
        let guard = npcs.iter().find(|npc| &npc.id == id).expect("a guard");
        let length = guard.position.iter().map(|v| v * v).sum::<f64>().sqrt();
        let apart = super::placement::distance_m(
            guard.position.map(|v| v / length),
            sites[&town].direction,
        );
        // Inside the wall (apothem 95 m), near the gate in the north (-Z).
        assert!(
            apart > 70.0 && apart < 100.0,
            "{id} stands {apart} m from the centre"
        );
    }
}

#[test]
fn a_structure_is_shipped_validated_and_excludes_generated_scenery() {
    let a = scratch("structure");
    base(&a, "zeta", "1.0.0", "");
    let with_structure = |extra: &str| {
        write(
            &a,
            "places/p.yaml",
            &format!("- {{id: hut, kind: building, name_key: place.hut, structure: {{model: media/models/architecture/hut.glb{extra}}}}}\n"),
        );
    };
    with_structure("");
    assert!(
        Story::load(std::slice::from_ref(&a), false).is_err(),
        "a model that is not shipped must be rejected"
    );
    write(&a, "media/models/architecture/hut.glb", "glTF");
    let story = Story::load(std::slice::from_ref(&a), false).unwrap();
    assert!(story
        .media
        .contains_key("zeta/media/models/architecture/hut.glb"));
    let structure = story.anchors[0].place.structure.as_ref().unwrap();
    assert_eq!(structure.model, "media/models/architecture/hut.glb");
    assert!(!structure.at_sea);
    with_structure(", at_sea: true");
    let afloat = Story::load(std::slice::from_ref(&a), false).unwrap();
    assert!(afloat.anchors[0].place.structure.as_ref().unwrap().at_sea);
    assert!(!afloat.anchors[0].place.structure.as_ref().unwrap().on_water);
    with_structure(", on_water: true");
    let on_river = Story::load(std::slice::from_ref(&a), false).unwrap();
    assert!(
        on_river.anchors[0]
            .place
            .structure
            .as_ref()
            .unwrap()
            .on_water
    );
    for bad in [
        ", scale_m: 0",
        ", scale_m: 500",
        ", visible_m: 10",
        ", offset_m: [9000, 0]",
        ", ground_m: [0, 5000]",
        ", collision: media/other/hut.glb",
        ", surprise: 1",
    ] {
        with_structure(bad);
        assert!(
            Story::load(std::slice::from_ref(&a), false).is_err(),
            "{bad} must be rejected"
        );
    }
    write(
        &a,
        "places/p.yaml",
        "- {id: hut, kind: building, name_key: place.hut, scenery: {preset: town}, structure: {model: media/models/architecture/hut.glb}}\n",
    );
    assert!(
        Story::load(std::slice::from_ref(&a), false).is_err(),
        "scenery and a structure exclude each other"
    );
    with_structure("");
    let big = vec![0u8; (super::disk::MAX_MODEL_BYTES + 1) as usize];
    fs::write(a.join("media/models/architecture/hut.glb"), big).unwrap();
    assert!(
        Story::load(std::slice::from_ref(&a), false).is_err(),
        "oversized model"
    );
}

#[test]
fn a_readable_structure_names_a_card_and_an_achievement_of_the_disk() {
    let a = scratch("reading");
    base(&a, "zeta", "1.0.0", "");
    write(&a, "media/models/architecture/hut.glb", "glTF");
    write(&a, "media/knowledge/tablet.jpg", "jpeg");
    write(
        &a,
        "knowledge.yaml",
        "- {id: tablet, title_key: hi, text_key: hi, image: media/knowledge/tablet.jpg, credit_key: hi, wikipedia: {en: Tablet}}\n",
    );
    let with = |structure: &str, achievements: &str| {
        write(
            &a,
            "places/p.yaml",
            &format!("- {{id: hut, kind: building, name_key: place.hut, structure: {{model: media/models/architecture/hut.glb{structure}}}}}\n"),
        );
        write(&a, "achievements.yaml", achievements);
        Story::load(std::slice::from_ref(&a), false)
    };
    let reader = "- {id: reader, title_key: hi, description_key: bye, icon: foundation_tablet, hidden: true}\n";
    let story = with(", about: tablet, achievement: reader", reader).unwrap();
    let structure = story.anchors[0].place.structure.as_ref().unwrap();
    assert_eq!(structure.about.as_deref(), Some("zeta:tablet"));
    assert_eq!(structure.achievement.as_deref(), Some("zeta:reader"));
    let achievement = &story.achievements[0];
    assert_eq!(achievement.id, "zeta:reader");
    assert_eq!(achievement.title_key, "zeta:hi");
    assert_eq!(achievement.category, "story");
    assert!(achievement.hidden);
    let set = "- {id: all, title_key: hi, description_key: bye, icon: foundation_tablet, read_all: [hut]}\n";
    let story = with(", about: tablet", set).unwrap();
    assert_eq!(story.achievements[0].read_all, ["zeta:hut"]);
    for (bad_structure, bad_achievements, why) in [
        ("", set, "a set naming a place with nothing to read"),
        (
            ", about: tablet",
            "- {id: all, title_key: hi, description_key: bye, icon: x, read_all: [nowhere]}\n",
            "a set naming an unknown place",
        ),
        (", about: nothing", reader, "an unknown card"),
        (
            ", about: tablet, achievement: writer",
            reader,
            "an unknown achievement",
        ),
        (
            ", achievement: reader",
            reader,
            "an achievement without anything to read",
        ),
        (
            "",
            "- {id: read__er, title_key: hi, description_key: hi, icon: x}\n",
            "a double underscore",
        ),
        (
            "",
            "- {id: reader, title_key: hi, description_key: hi, icon: x, category: fame}\n",
            "an unknown category",
        ),
        (
            "",
            "- {id: reader, title_key: nope, description_key: hi, icon: x}\n",
            "an untranslated title",
        ),
    ] {
        assert!(
            with(bad_structure, bad_achievements).is_err(),
            "{why} must be rejected"
        );
    }
}

#[test]
fn a_disk_may_define_the_worlds_planet() {
    let a = scratch("world-a");
    base(&a, "worlda", "1.0.0", "");
    write(
        &a,
        "datadisk.yaml",
        "id: worlda\nversion: 1.0.0\nname: Test\nrequires_ruleset: \"1\"\nlicense: CC0-1.0\nattribution: Test\nlanguages: [en, cs]\nworld:\n  terrain: {heightmap: media/world/height.pgm, water: media/world/water.pgm}\n",
    );
    write_world_rasters(&a, (16, 8), (16, 8), true);
    let story = Story::load(std::slice::from_ref(&a), false).unwrap();
    let world = story.world.expect("the disk defines the planet");
    assert_eq!(world.disk, "worlda");
    assert_eq!(world.terrain.heightmap, "media/world/height.pgm");
    assert_eq!(world.terrain.water, "media/world/water.pgm");
    assert!(
        story.media.contains_key("worlda/media/world/height.pgm"),
        "the rasters are part of the disk's media"
    );
}

#[test]
fn a_disk_may_paint_land_biomes_over_its_planet() {
    let manifest = "id: worlda\nversion: 1.0.0\nname: Test\nrequires_ruleset: \"1\"\nlicense: CC0-1.0\nattribution: Test\nlanguages: [en, cs]\nworld:\n  terrain: {heightmap: media/world/height.pgm, water: media/world/water.pgm}\n  biomes: {map: media/world/biomes.pgm}\n";
    let pgm = |width: usize, height: usize, pixels: &[u8]| {
        let mut bytes = format!("P5\n{width} {height}\n255\n").into_bytes();
        bytes.extend_from_slice(pixels);
        bytes
    };
    for (case, bytes, fine) in [
        ("painted", pgm(16, 8, &[5; 128]), true),
        (
            "size differs from the heightmap",
            pgm(8, 4, &[5; 32]),
            false,
        ),
        ("a water biome", pgm(16, 8, &[1; 128]), false),
        ("truncated", pgm(16, 8, &[5; 100]), false),
    ] {
        let a = scratch("world-biomes");
        base(&a, "worlda", "1.0.0", "");
        write(&a, "datadisk.yaml", manifest);
        write_world_rasters(&a, (16, 8), (16, 8), true);
        fs::write(a.join("media/world/biomes.pgm"), bytes).unwrap();
        let story = Story::load(std::slice::from_ref(&a), false);
        assert_eq!(story.is_ok(), fine, "{case}: {:?}", story.err());
        if let Ok(story) = story {
            let biomes = story.world.unwrap().biomes.expect("the map is loaded");
            assert_eq!((biomes.width, biomes.height), (16, 8));
        }
    }
}

#[test]
fn two_disks_cannot_both_define_the_planet() {
    let a = scratch("world-b");
    let b = scratch("world-c");
    for (root, id) in [(&a, "worldb"), (&b, "worldc")] {
        base(root, id, "1.0.0", "");
        write(
            root,
            "datadisk.yaml",
            &format!(
                "id: {id}\nversion: 1.0.0\nname: Test\nrequires_ruleset: \"1\"\nlicense: CC0-1.0\nattribution: Test\nlanguages: [en, cs]\nworld:\n  terrain: {{heightmap: media/world/height.pgm, water: media/world/water.pgm}}\n"
            ),
        );
        write_world_rasters(root, (16, 8), (16, 8), true);
    }
    let error = Story::load(&[a, b], false).unwrap_err();
    assert!(error.to_string().contains("world's planet"), "{error}");
}

#[test]
fn world_rasters_must_be_matching_equirectangular_pgms() {
    for (case, height, water, fine) in [
        ("mismatched sizes", (16, 8), (8, 4), false),
        ("not twice as wide", (8, 8), (8, 8), false),
        ("wrong heightmap maxval", (16, 8), (16, 8), false),
    ] {
        let a = scratch("world-bad");
        base(&a, "worldbad", "1.0.0", "");
        write(
            &a,
            "datadisk.yaml",
            "id: worldbad\nversion: 1.0.0\nname: Test\nrequires_ruleset: \"1\"\nlicense: CC0-1.0\nattribution: Test\nlanguages: [en, cs]\nworld:\n  terrain: {heightmap: media/world/height.pgm, water: media/world/water.pgm}\n",
        );
        write_world_rasters(&a, height, water, fine);
        assert!(
            Story::load(std::slice::from_ref(&a), false).is_err(),
            "{case} must be rejected"
        );
    }
}

/// Writes the equirectangular world rasters a manifest names; `fine` picks a valid pair.
fn write_world_rasters(root: &Path, height: (usize, usize), water: (usize, usize), fine: bool) {
    let raster = |path: &str, size: (usize, usize), maxval: u16| {
        let (width, rows) = size;
        let sample = if maxval > 255 { 2 } else { 1 };
        let mut bytes = format!("P5\n{width} {rows}\n{maxval}\n").into_bytes();
        for _ in 0..width * rows {
            let value = maxval;
            if sample == 2 {
                bytes.extend_from_slice(&value.to_be_bytes());
            } else {
                bytes.push(value as u8);
            }
        }
        let file = root.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, bytes).unwrap();
    };
    raster(
        "media/world/height.pgm",
        height,
        if fine { 65535 } else { 255 },
    );
    raster("media/world/water.pgm", water, 255);
}
