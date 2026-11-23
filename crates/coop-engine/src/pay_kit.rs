//! The test game's `RemoveItem` (`pay`): the command's body as the game's does it for the player
//! (parse, what it carries, the player's removal, the message), the functions [`crate::pay::take`]
//! calls, and a script block's `Player.RemoveItem` for a line's result.

use engine_core::prelude::*;

use super::{eax, BAG_FORMS, BAG_INVENTORY, SCENE_ARGS, SCENE_OFFSET};
use crate::addr;

/// The kit's own memory: the item forms' virtual table, how many times the command's body ran,
/// the message's two parts, and the virtual table of a form that is not an item.
const PK: u32 = 0x0173_0000;
const ITEM_VTABLE: u32 = PK;
const BODY_RUNS: u32 = PK + 0x100;
const PART_A: u32 = PK + 0x200;
const PART_B: u32 = PK + 0x220;
const NOT_ITEM_VTABLE: u32 = PK + 0x300;
/// The functions behind the item forms' "is an item" slot (and another form's), and the player's
/// removal slot.
const IS_ITEM_FN: u32 = 0x0130_0100;
const PLAYER_REMOVE_FN: u32 = 0x0130_0110;
const IS_NOT_ITEM_FN: u32 = 0x0130_0120;
/// In any script's block: an item its body takes (`RemoveItem`; its form id, 0 for none), how
/// many, whether silent, and from which reference (its form id; 0 for the player).
pub(crate) const SCRIPT_REMOVE_ITEM: u32 = 0x50;
/// An item form's name: at +0x10 of the kit's 0x20-byte item forms.
const ITEM_NAME: u32 = 0x10;
const ITEM_NAME_MAX: usize = 15;

/// Called by `Story::new` before co-op is installed, after the player and the bag exist.
pub(super) fn register(e: &mut Engine, player_vtable: u32) {
    e.map(PK, 0x1000);
    e.mem
        .set_u32(ITEM_VTABLE + addr::FORM_IS_ITEM_SLOT, IS_ITEM_FN);
    e.register(IS_ITEM_FN, |_, _| eax(1));
    e.mem
        .set_u32(NOT_ITEM_VTABLE + addr::FORM_IS_ITEM_SLOT, IS_NOT_ITEM_FN);
    e.register(IS_NOT_ITEM_FN, |_, _| eax(0));
    e.mem.set_cstr(PART_A, b"removed");
    e.mem.set_cstr(PART_B, b"(s)");
    e.mem
        .set_u32(player_vtable + addr::REF_REMOVE_ITEM_SLOT, PLAYER_REMOVE_FN);
    // The player's removal (item, worn, count, ...): out of the bag's inventory.
    // (The game's command takes no more than the player carries before it calls this.)
    e.register(PLAYER_REMOVE_FN, |e, a| {
        let have = carried(e, a[1]);
        assert!(a[3] <= have, "removes {} of {have}", a[3]);
        set_carried(e, a[1], have - a[3]);
        Ret::default()
    });
    // `GetItemCountConditionFunction` (actor, item, 0, double*): the player's count.
    e.register(addr::GET_ITEM_COUNT_CONDITION, |e, a| {
        let n = if a[0] == e.mem.u32(addr::PLAYER_SINGLETON) {
            carried(e, a[1])
        } else {
            0
        };
        e.mem.set_f64(a[3], f64::from(n));
        eax(1)
    });
    // The kit's player wears nothing.
    e.register(addr::ACTOR_HAS_ITEM, |_, _| eax(0));
    e.register(addr::INTERFACE_INVENTORY_CHANGED, |_, _| Ret::default());
    e.register(addr::ACTOR_PICK_UP_SOUND_NAME, |_, _| eax(0));
    e.register(addr::BS_STRING_TEXT, |_, a| {
        eax(match a[0] {
            addr::REMOVED_MESSAGE_PART_A => PART_A,
            addr::REMOVED_MESSAGE_PART_B => PART_B,
            _ => 0,
        })
    });
    let volume = addr::PICK_UP_MESSAGE_VOLUME & !0xfff;
    if !e.mem.is_mapped(volume) {
        e.map(volume, 0x1000);
    }
    // The command's body for the player, as the game's (`fn_005b4e90`): parse, no more than the
    // player carries, the removal, the message.
    e.register(addr::CMD_REMOVE_ITEM, |e, a| {
        let runs = e.mem.u32(BODY_RUNS);
        e.mem.set_u32(BODY_RUNS, runs + 1);
        let outs = SCENE_ARGS + 0x40;
        let mut words = args![a[0], a[1], a[7], a[2], a[3], a[4], a[5]];
        words.extend((0..6).map(|i| outs + 4 * i));
        e.call(addr::SCRIPT_PARSE_PARAMETERS, &words);
        let (form, count) = (e.mem.u32(outs), e.mem.u32(outs + 4));
        if a[2] != 0 && a[2] == e.mem.u32(addr::PLAYER_SINGLETON) && form != 0 {
            let have = carried(e, form);
            set_carried(e, form, have - count.min(have));
        }
        eax(1)
    });
}

