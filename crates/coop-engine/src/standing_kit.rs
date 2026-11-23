//! The test game's karma and perks (`reward`): `PlayerCharacter::RewardKarma` counting what it
//! was given, the `AddPerk` and `RemovePerk` commands' bodies as the game's (parse, the player's
//! rank of the perk through its virtual slots), and a script's karma and perk commands for a
//! line's result.

use engine_core::prelude::*;

use super::{add_form, eax, SCENE_ARGS, SCENE_OFFSET};
use crate::addr;

/// The kit's own memory: the karma total, the player's perks (perk, rank, teammate rank: 32 of
/// them), the commands scripts run (32), and the forms.
const SK: u32 = 0x0175_0000;
const KARMA: u32 = SK;
const PERKS: u32 = SK + 0x400;
const PERK_SLOT: u32 = 12;
const PERKS_MAX: u32 = 32;
const COMMANDS: u32 = SK + 0x800;
const COMMAND_SLOT: u32 = 24;
const COMMANDS_MAX: u32 = 32;
const FORMS: u32 = SK + 0x1000;
const FORM_SIZE: u32 = 0x40;
const FORMS_MAX: u32 = 32;
/// The functions behind the player's perk slots.
const RANK_FN: u32 = 0x0130_0130;
const SET_RANK_FN: u32 = 0x0130_0140;
const REMOVE_FN: u32 = 0x0130_0150;

/// What a script's command is: karma, a perk added or removed.
#[derive(Clone, Copy)]
#[repr(u32)]
enum Kind {
    Karma = 1,
    AddPerk = 5,
    RemovePerk = 6,
}

/// Called by `Story::new` before co-op is installed, after the player exists.
pub(super) fn register(e: &mut Engine, player_vtable: u32) {
    e.map(SK, 0x2000);
    e.register(addr::PLAYER_REWARD_KARMA, |e, a| {
        let total = e.mem.u32(KARMA) as i32;
        e.mem.set_u32(KARMA, total.wrapping_add(a[1] as i32) as u32);
        Ret::default()
    });
    for (slot, f) in [
        (addr::ACTOR_PERK_RANK_SLOT, RANK_FN),
        (addr::ACTOR_SET_PERK_RANK_SLOT, SET_RANK_FN),
        (addr::ACTOR_REMOVE_PERK_SLOT, REMOVE_FN),
    ] {
        e.mem.set_u32(player_vtable + slot, f);
    }
    // (this, perk, teammate)
    e.register(RANK_FN, |e, a| eax(rank(e, a[1], a[2] != 0)));
    // (this, perk, rank, teammate)
    e.register(SET_RANK_FN, |e, a| {
        set_rank(e, a[1], a[3] != 0, a[2]);
        Ret::default()
    });
    e.register(REMOVE_FN, |e, a| {
        set_rank(e, a[1], a[2] != 0, 0);
        Ret::default()
    });
    // The commands' bodies for an actor, as the game's (`script_add_perk_function`,
    // `fn_005d4f00`): the kit's player only.
    e.register(addr::CMD_ADD_PERK, |e, a| {
        let (perk, flag) = parse(e, a);
        if perk != 0 && a[2] != 0 && a[2] == e.mem.u32(addr::PLAYER_SINGLETON) {
            let r = e
                .vcall(a[2], addr::ACTOR_PERK_RANK_SLOT, &args![perk, flag])
                .u32();
            if u32::from(e.mem.u8(perk + addr::PERK_HIGHEST_RANK_OFFSET)) > r {
                e.vcall(
                    a[2],
                    addr::ACTOR_SET_PERK_RANK_SLOT,
                    &args![perk, r + 1, flag],
                );
            }
        }
        eax(1)
    });
    e.register(addr::CMD_REMOVE_PERK, |e, a| {
        let (perk, flag) = parse(e, a);
        if perk != 0 && a[2] != 0 && a[2] == e.mem.u32(addr::PLAYER_SINGLETON) {
            e.vcall(a[2], addr::ACTOR_REMOVE_PERK_SLOT, &args![perk, flag]);
        }
        eax(1)
    });
}

/// The command's two parameters, as the kit's `ParseParameters` gives them.
fn parse(e: &mut Engine, a: &[u32]) -> (u32, u32) {
    let outs = SCENE_ARGS + 0x40;
    let mut words = args![a[0], a[1], a[7], a[2], a[3], a[4], a[5]];
    words.extend((0..6).map(|i| outs + 4 * i));
    e.call(addr::SCRIPT_PARSE_PARAMETERS, &words);
    (e.mem.u32(outs), u32::from(e.mem.u32(outs + 4) != 0))
}

