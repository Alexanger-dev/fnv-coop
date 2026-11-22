//! Tests of [`super`] on the testkit's look kit.

use super::*;
use crate::testkit::{look_kit, Story, StorySpec};

const HOUSE: u32 = 0x0010_5a00;
const RIFLE: u32 = 0x0010_7001;
const PISTOL: u32 = 0x0010_7002;
const ROUNDS: u32 = 0x0010_7003;
const SUIT: u32 = 0x0010_7004;
const HAT: u32 = 0x0010_7005;
const RACE: u32 = 0x0010_7100;
const HAIR: u32 = 0x0010_7101;
const EYES: u32 = 0x0010_7102;
const NOSE: u32 = 0x0010_7103;
const BROW: u32 = 0x0010_7104;
const ARMOR_TYPE: u8 = 0x18;
const AMMO_TYPE: u8 = 0x29;

fn story() -> Story {
    let mut s = Story::new(&StorySpec {
        cells: vec![HOUSE],
        ..StorySpec::default()
    });
    look_kit::install(&mut s);
    for (id, kind) in [
        (RIFLE, addr::FORM_TYPE_WEAPON),
        (PISTOL, addr::FORM_TYPE_WEAPON),
        (ROUNDS, AMMO_TYPE),
        (SUIT, ARMOR_TYPE),
        (HAT, ARMOR_TYPE),
    ] {
        s.add_look_item(id, kind);
    }
    for id in [RACE, HAIR, EYES, NOSE, BROW] {
        s.add_look_form(id, 0x0c);
    }
    s.set_look_ammo(RIFLE, ROUNDS);
    s.move_player(HOUSE, [10.0, 20.0, 0.0], 0.5);
    s
}

fn avatar(s: &mut Story, base: u32) -> live::Avatar {
    live::avatar_create(&mut s.e, base).expect("an avatar")
}

#[test]
fn the_players_pose_is_its_movement_weapon_and_reload() {
    let mut s = story();
    assert_eq!(pose(&mut s.e), Some(Pose::default()));
    s.set_moves(actors_player(), 0x201);
    s.look_give(actors_player(), RIFLE, 1, true);
    s.set_look_drawn(actors_player(), true);
    s.set_look_action(actors_player(), addr::ANIM_ACTION_RELOAD);
    assert_eq!(
        pose(&mut s.e),
        Some(Pose {
            moves: 0x201,
            drawn: true,
            weapon: RIFLE,
            reloading: true,
        })
    );
    s.set_look_action(actors_player(), 3);
    assert!(!pose(&mut s.e).unwrap().reloading);
}

fn actors_player() -> u32 {
    crate::actors::PLAYER_REF_ID
}

#[test]
fn each_attack_this_games_player_starts_is_noted_and_nothing_else() {
    let mut s = story();
    assert_eq!(player_attack(&mut s.e), None);
    assert!(s.player_attack(0x20));
    assert_eq!(
        player_attack(&mut s.e),
        Some(Attack {
            seq: 1,
            group: 0x20
        })
    );
    assert!(s.player_attack(0x21));
    assert_eq!(
        player_attack(&mut s.e),
        Some(Attack {
            seq: 2,
            group: 0x21
        })
    );
    // An attack the game did not start is not one.
    s.refuse_player_attacks(true);
    assert!(!s.player_attack(0x22));
    assert_eq!(player_attack(&mut s.e).map(|a| a.seq), Some(2));
    // Another actor's attack (an avatar's) is not the player's.
    s.refuse_player_attacks(false);
    let a = avatar(&mut s, 7);
    let r = forms::form(&mut s.e, a.id).unwrap();
    assert!(s
        .e
        .call(addr::ACTOR_START_ATTACK, &args![r, 0x23u32])
        .bool());
    assert_eq!(player_attack(&mut s.e).map(|a| a.seq), Some(2));
    assert_eq!(s.attacks_started(), 3);
}

