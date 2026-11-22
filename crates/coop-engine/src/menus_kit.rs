//! The test game's menus a script command opens (`menus`): the four commands' bodies as the
//! game's do it (parse, the reference an actor, the menu's function), the menus' functions noting
//! what opened, and a script block's `ShowBarterMenu` (and the others) for a line's result.

use engine_core::prelude::*;

use super::{eax, SCENE_ARGS, SCENE_OFFSET};
use crate::{addr, menus::Menu};

/// The kit's own memory.
const MK: u32 = 0x0174_0000;
/// The menus opened in this game: how many, then the last one's menu (a [`Menu::byte`]), the
/// actor's form id and the value.
const OPENED: u32 = MK;
/// In any script's block: a menu its body opens with the reference it runs on (its
/// [`Menu::byte`] plus one, 0 for none), and the command's parameter.
pub(crate) const SCRIPT_MENU: u32 = 0x74;

/// Called by `Story::new` before co-op is installed.
pub(super) fn register(e: &mut Engine) {
    e.map(MK, 0x1000);
    // The commands, as `tesscriptfunctions_p5`-`p8.rs` have them for an actor.
    e.register(addr::CMD_SHOW_BARTER_MENU, |e, a| {
        let v = parse(e, a);
        if a[2] != 0 && a[2] != e.mem.u32(addr::PLAYER_SINGLETON) && is_actor(e, a[2]) {
            e.call(addr::SHOW_BARTER, &args![a[2], v]);
        }
        eax(1)
    });
    e.register(addr::CMD_SHOW_REPAIR_MENU, |e, a| {
        if a[2] != 0 && is_actor(e, a[2]) {
            e.call(addr::SHOW_REPAIR, &args![a[2]]);
        }
        eax(1)
    });
    e.register(addr::CMD_OPEN_TEAMMATE_CONTAINER, |e, a| {
        if a[2] != 0 && is_actor(e, a[2]) {
            let flag = parse(e, a);
            let teammate = e.call(addr::ACTOR_FORCE_NEXT_UPDATE, &args![a[2]]).u32() & 0xff;
            if teammate != 0 || flag != 0 {
                e.call(
                    addr::QUEUE_MENU_CREATE,
                    &args![1u32, a[2], 0u32, 0u32, 3u32, 0u32],
                );
            }
        }
        eax(1)
    });
    e.register(addr::CMD_SHOW_RECIPE_MENU, |e, a| {
        let v = parse(e, a);
        if a[2] != 0 && is_actor(e, a[2]) {
            e.call(addr::SHOW_RECIPES, &args![a[2], v]);
        }
        eax(1)
    });
    // The menus' functions.
    e.register(addr::SHOW_BARTER, |e, a| {
        note(e, Menu::Barter, a[0], a[1]);
        Ret::default()
    });
    e.register(addr::SHOW_REPAIR, |e, a| {
        note(e, Menu::Repair, a[0], 0);
        Ret::default()
    });
    e.register(addr::SHOW_RECIPES, |e, a| {
        let category = match a[1] {
            0 => 0,
            f => e.mem.u32(f + addr::FORM_ID_OFFSET),
        };
        note(e, Menu::Recipes, a[0], category);
        Ret::default()
    });
    e.register(addr::QUEUE_MENU_CREATE, |e, a| {
        if a[0] == 1 && a[4] == 3 {
            note(e, Menu::Teammate, a[1], 0);
        }
        Ret::default()
    });
    e.register(addr::ACTOR_FORCE_NEXT_UPDATE, |e, a| {
        eax(u32::from(e.mem.u8(a[0] + addr::ACTOR_TEAMMATE_OFFSET)))
    });
}

fn is_actor(e: &mut Engine, r: u32) -> bool {
    crate::actors::is_actor_type(e.mem.u8(r + addr::FORM_TYPE_OFFSET))
}

/// The command's one parameter, as the kit's `ParseParameters` gives it (0 when none).
fn parse(e: &mut Engine, a: &[u32]) -> u32 {
    let outs = SCENE_ARGS + 0x40;
    e.mem.set_u32(outs, 0);
    let mut words = args![a[0], a[1], a[7], a[2], a[3], a[4], a[5]];
    words.extend((0..6).map(|i| outs + 4 * i));
    e.call(addr::SCRIPT_PARSE_PARAMETERS, &words);
    e.mem.u32(outs)
}

fn note(e: &mut Engine, m: Menu, actor: u32, value: u32) {
    let n = e.mem.u32(OPENED);
    e.mem.set_u32(OPENED, n + 1);
    e.mem.set_u32(OPENED + 4, u32::from(m.byte()));
    e.mem
        .set_u32(OPENED + 8, e.mem.u32(actor + addr::FORM_ID_OFFSET));
    e.mem.set_u32(OPENED + 12, value);
}

/// The kit's script runner: a script whose block names a menu runs its command on `reference`
/// (the command's parameter in the kit's parse).
pub(super) fn run(e: &mut Engine, script: u32, reference: u32) {
    let at = script.wrapping_add(SCRIPT_MENU);
    if script == 0 || !e.mem.is_mapped(at) {
        return;
    }
    let Some(menu) = e
        .mem
        .u32(at)
        .checked_sub(1)
        .and_then(|b| Menu::from_byte(b as u8))
    else {
        return;
    };
    let value = e.mem.u32(at + 4);
    e.mem.set_u32(SCENE_ARGS, value);
    e.mem.set_u32(SCENE_OFFSET, 0);
    let body = match menu {
        Menu::Barter => addr::CMD_SHOW_BARTER_MENU,
        Menu::Repair => addr::CMD_SHOW_REPAIR_MENU,
        Menu::Teammate => addr::CMD_OPEN_TEAMMATE_CONTAINER,
        Menu::Recipes => addr::CMD_SHOW_RECIPE_MENU,
    };
    e.call(
        body,
        &args![0u32, 0u32, reference, 0u32, 0u32, 0u32, 0u32, SCENE_OFFSET],
    );
}

impl super::Story {
    /// The menus opened in this game: how many, and the last (its menu, the actor's id, the
    /// value), `None` before any.
    pub fn menus_opened(&self) -> (u32, Option<(Menu, u32, u32)>) {
        let n = self.e.mem.u32(OPENED);
        let last = (n != 0).then(|| {
            (
                Menu::from_byte(self.e.mem.u32(OPENED + 4) as u8).expect("a menu"),
                self.e.mem.u32(OPENED + 8),
                self.e.mem.u32(OPENED + 12),
            )
        });
        (n, last)
    }
}
