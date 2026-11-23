//! What a line's result takes from the player (docs/coop/SPECS.md S30, S42 step 4): a payment
//! (`Player.RemoveItem Caps001 100`), a bribe, a hand-in (`Player.RemoveItem` of a quest item).
//!
//! The host's game runs a guest's line's result (S42): a `Player.RemoveItem` in it would take from
//! the host's player, while the guest, whose own game listed the line because its player had the
//! caps or the item, kept them. The rules give a payment to the player whose line it is
//! (`RewardKind::Payment`: the line's initiator). While the host runs another player's result
//! ([`talk::enacting`]), a `RemoveItem` on the host's player is not run: its item and count are
//! kept ([`Payment`], [`drain`]) for the session to send to the player who said the line, whose
//! game takes them from its own player as the command's body would ([`take`]): no more than it
//! carries, a worn one handled as the body does, the player's "removed" message shown unless the command
//! was silent.
//!
//! A form list (`Player.RemoveItem NVCompanionWeapons`) or any other form that is not an item
//! itself is left to the host's game as before.

use std::sync::{Mutex, PoisonError};

use engine_core::extend::{Owner, WrapSlot};
use engine_core::prelude::*;

use crate::{addr, commands, forms, talk};

/// The wrap at `005b4e90`, the `RemoveItem` command's body.
pub(crate) const REMOVE: WrapSlot = WrapSlot::new(Owner::Coop, addr::CMD_REMOVE_ITEM);

/// What another player's result took from the player here: the item's form id, how many, and
/// whether the command was silent (no message).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Payment {
    pub item: u32,
    pub count: u32,
    pub silent: bool,
}

#[derive(Default)]
struct Payments {
    kept: Mutex<Vec<Payment>>,
}

fn kept(e: &mut Engine) -> std::sync::MutexGuard<'_, Vec<Payment>> {
    e.ext_free::<Payments>()
        .kept
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

fn is_item(e: &mut Engine, form: u32) -> bool {
    e.vcall(form, addr::FORM_IS_ITEM_SLOT, &args![]).u32() & 0xff != 0
}

/// `RemoveItem item count [silent]`: the command's body, unless this game runs another player's
/// result and the reference is this game's player: then what it takes is kept for that player
/// and the command answers done (the script runner moves on by the line's length).
pub(crate) fn remove(e: &mut Engine, a: &[u32]) -> Ret {
    let this_obj = a.get(2).copied().unwrap_or(0);
    let player = e.mem.u32(addr::PLAYER_SINGLETON);
    if !talk::enacting(e) || this_obj == 0 || this_obj != player {
        return e.call_next(addr::CMD_REMOVE_ITEM, a);
    }
    // Read without moving the offset on: the body reads them again when it runs.
    let Some([form, count, silent, ..]) = commands::parse(e, a, true) else {
        return e.call_next(addr::CMD_REMOVE_ITEM, a);
    };
    if form == 0 || !is_item(e, form) {
        return e.call_next(addr::CMD_REMOVE_ITEM, a);
    }
    let item = e.mem.u32(form.wrapping_add(addr::FORM_ID_OFFSET));
    kept(e).push(Payment {
        item,
        count,
        silent: silent != 0,
    });
    1u32.into_ret()
}

/// The payments kept since the last call.
pub fn drain(e: &mut Engine) -> Vec<Payment> {
    std::mem::take(&mut *kept(e))
}