#[test]
fn an_avatar_led_keeps_its_ai_walks_with_the_players_flags_and_fights_nobody() {
    let mut s = story();
    let a = avatar(&mut s, 7);
    // Placed with its AI off, as the ladder places one.
    assert!(!s.placed()[0].3);
    let h = lead(&mut s.e, a, [40.0, 50.0, 1.0], 1.0).unwrap();
    assert!(!h.woke && !h.rehomed && !h.unseen);
    assert_eq!(h.pos, [40.0, 50.0, 1.0]);
    assert!(s.placed()[0].3, "its AI is on: the game animates it");
    assert!(gait(&mut s.e, a.id, 0x201));
    assert_eq!(s.forced_moves(a.id), Some(0x201));
    assert!(gait(&mut s.e, a.id, 0));
    assert_eq!(s.forced_moves(a.id), Some(0));
    // A fight of its own is ended.
    assert!(!calm(&mut s.e, a.id));
    s.set_combat(a.id, actors_player());
    assert!(calm(&mut s.e, a.id));
    assert!(!s.actor_view(a.id).in_combat);
    // Gone: nothing.
    assert!(lead(&mut s.e, live::Avatar { id: 0x0010_9999 }, [0.0; 3], 0.0).is_none());
}

#[test]
fn an_avatar_draws_attacks_and_reloads_through_the_game() {
    let mut s = story();
    let a = avatar(&mut s, 7);
    assert!(draw(&mut s.e, a.id, true));
    assert!(s.look_gear(a.id).drawn);
    assert_eq!(pose_of_id(&mut s.e, a.id).map(|p| p.drawn), Some(true));
    assert!(draw(&mut s.e, a.id, false));
    assert!(!s.look_gear(a.id).drawn);
    assert!(attack(&mut s.e, a.id, 0x20));
    assert_eq!(s.attacks_queued(a.id), 1);
    assert_eq!(s.queued_group(a.id), 0x20);
    assert!(reload(&mut s.e, a.id));
    assert_eq!(s.look_gear(a.id).reloads, 1);
    assert!(!draw(&mut s.e, 0x0010_9999, true));
}

#[test]
fn an_avatar_wears_what_it_is_given_and_takes_off_the_rest() {
    let mut s = story();
    // The player wears the suit and holds the rifle and its rounds; the hat is only carried.
    s.look_give(actors_player(), SUIT, 1, true);
    s.look_give(actors_player(), RIFLE, 1, true);
    s.look_give(actors_player(), ROUNDS, 20, true);
    s.look_give(actors_player(), HAT, 1, false);
    assert_eq!(worn(&mut s.e, actors_player()), [SUIT, RIFLE, ROUNDS]);
    let a = avatar(&mut s, 7);
    // The avatar has its own hat on.
    s.look_give(a.id, HAT, 1, true);
    let w = wear(&mut s.e, a.id, &[SUIT, RIFLE], true);
    assert_eq!(s.look_wearing(a.id), [SUIT, RIFLE]);
    // The suit and the rifle given, and the rifle's rounds (it had none).
    assert_eq!(
        w,
        Wear {
            put_on: 2,
            taken_off: 1,
            given: 3,
        }
    );
    assert_eq!(s.look_count(a.id, ROUNDS), AMMO_FILL);
    // As wanted already: nothing asked of the game.
    let before = s.look_gear(a.id);
    assert_eq!(wear(&mut s.e, a.id, &[SUIT, RIFLE], true), Wear::default());
    assert_eq!(s.look_gear(a.id), before);
    // Another weapon takes the rifle's place; without strip the suit stays.
    let w = wear(&mut s.e, a.id, &[PISTOL], false);
    assert_eq!((w.put_on, w.taken_off), (1, 0));
    assert_eq!(s.look_wearing(a.id), [SUIT, PISTOL]);
    // An item this game lacks is left out.
    assert_eq!(wear(&mut s.e, a.id, &[0x0010_9999], true).put_on, 0);
}

