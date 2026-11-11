//! What a story script tells an actor to do, as distinct from state (docs/coop/SPECS.md S13,
//! docs/coop/DECISIONS.md D5: host commands). The first: `evp`, an actor told to evaluate its
//! package again. A stage's result script says `SunnyREF.evp` so she acts on the new stage at
//! once; that script runs on the host only, and the guest's copy of her would stand still.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, PoisonError};

use engine_core::extend::{Owner, WrapSlot};
use engine_core::prelude::*;

use crate::{actors, addr, fight, forms, gate, journal};

pub(crate) const EVALUATE: WrapSlot = WrapSlot::new(Owner::Coop, addr::ACTOR_EVALUATE_PACKAGE);

/// The commands noted since the last drain (game-wide, as the journal).
#[derive(Debug, Default)]
pub struct Commands {
    /// Reference ids of the actors a script told to evaluate their package.
    evaluate: Mutex<Vec<u32>>,
    /// Conversations between two actors a script started, in order.
    scenes: Mutex<Vec<Scene>>,
    /// A guest's own conversations between two actors, dropped.
    dropped: AtomicU32,
    /// Guest: the package the host's copy of each actor runs (reference id to package id).
    wanted: Mutex<BTreeMap<u32, u32>>,
    /// Guest: whether those packages are given to this game's copies.
    packages_on: AtomicBool,
    /// Guest: how many evaluations were answered with the host's package.
    given: AtomicU32,
}

pub(crate) const FIND_PACKAGE: WrapSlot =
    WrapSlot::new(Owner::Coop, addr::PACKAGE_LIST_FIND_CURRENT);

/// The form id of the package actor `id` runs in this game now (0: none, or one made while
/// playing, which the other game does not have); `None` when the actor is not here.
pub fn current_package(e: &mut Engine, id: u32) -> Option<u32> {
    let a = actors::actor(e, id)?;
    if !e.is_registered(addr::ACTOR_GET_CURRENT_EDITOR_PACKAGE) {
        return Some(0);
    }
    let package = e
        .call(addr::ACTOR_GET_CURRENT_EDITOR_PACKAGE, &args![a])
        .u32();
    if package == 0 {
        return Some(0);
    }
    let pid = e.mem.u32(package.wrapping_add(addr::FORM_ID_OFFSET));
    Some(if pid < actors::FIRST_DYNAMIC_ID {
        pid
    } else {
        0
    })
}

/// Guest: the host's copy of actor `actor` runs package `package` (0: none this game has; this
/// game's own choice stands).
pub fn set_wanted_package(e: &mut Engine, actor: u32, package: u32) {
    let c = e.ext_free::<Commands>();
    let mut wanted = c.wanted.lock().unwrap_or_else(PoisonError::into_inner);
    if package == 0 {
        wanted.remove(&actor);
    } else {
        wanted.insert(actor, package);
    }
}

/// Guest: the package the host's copy of `actor` runs, if known.
pub fn wanted_package(e: &mut Engine, actor: u32) -> Option<u32> {
    let c = e.ext_free::<Commands>();
    let wanted = c.wanted.lock().unwrap_or_else(PoisonError::into_inner);
    wanted.get(&actor).copied()
}

/// Guest: whether the host's packages are given to this game's copies (off while this player's
/// own opening runs, whose actors act for this player).
pub fn set_packages_on(e: &mut Engine, on: bool) {
    e.ext_free::<Commands>()
        .packages_on
        .store(on, Ordering::SeqCst);
}

/// Guest: how many evaluations were answered with the host's package.
pub fn packages_given(e: &mut Engine) -> u32 {
    e.ext_free::<Commands>().given.load(Ordering::SeqCst)
}

/// The wrap at `0067e780`. On a guest, for an actor whose host's package is known and is a
/// package of this game, the answer is that package (its procedure type computed first, as the
/// game does for the one it picks); this game's own schedule and conditions are not asked. They
/// are each game's own: a condition on "the player" means this game's player, and the two
/// players' copies of an actor would do different things (PC run 12: Sunny led the host outdoors
/// and stayed with the guest indoors). Everything else is the game's own answer.
pub(crate) fn find_package(e: &mut Engine, a: &[u32]) -> Ret {
    let reference = a.get(1).copied().unwrap_or(0);
    if reference != 0 && gate::is_guest(e) {
        let c = e.ext_free::<Commands>();
        if c.packages_on.load(Ordering::SeqCst) {
            let id = e.mem.u32(reference.wrapping_add(addr::FORM_ID_OFFSET));
            if let Some(package) = wanted_package(e, id).and_then(|w| forms::form(e, w)) {
                if e.mem.u8(package.wrapping_add(addr::FORM_TYPE_OFFSET)) == addr::FORM_TYPE_PACKAGE
                {
                    if e.call(addr::PACKAGE_PROCEDURE_TYPE, &args![package]).u32() as i32 == -1 {
                        let value = e.call(addr::REF_PROCEDURE_VALUE, &args![reference]).u32();
                        e.call(addr::PACKAGE_CALCULATE_PROCEDURE, &args![package, value]);
                    }
                    e.ext_free::<Commands>()
                        .given
                        .fetch_add(1, Ordering::SeqCst);
                    return package.into_ret();
                }
            }
        }
    }
    e.call_next(addr::PACKAGE_LIST_FIND_CURRENT, a)
}