/// Item forms of the bag are items (their virtual table's slot says so).
pub(super) fn make_item(e: &mut Engine, form: u32) {
    e.mem.set_u32(form, ITEM_VTABLE);
}

/// The name of a bag's item form, for the game's full-name call (`None` for any other form).
pub(crate) fn item_name(e: &mut Engine, form: u32) -> Option<u32> {
    ((BAG_FORMS..BAG_INVENTORY).contains(&form) && e.mem.u32(form) == ITEM_VTABLE)
        .then_some(form + ITEM_NAME)
}

fn slot_of(e: &mut Engine, form: u32) -> u32 {
    let mut slot = BAG_INVENTORY;
    while e.mem.u32(slot) != 0 && e.mem.u32(slot) != form {
        slot += 8;
    }
    slot
}

fn carried(e: &mut Engine, form: u32) -> u32 {
    let slot = slot_of(e, form);
    if e.mem.u32(slot) == form {
        e.mem.u32(slot + 4)
    } else {
        0
    }
}

fn set_carried(e: &mut Engine, form: u32, n: u32) {
    let slot = slot_of(e, form);
    e.mem.set_u32(slot, form);
    e.mem.set_u32(slot + 4, n);
}

/// The kit's script runner: a script whose block names an item to take runs `RemoveItem` on the
/// player.
pub(super) fn run(e: &mut Engine, script: u32) {
    let at = script.wrapping_add(SCRIPT_REMOVE_ITEM);
    if script == 0 || !e.mem.is_mapped(at) || e.mem.u32(at) == 0 {
        return;
    }
    let Some(item) = crate::forms::form(e, e.mem.u32(at)) else {
        return;
    };
    e.mem.set_u32(SCENE_ARGS, item);
    e.mem.set_u32(SCENE_ARGS + 4, e.mem.u32(at + 4));
    e.mem.set_u32(SCENE_ARGS + 8, e.mem.u32(at + 8));
    e.mem.set_u32(SCENE_OFFSET, 0);
    let reference = match e.mem.u32(at + 12) {
        0 => e.mem.u32(addr::PLAYER_SINGLETON),
        id => crate::forms::form(e, id).unwrap_or(0),
    };
    e.call(
        addr::CMD_REMOVE_ITEM,
        &args![0u32, 0u32, reference, 0u32, 0u32, 0u32, 0u32, SCENE_OFFSET],
    );
}

impl super::Story {
    /// Makes a form `id` of this story's data that is not an item itself (as a form list).
    pub fn add_non_item_form(&mut self, id: u32) {
        self.add_item_form(id);
        let form = crate::forms::form(&mut self.e, id).expect("just made");
        self.e.mem.set_u32(form, NOT_ITEM_VTABLE);
    }

    /// How many times the `RemoveItem` command's body ran in this game.
    pub fn remove_item_runs(&self) -> u32 {
        self.e.mem.u32(BODY_RUNS)
    }

    /// Sets how many of item form `id` (made with [`super::Story::add_item_form`]) the player
    /// carries.
    pub fn set_player_items(&mut self, id: u32, n: u32) {
        let form = crate::forms::form(&mut self.e, id).expect("an item form of the story");
        set_carried(&mut self.e, form, n);
    }

    /// Gives item form `id` (made with [`super::Story::add_item_form`]) a name and a form type the
    /// game shows its "removed" message for (a misc item's).
    pub fn name_item(&mut self, id: u32, name: &str) {
        let form = crate::forms::form(&mut self.e, id).expect("an item form of the story");
        assert!(name.len() <= ITEM_NAME_MAX && item_name(&mut self.e, form).is_some());
        self.e.mem.set_cstr(form + ITEM_NAME, name.as_bytes());
        self.e.mem.set_u8(form + addr::FORM_TYPE_OFFSET, 0x1f);
    }
}