fn perk_slot(e: &mut Engine, perk: u32) -> u32 {
    for i in 0..PERKS_MAX {
        let at = PERKS + PERK_SLOT * i;
        let p = e.mem.u32(at);
        if p == 0 || p == perk {
            return at;
        }
    }
    panic!("the kit's perks are full");
}

fn rank(e: &mut Engine, perk: u32, teammate: bool) -> u32 {
    let at = perk_slot(e, perk);
    if e.mem.u32(at) == 0 {
        0
    } else {
        e.mem.u32(at + 4 + 4 * u32::from(teammate))
    }
}

fn set_rank(e: &mut Engine, perk: u32, teammate: bool, r: u32) {
    let at = perk_slot(e, perk);
    e.mem.set_u32(at, perk);
    e.mem.set_u32(at + 4 + 4 * u32::from(teammate), r);
}

fn new_form(e: &mut Engine, id: u32) -> u32 {
    let n = e.mem.u32(FORMS - 4);
    assert!(n < FORMS_MAX, "the kit's forms are full");
    e.mem.set_u32(FORMS - 4, n + 1);
    let form = FORMS + FORM_SIZE * n;
    e.mem.set_u32(form + addr::FORM_ID_OFFSET, id);
    add_form(e, id, form);
    form
}

fn push(e: &mut Engine, script: u32, kind: Kind, words: [u32; 4]) {
    let n = e.mem.u32(COMMANDS - 4);
    assert!(n < COMMANDS_MAX, "the kit's script commands are full");
    e.mem.set_u32(COMMANDS - 4, n + 1);
    let at = COMMANDS + COMMAND_SLOT * n;
    e.mem.set_u32(at, script);
    e.mem.set_u32(at + 4, kind as u32);
    for (i, w) in words.into_iter().enumerate() {
        e.mem.set_u32(at + 8 + 4 * i as u32, w);
    }
}

/// The kit's script runner: the commands a script was given, in order.
pub(super) fn run(e: &mut Engine, script: u32) {
    if script == 0 || !e.mem.is_mapped(COMMANDS) {
        return;
    }
    for n in 0..e.mem.u32(COMMANDS - 4) {
        let at = COMMANDS + COMMAND_SLOT * n;
        if e.mem.u32(at) != script {
            continue;
        }
        let w: Vec<u32> = (0..4).map(|i| e.mem.u32(at + 8 + 4 * i)).collect();
        let player = e.mem.u32(addr::PLAYER_SINGLETON);
        match e.mem.u32(at + 4) {
            1 => {
                e.call(addr::PLAYER_REWARD_KARMA, &args![player, w[0]]);
            }
            k @ 5..=6 => {
                let Some(perk) = crate::forms::form(e, w[0]) else {
                    continue;
                };
                let reference = match w[2] {
                    0 => player,
                    id => crate::forms::form(e, id).unwrap_or(0),
                };
                e.mem.set_u32(SCENE_ARGS, perk);
                e.mem.set_u32(SCENE_ARGS + 4, w[1]);
                e.mem.set_u32(SCENE_OFFSET, 0);
                let body = if k == 5 {
                    addr::CMD_ADD_PERK
                } else {
                    addr::CMD_REMOVE_PERK
                };
                e.call(
                    body,
                    &args![0u32, 0u32, reference, 0u32, 0u32, 0u32, 0u32, SCENE_OFFSET],
                );
            }
            _ => {}
        }
    }
}

impl super::Story {
    /// Makes a perk form `id` of this story's data with `highest` ranks.
    pub fn add_perk_form(&mut self, id: u32, highest: u8) {
        let form = new_form(&mut self.e, id);
        self.e
            .mem
            .set_u8(form + addr::PERK_HIGHEST_RANK_OFFSET, highest);
    }

    /// The karma this game's player was given in all.
    pub fn karma(&self) -> i32 {
        self.e.mem.u32(KARMA) as i32
    }

    /// The player's rank of perk `id` (for itself, or for its teammates).
    pub fn perk_rank(&mut self, id: u32, teammate: bool) -> u32 {
        let form = crate::forms::form(&mut self.e, id).expect("a perk of the story");
        rank(&mut self.e, form, teammate)
    }

    /// Gives script `script` (an address) `RewardKarma change`.
    pub fn script_karma(&mut self, script: u32, change: i32) {
        push(&mut self.e, script, Kind::Karma, [change as u32, 0, 0, 0]);
    }

    /// Gives script `script` `AddPerk perk teammate` (or `RemovePerk`) on reference `on` (a form
    /// id; 0 for the player).
    pub fn script_perk(&mut self, script: u32, perk: u32, teammate: bool, remove: bool, on: u32) {
        let kind = if remove {
            Kind::RemovePerk
        } else {
            Kind::AddPerk
        };
        push(
            &mut self.e,
            script,
            kind,
            [perk, u32::from(teammate), on, 0],
        );
    }
}