fn a_face() -> Face {
    Face {
        female: true,
        race: RACE,
        hair: HAIR,
        hair_length: 0.25,
        eyes: EYES,
        hair_color: 0x0040_2010,
        height: 1.02,
        weight: 0.5,
        head_parts: vec![NOSE, BROW],
        coord: [
            (3, 1, vec![0.5, -1.25, 2.0]),
            (2, 1, vec![0.125, -0.75]),
            (1, 2, vec![3.5, -2.5]),
            (0, 0, Vec::new()),
        ],
        name: "Courier Two".into(),
    }
}

#[test]
fn a_base_copy_takes_another_players_face_and_name() {
    let mut s = story();
    // This game's player has its own face.
    let mut mine = a_face();
    mine.female = false;
    mine.head_parts = vec![BROW];
    mine.name = "Courier One".into();
    assert!(set_face(&mut s.e, 7, &mine));
    assert_eq!(player_face(&mut s.e), Some(mine.clone()));
    // A copy of the player's base: its own id, the player's face until it is given another.
    let copy = base_copy(&mut s.e, 7).expect("a copy");
    assert_ne!(copy, 7);
    assert_eq!(s.npcs_made(), 1);
    assert_eq!(face_of_id(&mut s.e, copy), Some(mine.clone()));
    assert_eq!(
        s.face_npc(copy),
        7,
        "the game's copy takes the source's face NPC"
    );
    let theirs = a_face();
    assert!(set_face(&mut s.e, copy, &theirs));
    assert_eq!(face_of_id(&mut s.e, copy), Some(theirs));
    assert_eq!(s.face_npc(copy), 0, "its own face");
    // The player's own base is untouched.
    assert_eq!(player_face(&mut s.e), Some(mine));
    // An avatar placed from it is of it.
    let a = avatar(&mut s, copy);
    assert_eq!(base_of(&mut s.e, a.id), Some(copy));
    // Not NPCs: nothing.
    assert_eq!(base_copy(&mut s.e, RIFLE), None);
    assert!(!set_face(&mut s.e, RIFLE, &a_face()));
    assert_eq!(base_copy(&mut s.e, 0x0010_9999), None);
}

#[test]
fn a_face_with_forms_this_game_lacks_keeps_its_own_for_them() {
    let mut s = story();
    let copy = base_copy(&mut s.e, 7).unwrap();
    let mut f = a_face();
    assert!(set_face(&mut s.e, copy, &f));
    f.race = 0x0010_9998;
    f.head_parts = vec![NOSE, 0x0010_9999];
    assert!(set_face(&mut s.e, copy, &f));
    let got = face_of_id(&mut s.e, copy).unwrap();
    assert_eq!(got.race, RACE);
    assert_eq!(got.head_parts, [NOSE]);
}

/// A 3D in plain memory: objects with a vtable whose `GetRTTI` and `IsNode` answer from the
/// object's words (+0x40 its class record, +0x44 whether a node).
mod body_check {
    use super::super::*;
    use engine_core::Engine;

    const BASE: u32 = 0x3000_0000;
    const VTABLE: u32 = BASE;
    const GET_RTTI: u32 = 0x3100_0000;
    const IS_NODE: u32 = 0x3100_0010;

    fn eax(v: u32) -> Ret {
        Ret {
            eax: v,
            ..Ret::default()
        }
    }

    struct Tree {
        e: Engine,
        next: u32,
    }

    impl Tree {
        fn new() -> Tree {
            let mut e = Engine::new();
            e.mem.map(BASE, 0x10_0000);
            e.mem.set_u32(VTABLE + 8, GET_RTTI);
            e.mem.set_u32(VTABLE + 0xc, IS_NODE);
            e.register(GET_RTTI, |e, a| eax(e.mem.u32(a[0] + 0x40)));
            e.register(IS_NODE, |e, a| {
                eax(if e.mem.u32(a[0] + 0x44) != 0 { a[0] } else { 0 })
            });
            Tree {
                e,
                next: BASE + 0x1000,
            }
        }