pub(crate) const CONVERSE: WrapSlot = WrapSlot::new(Owner::Coop, addr::CMD_START_CONVERSATION);

/// A conversation a script started between two actors (`StartConversation`): who, with whom,
/// about what, and where both stood.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scene {
    /// The reference id of the actor the command ran on.
    pub actor: u32,
    /// The reference id of the one it talks to.
    pub target: u32,
    /// The topic's form id, 0 for none.
    pub topic: u32,
    /// The two location references' ids, 0 for none.
    pub locations: [u32; 2],
    /// The command's two flags as the game passes them on.
    pub flags: [bool; 2],
    /// The space both were read in (the actor's) and their places.
    pub space: u32,
    /// See [`Scene::space`].
    pub actor_pos: [f32; 3],
    /// See [`Scene::space`].
    pub target_pos: [f32; 3],
}

/// The command's six parameters as its body parses them (target, topic, two locations, two
/// flags; defaults 0, 0, 0, 0, 1, 0); `None` when the parse fails. The offset into the script
/// data is moved on as the body's own parse moves it; with `keep` it is put back afterwards, so
/// the body can parse again.
fn parse(e: &mut Engine, a: &[u32], keep: bool) -> Option<[u32; 6]> {
    let word = |i: usize| a.get(i).copied().unwrap_or(0);
    let offset_at = word(7);
    let before = if offset_at != 0 {
        e.mem.u32(offset_at)
    } else {
        0
    };
    let parsed = e.with_stack(24, |e, block| {
        let outs: Vec<u32> = (0..6).map(|i| block.addr() + 4 * i).collect();
        for (out, value) in outs.iter().zip([0u32, 0, 0, 0, 1, 0]) {
            e.mem.set_u32(*out, value);
        }
        let mut words = args![
            word(0),
            word(1),
            offset_at,
            word(2),
            word(3),
            word(4),
            word(5)
        ];
        words.extend_from_slice(&outs);
        let ok = e.call(addr::SCRIPT_PARSE_PARAMETERS, &words).u32() & 0xff != 0;
        let mut values = [0u32; 6];
        for (v, out) in values.iter_mut().zip(&outs) {
            *v = e.mem.u32(*out);
        }
        ok.then_some(values)
    });
    if keep && offset_at != 0 {
        e.mem.set_u32(offset_at, before);
    }
    parsed
}

/// The wrap at `005c8740`. A conversation with the player on either side is that player's own
/// and runs as it is. One between two actors is the host's to decide: the host's game runs it
/// and notes it; a guest's game does not run its own (the command's parameters are still read,
/// so the script goes on after them) and plays the host's when it comes.
pub(crate) fn converse(e: &mut Engine, a: &[u32]) -> Ret {
    let this_obj = a.get(2).copied().unwrap_or(0);
    let player = e.mem.u32(addr::PLAYER_SINGLETON);
    let guest = gate::is_guest(e);
    if this_obj == 0 || this_obj == player || !(guest || journal::armed(e)) {
        return e.call_next(addr::CMD_START_CONVERSATION, a);
    }
    let Some(v) = parse(e, a, true) else {
        return e.call_next(addr::CMD_START_CONVERSATION, a);
    };
    let target = v[0];
    if target == 0 || target == player {
        return e.call_next(addr::CMD_START_CONVERSATION, a);
    }
    let id_of = |e: &mut Engine, form: u32| match form {
        0 => 0,
        f => e.mem.u32(f.wrapping_add(addr::FORM_ID_OFFSET)),
    };
    let (actor_id, target_id) = (id_of(e, this_obj), id_of(e, target));
    let is_actor = |e: &mut Engine, f: u32| {
        actors::is_actor_type(e.mem.u8(f.wrapping_add(addr::FORM_TYPE_OFFSET)))
    };
    if !actors::shareable(actor_id)
        || !actors::shareable(target_id)
        || !is_actor(e, this_obj)
        || !is_actor(e, target)
    {
        return e.call_next(addr::CMD_START_CONVERSATION, a);
    }
    if guest {
        // Read past the parameters as the body would, and say the command ran.
        let _ = parse(e, a, false);
        e.ext_free::<Commands>()
            .dropped
            .fetch_add(1, Ordering::SeqCst);
        return 1u32.into_ret();
    }
    let pos = |e: &mut Engine, r: u32| {
        [0, 4, 8].map(|o| e.mem.f32(r.wrapping_add(addr::REF_POS_OFFSET + o)))
    };
    let scene = Scene {
        actor: actor_id,
        target: target_id,
        topic: id_of(e, v[1]),
        locations: [id_of(e, v[2]), id_of(e, v[3])],
        flags: [v[4] == 1, v[5] == 1],
        space: fight::space_of(e, this_obj),
        actor_pos: pos(e, this_obj),
        target_pos: pos(e, target),
    };
    e.ext_free::<Commands>()
        .scenes
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(scene);
    e.call_next(addr::CMD_START_CONVERSATION, a)
}

