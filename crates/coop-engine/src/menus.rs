//! Menus a line's result opens (the owner, 2026-11-22; council 3-0): a trade, repairs, a
//! recipe list, a companion's inventory open for the player who picked the line.
//!
//! The host's game runs a guest's line's result (docs/coop/SPECS.md S42): its quest changes and
//! items happen once, for everyone. A command in it that opens a menu would open it on the host's
//! screen (the guest asked Trudy for her wares; the trade window opened for the host). While the
//! host runs another player's result ([`talk::enacting`]), the four commands that open a menu with
//! the reference they run on are not run: each is kept ([`MenuOpen`], [`drain`]) for the session
//! to send to the player who picked the line, whose game opens it on its own copy of the NPC
//! ([`open`]), as the command's body would (`Script` functions `005ceeb0`, `005d5200`,
//! `005deb10`, `005d9430`, `tesscriptfunctions_p5`-`p8.rs`).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, PoisonError};

use engine_core::extend::{Owner, WrapSlot};
use engine_core::prelude::*;

use crate::{actors, addr, commands, forms, talk};

pub(crate) const BARTER: WrapSlot = WrapSlot::new(Owner::Coop, addr::CMD_SHOW_BARTER_MENU);
pub(crate) const REPAIR: WrapSlot = WrapSlot::new(Owner::Coop, addr::CMD_SHOW_REPAIR_MENU);
pub(crate) const TEAMMATE: WrapSlot = WrapSlot::new(Owner::Coop, addr::CMD_OPEN_TEAMMATE_CONTAINER);
pub(crate) const RECIPES: WrapSlot = WrapSlot::new(Owner::Coop, addr::CMD_SHOW_RECIPE_MENU);

/// A menu a script command opens with the reference it runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Menu {
    /// `ShowBarterMenu [discount]`: the trade window.
    Barter,
    /// `ShowRepairMenu`: the repair services.
    Repair,
    /// `OpenTeammateContainer [force]`: a companion's inventory.
    Teammate,
    /// `ShowRecipeMenu [category]`: the recipes (a campfire, a workbench).
    Recipes,
}

impl Menu {
    /// As a byte, for the wire.
    pub fn byte(self) -> u8 {
        match self {
            Menu::Barter => 0,
            Menu::Repair => 1,
            Menu::Teammate => 2,
            Menu::Recipes => 3,
        }
    }

    /// From [`Menu::byte`].
    pub fn from_byte(b: u8) -> Option<Menu> {
        [Menu::Barter, Menu::Repair, Menu::Teammate, Menu::Recipes]
            .into_iter()
            .find(|m| m.byte() == b)
    }

    fn body(self) -> u32 {
        match self {
            Menu::Barter => addr::CMD_SHOW_BARTER_MENU,
            Menu::Repair => addr::CMD_SHOW_REPAIR_MENU,
            Menu::Teammate => addr::CMD_OPEN_TEAMMATE_CONTAINER,
            Menu::Recipes => addr::CMD_SHOW_RECIPE_MENU,
        }
    }
}

/// A menu kept from another player's result run here: which, on which reference, with which
/// value (a discount, the force flag, a recipe category's form id).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuOpen {
    pub menu: Menu,
    pub npc: u32,
    pub value: u32,
}

#[derive(Default)]
pub struct Menus {
    kept: Mutex<Vec<MenuOpen>>,
    /// Menus opened here for the host's ask.
    opened: AtomicU32,
}

fn kept(e: &mut Engine) -> std::sync::MutexGuard<'_, Vec<MenuOpen>> {
    e.ext_free::<Menus>()
        .kept
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn barter(e: &mut Engine, a: &[u32]) -> Ret {
    hold(e, a, Menu::Barter)
}

pub(crate) fn repair(e: &mut Engine, a: &[u32]) -> Ret {
    hold(e, a, Menu::Repair)
}

pub(crate) fn teammate(e: &mut Engine, a: &[u32]) -> Ret {
    hold(e, a, Menu::Teammate)
}

pub(crate) fn recipes(e: &mut Engine, a: &[u32]) -> Ret {
    hold(e, a, Menu::Recipes)
}

/// The command's body, unless this game runs another player's result: then the menu is kept for
/// that player (its parameters read past, as the body would) and the command answers done.
fn hold(e: &mut Engine, a: &[u32], menu: Menu) -> Ret {
    let this_obj = a.get(2).copied().unwrap_or(0);
    if !talk::enacting(e) || this_obj == 0 {
        return e.call_next(menu.body(), a);
    }
    let value = match menu {
        Menu::Repair => 0,
        _ => commands::parse(e, a, false).map_or(0, |v| v[0]),
    };
    let value = match menu {
        // A recipe category is a form: sent as its id.
        Menu::Recipes if value != 0 => e.mem.u32(value.wrapping_add(addr::FORM_ID_OFFSET)),
        // Whether a companion's inventory opens is the story's: the host's copy of the companion
        // (its teammate byte) or the force flag decides, as the body would here.
        Menu::Teammate => {
            let opens = e
                .call(addr::ACTOR_FORCE_NEXT_UPDATE, &args![this_obj])
                .u32()
                & 0xff
                != 0
                || value != 0;
            if !opens {
                return 1u32.into_ret();
            }
            1
        }
        _ => value,
    };
    let npc = e.mem.u32(this_obj.wrapping_add(addr::FORM_ID_OFFSET));
    kept(e).push(MenuOpen { menu, npc, value });
    1u32.into_ret()
}

/// The menus kept since the last call.
pub fn drain(e: &mut Engine) -> Vec<MenuOpen> {
    std::mem::take(&mut *kept(e))
}

/// Why a menu was not opened here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotOpened {
    /// No such reference here.
    NoNpc,
    /// The reference is no actor (the barter and recipe commands can also open on a talking
    /// activator's speaker, which is not sent yet: the session logs it).
    NotAnActor,
    /// The recipe category is not in this game.
    NoCategory,
}

/// Opens `m` in this game on its copy of the NPC, as the command's body does for an actor: the
/// trade (`00704f80`, with the discount), the repairs (`00704690`), a companion's inventory
/// (`QueueMenuCreate(1, npc, 0, 0, 3, 0)`; the host's game decided it opens), the recipes
/// (`00704fc0`, with the category).
pub fn open(e: &mut Engine, m: MenuOpen) -> Result<(), NotOpened> {
    let npc = forms::form(e, m.npc).ok_or(NotOpened::NoNpc)?;
    if actors::actor(e, m.npc).is_none() {
        return Err(NotOpened::NotAnActor);
    }
    match m.menu {
        Menu::Barter => {
            e.call(addr::SHOW_BARTER, &args![npc, m.value]);
        }
        Menu::Repair => {
            e.call(addr::SHOW_REPAIR, &args![npc]);
        }
        Menu::Teammate => {
            e.call(
                addr::QUEUE_MENU_CREATE,
                &args![1u32, npc, 0u32, 0u32, 3u32, 0u32],
            );
        }
        Menu::Recipes => {
            let category = if m.value == 0 {
                0
            } else {
                forms::form(e, m.value).ok_or(NotOpened::NoCategory)?
            };
            e.call(addr::SHOW_RECIPES, &args![npc, category]);
        }
    }
    e.ext_free::<Menus>().opened.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

/// How many menus this game opened for the host's ask.
pub fn opened(e: &mut Engine) -> u32 {
    e.ext_free::<Menus>().opened.load(Ordering::SeqCst)
}