        fn alloc(&mut self, size: u32) -> u32 {
            let at = self.next;
            self.next += size;
            at
        }

        fn text(&mut self, t: &str) -> u32 {
            let at = self.alloc(0x40);
            self.e.mem.set_cstr(at, t.as_bytes());
            at
        }

        fn class(&mut self, t: &str) -> u32 {
            let record = self.alloc(8);
            let name = self.text(t);
            self.e.mem.set_u32(record, name);
            record
        }

        /// An object of class `class` named `name` under `parent`, its bound centred at `at`.
        fn object(&mut self, class: &str, name: &str, parent: u32, at: [f32; 3]) -> u32 {
            let o = self.alloc(0x100);
            self.e.mem.set_u32(o, VTABLE);
            let n = self.text(name);
            self.e.mem.set_u32(o + 8, n);
            self.e.mem.set_u32(o + 0x18, parent);
            let c = self.class(class);
            self.e.mem.set_u32(o + 0x40, c);
            let node = class.contains("Node");
            self.e.mem.set_u32(o + 0x44, u32::from(node));
            let bound = self.alloc(0x10);
            for (i, v) in at.iter().enumerate() {
                self.e.mem.set_f32(bound + 4 * i as u32, *v);
            }
            self.e.mem.set_u32(o + 0x20, bound);
            if node {
                let children = self.alloc(0x40);
                self.e.mem.set_u32(o + 0xa0, children);
            }
            if parent != 0 {
                let children = self.e.mem.u32(parent + 0xa0);
                let n = self.e.mem.u16(parent + 0xa6);
                self.e.mem.set_u32(children + 4 * u32::from(n), o);
                self.e.mem.set_u16(parent + 0xa6, n + 1);
            }
            o
        }

        /// A skin on `shape`: its root parent and first bone, a dismember skin with its
        /// partitions shown or not.
        fn skin(&mut self, shape: u32, root: u32, bone: u32, shown: Option<bool>) {
            let skin = self.alloc(0x80);
            self.e.mem.set_u32(skin, VTABLE);
            let class = match shown {
                Some(_) => "BSDismemberSkinInstance",
                None => "NiSkinInstance",
            };
            let c = self.class(class);
            self.e.mem.set_u32(skin + 0x40, c);
            self.e.mem.set_u32(skin + 0x10, root);
            let bones = self.alloc(4);
            self.e.mem.set_u32(bones, bone);
            self.e.mem.set_u32(skin + 0x14, bones);
            if let Some(shown) = shown {
                self.e.mem.set_u8(skin + 0x3c, u8::from(shown));
            }
            self.e.mem.set_u32(shape + 0xbc, skin);
        }
    }