/// Takes payment `p` from this game's player as the `RemoveItem` command's body does for an
/// actor (`tesscriptfunctions.rs`, `fn_005b4e90`): no more than it carries
/// (`GetItemCountConditionFunction`), a worn one marked wearable again, then the
/// player's removal (virtual slot `0x17c`), the "removed" message unless silent, and the
/// interface's refresh. `None` when there is no player yet; else how many were taken (0: the item
/// is not in this game's data, or the player has none).
pub fn take(e: &mut Engine, p: Payment) -> Option<u32> {
    let player = e.mem.u32(addr::PLAYER_SINGLETON);
    if player == 0 {
        return None;
    }
    let Some(item) = forms::form(e, p.item).filter(|f| is_item(e, *f)) else {
        return Some(0);
    };
    let carried = e.with_stack(8, |e, have| {
        e.mem.set_f64(have.addr(), f64::from(p.count as i32));
        e.call(
            addr::GET_ITEM_COUNT_CONDITION,
            &args![player, item, 0u32, have],
        );
        e.mem.f64(have.addr())
    });
    let mut count = p.count as i32;
    if f64::from(count) > carried {
        // (`_ftol2`: toward zero)
        count = carried as i32;
    }
    let mut worn = 0u32;
    if e.call(addr::ACTOR_HAS_ITEM, &args![player, item]).u32() & 0xff != 0 {
        let changes = e.call(addr::REF_INVENTORY_CHANGES, &args![player]).u32();
        if changes != 0 {
            worn = e
                .call(addr::CHANGES_WEARING_OBJECT, &args![changes, item, 0u32])
                .u32();
            if worn != 0 {
                e.call(addr::EXTRA_SET_CAN_NOT_WEAR, &args![worn, 0u32]);
            }
        }
    }
    if count <= 0 {
        return Some(0);
    }
    if e.call(addr::ACTOR_HAS_ITEM, &args![player, item]).u32() & 0xff != 0 {
        e.call(
            addr::ACTOR_ITEMS_MEMBER_REMOVE,
            &args![
                player.wrapping_add(addr::ACTOR_ITEMS_MEMBER_OFFSET),
                item,
                1u32
            ],
        );
    }
    e.vcall(
        player,
        addr::REF_REMOVE_ITEM_SLOT,
        &args![
            item,
            worn,
            count as u32,
            0u32,
            0u32,
            0u32,
            0u32,
            0u32,
            1u32,
            0u32
        ],
    );
    if !p.silent {
        removed_message(e, player, item, count);
    }
    e.call(addr::INTERFACE_INVENTORY_CHANGED, &args![]);
    Some(count as u32)
}

/// The player's "removed" message of the `RemoveItem` command (`player_removed_message` in
/// `tesscriptfunctions.rs`): `"<count> <name><part b> <part a>"`, or `"<name> <part a>"` for one,
/// shown with the Vault Boy icon and the item's pick-up sound for the item types the game shows
/// it for.
fn removed_message(e: &mut Engine, player: u32, item: u32, count: i32) {
    let form_type = u32::from(e.mem.u8(item.wrapping_add(addr::FORM_TYPE_OFFSET)));
    if !matches!(
        form_type,
        0x18 | 0x19 | 0x1e | 0x1f | 0x28 | 0x29 | 0x2e | 0x2f | 0x32 | 0x67 | 0x6c | 0x73 | 0x74
    ) {
        return;
    }
    let text_of = |e: &mut Engine, global: u32| -> Vec<u8> {
        match e.call(addr::BS_STRING_TEXT, &args![global]).u32() {
            0 => Vec::new(),
            s => e.mem.cstr(s),
        }
    };
    let part_a = text_of(e, addr::REMOVED_MESSAGE_PART_A);
    let name = match e.call(addr::FULL_NAME_TEXT, &args![item]).u32() {
        0 => Vec::new(),
        s => e.mem.cstr(s),
    };
    let mut text: Vec<u8> = Vec::new();
    if count > 1 {
        let part_b = text_of(e, addr::REMOVED_MESSAGE_PART_B);
        text.extend_from_slice(count.to_string().as_bytes());
        text.push(b' ');
        text.extend_from_slice(&name);
        text.extend_from_slice(&part_b);
    } else {
        text.extend_from_slice(&name);
    }
    text.push(b' ');
    text.extend_from_slice(&part_a);
    text.truncate(addr::HUD_MESSAGE_MAX);
    let sound = e
        .call(
            addr::ACTOR_PICK_UP_SOUND_NAME,
            &args![player, item, 0u32, 0u32],
        )
        .u32();
    let volume: f32 = e.global(addr::PICK_UP_MESSAGE_VOLUME);
    let buffer = e
        .call(addr::OPERATOR_NEW, &args![text.len() as u32 + 1])
        .u32();
    if buffer == 0 {
        return;
    }
    e.mem.set_cstr(buffer, &text);
    e.call(
        addr::SHOW_HUD_MESSAGE,
        &args![
            buffer,
            0u32,
            addr::MESSAGE_ICON_VAULT_BOY,
            sound,
            volume,
            0u32
        ],
    );
    e.call(addr::OPERATOR_DELETE, &args![buffer]);
}