/// Takes the conversations between actors that scripts started since the last call.
pub fn drain_scenes(e: &mut Engine) -> Vec<Scene> {
    let c = e.ext_free::<Commands>();
    std::mem::take(&mut *c.scenes.lock().unwrap_or_else(PoisonError::into_inner))
}

/// How many conversations between two actors this guest's own scripts asked for and did not get.
pub fn dropped_scenes(e: &mut Engine) -> u32 {
    e.ext_free::<Commands>().dropped.load(Ordering::SeqCst)
}

/// Why a host's conversation was not started here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoScene {
    /// One of the two is not in this game now, or is disabled.
    Absent,
    /// One of the two is not an actor the game can make talk.
    NotMobile,
}

/// Plays the host's conversation in this game: both are put where the host had them when they
/// are in that space here, then the actor starts the conversation as the command's body does
/// (the actor's virtual slot 0x280). The game's own checks beyond presence are the slot's.
pub fn play_scene(e: &mut Engine, s: &Scene) -> Result<(), NoScene> {
    let (Some(actor), Some(target)) = (
        actors::reference(e, s.actor),
        actors::reference(e, s.target),
    ) else {
        return Err(NoScene::Absent);
    };
    if actors::disabled(e, s.actor) != Some(false) || actors::disabled(e, s.target) != Some(false) {
        return Err(NoScene::Absent);
    }
    for r in [actor, target] {
        if e.vcall(r, addr::REF_IS_MOBILE_SLOT, &args![]).u32() & 0xff == 0 {
            return Err(NoScene::NotMobile);
        }
    }
    for (r, pos) in [(actor, s.actor_pos), (target, s.target_pos)] {
        if fight::space_of(e, r) == s.space {
            for (axis, v) in (*b"XYZ").into_iter().zip(pos) {
                e.call(addr::SET_POS_BASE, &args![r, u32::from(axis), v.to_bits()]);
            }
        }
    }
    let topic = forms::form(e, s.topic).unwrap_or(0);
    let places = s.locations.map(|id| forms::form(e, id).unwrap_or(0));
    e.with_stack(2 * addr::PACKAGE_LOCATION_SIZE, |e, block| {
        let at = [block.addr(), block.addr() + addr::PACKAGE_LOCATION_SIZE];
        let mut used = [0u32; 2];
        for i in 0..2 {
            e.call(addr::PACKAGE_LOCATION_CONSTRUCT, &args![at[i]]);
            if places[i] != 0 {
                used[i] = at[i];
                e.call(
                    addr::PACKAGE_LOCATION_SET_REFERENCE,
                    &args![at[i], places[i]],
                );
            }
        }
        e.vcall(
            actor,
            addr::ACTOR_START_CONVERSATION_SLOT,
            &args![
                target,
                used[0],
                used[1],
                u32::from(s.flags[0]),
                u32::from(s.flags[1]),
                0u32,
                topic,
                0u32,
                0u32
            ],
        );
        e.call(addr::PACKAGE_LOCATION_DESTRUCT, &args![at[1]]);
        e.call(addr::PACKAGE_LOCATION_DESTRUCT, &args![at[0]]);
    });
    Ok(())
}

/// The wrap at `008a6ce0`: the game's call runs as it is; on a host whose journal is armed, an
/// evaluation asked for while a script runs, of an actor both games know, is noted.
pub(crate) fn evaluate(e: &mut Engine, a: &[u32]) -> Ret {
    let actor = a.first().copied().unwrap_or(0);
    if actor != 0 && journal::armed(e) && !gate::is_guest(e) && gate::cause(e).is_some() {
        let id = e.mem.u32(actor.wrapping_add(addr::FORM_ID_OFFSET));
        if actors::shareable(id) {
            let c = e.ext_free::<Commands>();
            let mut list = c.evaluate.lock().unwrap_or_else(PoisonError::into_inner);
            if !list.contains(&id) {
                list.push(id);
            }
        }
    }
    e.call_next(addr::ACTOR_EVALUATE_PACKAGE, a)
}

/// Takes the actors scripts told to evaluate their package since the last call.
pub fn drain_evaluations(e: &mut Engine) -> Vec<u32> {
    let c = e.ext_free::<Commands>();
    std::mem::take(&mut *c.evaluate.lock().unwrap_or_else(PoisonError::into_inner))
}

/// Tells actor `id` to evaluate its package again, as a script's `evp` does (buffered; AI reset).
/// False when the actor is not in this game now.
pub fn evaluate_package(e: &mut Engine, id: u32) -> bool {
    let Some(a) = actors::actor(e, id) else {
        return false;
    };
    e.call(addr::ACTOR_EVALUATE_PACKAGE, &args![a, 0u32, 1u32]);
    true
}