    #[test]
    fn an_avatars_body_is_read_from_its_3d_and_what_hides_it_named() {
        let mut t = Tree::new();
        let at = [100.0, 200.0, 300.0];
        let root = t.object("BSFadeNode", "Scene Root", 0, at);
        for (i, v) in at.iter().enumerate() {
            t.e.mem.set_f32(root + 0x8c + 4 * i as u32, *v);
        }
        t.e.mem.set_f32(root + 0xb8, 1.0);
        let bone = t.object("NiNode", "Bip01", root, at);
        let upper = t.object("NiNode", "UpperBody", root, at);
        let body = t.object("NiTriShape", "UpperBody", upper, [100.0, 200.0, 360.0]);
        t.skin(body, root, bone, Some(true));
        let meat = t.object("NiTriShape", "bodymeat", upper, at);
        t.skin(meat, root, bone, Some(false));
        let head = t.object("NiTriShape", "FaceGenFace", root, at);
        t.skin(head, root, bone, None);
        let b = body_of_node(&mut t.e, root);
        assert_eq!(
            b,
            Body {
                root_culled: false,
                fade: Some(1.0),
                shapes: 3,
                culled: vec![],
                unbound: vec![],
                hidden: vec!["bodymeat".into()],
                far: vec![],
            }
        );
        // Out of place, bound to another 3D's skeleton, culled through its node.
        let elsewhere = t.object("NiNode", "Bip01", 0, at);
        let arms = t.object("NiTriShape", "Arms", upper, [100.0, 600.0, 300.0]);
        t.skin(arms, root, elsewhere, Some(true));
        let hand = t.object("NiTriShape", "Hand", upper, at);
        t.skin(hand, elsewhere, bone, None);
        t.e.mem.set_u32(upper + 0x30, 1);
        let b = body_of_node(&mut t.e, root);
        assert!(!b.root_culled);
        assert_eq!(b.shapes, 5);
        assert_eq!(b.culled.len(), 4, "{b:?}");
        assert!(!b.culled.contains(&"FaceGenFace".to_string()), "{b:?}");
        assert_eq!(b.unbound, ["Hand", "Arms"]);
        assert_eq!(b.far, ["Arms"]);
        assert_eq!(
            b.to_string(),
            "5 shapes; root culled false, fade 1.00; culled [\"Hand\", \"Arms\", \"bodymeat\", \
             \"UpperBody\"], not on its skeleton [\"Hand\", \"Arms\"], all parts hidden \
             [\"bodymeat\"], out of place [\"Arms\"]"
        );
        // The root culled: everything under it.
        t.e.mem.set_u32(root + 0x30, 1);
        let b = body_of_node(&mut t.e, root);
        assert!(b.root_culled);
        assert_eq!(b.culled.len(), 5);
        // A root that is no fade node has no fade.
        let plain = t.object("NiNode", "Root", 0, at);
        assert_eq!(body_of_node(&mut t.e, plain).fade, None);
    }

    /// The check reads a 3D that may be broken: pointers nowhere and loops are passed over,
    /// never followed.
    #[test]
    fn a_broken_3d_is_read_without_following_its_bad_pointers() {
        let mut t = Tree::new();
        let at = [0.0; 3];
        let root = t.object("NiNode", "Scene Root", 0, at);
        let bone = t.object("NiNode", "Bip01", root, at);
        // A child pointer to nowhere.
        let children = t.e.mem.u32(root + 0xa0);
        t.e.mem.set_u32(children + 4, 0x7000_0000);
        t.e.mem.set_u16(root + 0xa6, 2);
        // A shape whose skin is nowhere; one whose skin's bones are nowhere.
        let a = t.object("NiTriShape", "A", root, at);
        t.e.mem.set_u32(a + 0xbc, 0x7000_0000);
        let b = t.object("NiTriShape", "B", root, at);
        t.skin(b, root, bone, None);
        let skin = t.e.mem.u32(b + 0xbc);
        t.e.mem.set_u32(skin + 0x14, 0x7000_0000);
        // A root with no class record.
        t.e.mem.set_u32(root + 0x40, 0);
        let body = body_of_node(&mut t.e, root);
        assert_eq!(body.fade, None);
        assert!(body.shapes >= 2, "{body:?}");
        assert!(body.unbound.iter().all(|n| n == "B"), "{body:?}");
        assert!(!body.unbound.is_empty());
        assert_eq!(body.shapes, 2, "{body:?}");
        // A node its own child (a loop): cut short.
        let mut t = Tree::new();
        let root = t.object("NiNode", "Scene Root", 0, at);
        t.object("NiTriShape", "A", root, at);
        let children = t.e.mem.u32(root + 0xa0);
        t.e.mem.set_u32(children + 4, root);
        t.e.mem.set_u16(root + 0xa6, 2);
        let body = body_of_node(&mut t.e, root);
        assert!(body.shapes <= BODY_OBJECTS_MAX, "the loop is cut short");
    }
}
