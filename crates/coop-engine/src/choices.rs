//! A conversation's options in every player's dialogue window (S42 step 2, step C-15).
//!
//! The game builds a window's option list itself (`DialogMenu::LoadTopicsList`, `007638b0`):
//! one row per topic of the window's topic manager, each with the info whose conditions hold for
//! this game's player (its Intelligence, its skills, its quests), the row's index its place in
//! that list. The game calls it directly (from `LoadNextScreen`, itself called directly by
//! `DoIdle`, `DoClick` and `Create`), so co-op cannot wrap the game's own calls: it wraps the
//! calls co-op makes, and after each of those three ([`after_game`]) it takes a new list by the
//! time `LoadTopicsList` stamps on the menu (`uiIgnoreInputStartTime`). Each new list:
//!
//! - after the game's rows, the infos of those rows are noted: this game's own options
//!   ([`own`]), which the session sends to the host's dialogue table;
//! - the other players' options the table merged ([`set_foreign`]), those this game does not
//!   have, are added after them with the owner's name, e.g. `ICE CREAM! (Guest)`.
//!
//! A click on one of the added rows is not this game's to say (`DialogMenu::DoClick`,
//! `007624f0`, wrapped too): it is noted ([`take_clicked`]) and the game does nothing with it
//! (the session asks the table for it). A click on one of this game's own rows is said by the
//! game as any click, in the window showing another player's conversation too ([`crate::watch`]:
//! its results are then this game's, and the line it said is the step's pick). [`say`] clicks a
//! row of this game's own when the table asks this game to say it.
//!
//! A player in a conversation with another player gets a last row, `[Leave conversation]`
//! ([`set_leave`], S42 D12): a click on it is noted ([`take_leave`]) and the game does nothing
//! with it (the session closes this player's window and tells the table).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use engine_core::extend::{Owner, WrapSlot};
use engine_core::{args, Engine, Ret};

use crate::addr;

/// Co-op's place in front of `DialogMenu::LoadTopicsList`.
pub(crate) const LOAD_TOPICS: WrapSlot =
    WrapSlot::new(Owner::Coop, addr::DIALOG_MENU_LOAD_TOPICS_LIST);
/// Co-op's place in front of `DialogMenu::DoClick`.
pub(crate) const DO_CLICK: WrapSlot = WrapSlot::new(Owner::Coop, addr::DIALOG_MENU_DO_CLICK);

/// Most rows a window lists: more topics than this are not read (the game's lists are short).
pub const ROWS_MAX: usize = 64;
/// Most characters of an added row's text (the game formats its own into 0x200 bytes).
const ROW_TEXT_MAX: usize = 0x1f0;
/// The row that takes the player out of a conversation others go on with.
pub const LEAVE_TEXT: &str = "[Leave conversation]";

#[derive(Debug, Default)]
struct State {
    /// The window the rows below are of.
    menu: u32,
    /// The game's own rows: each one's info (its form id; 0 for a topic without one), by index.
    own: Vec<u32>,
    /// The other players' options: (info, the owner's name).
    foreign: Vec<(u32, String)>,
    /// The added rows' infos, by index after the game's own.
    rows: Vec<u32>,
    /// An added row clicked: its info.
    clicked: Option<u32>,
    /// Whether the window gets the `[Leave conversation]` row.
    leave: bool,
    /// The `[Leave conversation]` row's index in the window's list, when added.
    leave_row: Option<usize>,
    /// The `[Leave conversation]` row clicked.
    left: bool,
    /// The menu's `uiIgnoreInputStartTime` when its list was last taken.
    stamp: u32,
}

/// The game-wide state.
#[derive(Debug, Default)]
pub struct Choices {
    state: Mutex<State>,
    /// Option lists the game built.
    lists: AtomicU64,
    /// Rows added for the other players.
    rows_added: AtomicU64,
    /// Clicks not let through: an added row, or a row of a window showing another's conversation.
    clicks_held: AtomicU64,
}

fn state<R>(e: &mut Engine, f: impl FnOnce(&mut State) -> R) -> R {
    let c = e.ext_free::<Choices>();
    let mut s = c.state.lock().unwrap_or_else(PoisonError::into_inner);
    f(&mut s)
}

/// Installs the wraps where the game has the functions; called by [`crate::install`].
pub(crate) fn install(e: &mut Engine) {
    if e.is_registered(addr::DIALOG_MENU_LOAD_TOPICS_LIST) {
        e.wrap(LOAD_TOPICS, load_topics);
    }
    if e.is_registered(addr::DIALOG_MENU_DO_CLICK) {
        e.wrap(DO_CLICK, do_click);
    }
    let _ = e.ext_free::<Choices>();
}

/// `(option lists built, rows added for the other players, clicks held)` since the game started.
pub fn counts(e: &mut Engine) -> (u64, u64, u64) {
    let c = e.ext_free::<Choices>();
    (
        c.lists.load(Ordering::SeqCst),
        c.rows_added.load(Ordering::SeqCst),
        c.clicks_held.load(Ordering::SeqCst),
    )
}

/// This game's own options in the open dialogue window, as its last list had them (each an info's
/// form id, in the game's order): `None` when no window is open or its list was not built.
pub fn own(e: &mut Engine) -> Option<Vec<u32>> {
    let open = e.mem.u32(addr::DIALOG_MENU_OPEN);
    let (menu, own) = state(e, |s| (s.menu, s.own.clone()));
    (open != 0 && menu == open).then(|| own.into_iter().filter(|i| *i != 0).collect())
}

/// The other players' options, each an info's form id and the name of the player it is said by:
/// rows for those this game does not have are added to the window's list. When they changed and
/// the open window lists its options (state 2), the list is built again. Whether they changed.
pub fn set_foreign(e: &mut Engine, foreign: Vec<(u32, String)>) -> bool {
    let changed = state(e, |s| {
        if s.foreign == foreign {
            return false;
        }
        s.foreign = foreign;
        true
    });
    let menu = e.mem.u32(addr::DIALOG_MENU_OPEN);
    if changed
        && menu != 0
        && e.mem.u32(menu + addr::DIALOG_MENU_STATE_OFFSET) == 2
        && state(e, |s| s.menu == menu)
    {
        e.call(addr::DIALOG_MENU_LOAD_TOPICS_LIST, &args![menu]);
    }
    changed
}

/// Whether the window gets the `[Leave conversation]` row (after the options). When that
/// changed and the open window lists its options, the list is built again. Whether it changed.
pub fn set_leave(e: &mut Engine, leave: bool) -> bool {
    let changed = state(e, |s| std::mem::replace(&mut s.leave, leave) != leave);
    let menu = e.mem.u32(addr::DIALOG_MENU_OPEN);
    if changed
        && menu != 0
        && e.mem.u32(menu + addr::DIALOG_MENU_STATE_OFFSET) == 2
        && state(e, |s| s.menu == menu)
    {
        e.call(addr::DIALOG_MENU_LOAD_TOPICS_LIST, &args![menu]);
    }
    changed
}

/// The player's click on the `[Leave conversation]` row, for the test games (which build no
/// list of their own).
#[cfg(any(test, feature = "testkit"))]
pub(crate) fn note_leave_click(e: &mut Engine) {
    state(e, |s| s.left = true);
}

/// Whether windows get the `[Leave conversation]` row now.
pub fn leave_offered(e: &mut Engine) -> bool {
    state(e, |s| s.leave)
}

/// Whether the `[Leave conversation]` row was clicked since the last call.
pub fn take_leave(e: &mut Engine) -> bool {
    state(e, |s| std::mem::take(&mut s.left))
}

/// An added row (another player's option) clicked since the last call: its info.
pub fn take_clicked(e: &mut Engine) -> Option<u32> {
    state(e, |s| s.clicked.take())
}

/// Says option `info` (a form id) of this game's own in the open window listing its options, as
/// the player's click on its row (the game's `DoClick`, its half-second guard after a new list
/// lifted): the table asked this game to say it, another player having picked it (S42 step 2c).
/// False, and nothing done, when no window lists it as one of this game's own.
pub fn say(e: &mut Engine, info: u32) -> bool {
    let menu = e.mem.u32(addr::DIALOG_MENU_OPEN);
    if menu == 0 || info == 0 || e.mem.u32(menu + addr::DIALOG_MENU_STATE_OFFSET) != 2 {
        return false;
    }
    let index = state(e, |s| {
        (s.menu == menu)
            .then(|| s.own.iter().position(|i| *i == info))
            .flatten()
    });
    let Some(tile) = index.and_then(|i| row_tile(e, menu, i as u32)) else {
        return false;
    };
    e.mem
        .set_u32(menu + addr::DIALOG_MENU_IGNORE_INPUT_OFFSET, 0);
    e.call(addr::DIALOG_MENU_DO_CLICK, &args![menu, -1i32 as u32, tile]);
    log(|| format!("option {info:08x} said for another player's pick"));
    true
}

/// The tile of the window's row `index` (the list box's items: the tile at +0, the row's data,
/// its index, at +4).
fn row_tile(e: &mut Engine, menu: u32, index: u32) -> Option<u32> {
    let mut node = menu + addr::DIALOG_MENU_LIST_OFFSET + 4;
    for _ in 0..ROWS_MAX * 2 {
        let item = e.mem.u32(node);
        if item != 0 && e.mem.u32(item + 4) == index {
            return Some(e.mem.u32(item)).filter(|t| *t != 0);
        }
        node = e.mem.u32(node + 4);
        if node == 0 {
            break;
        }
    }
    None
}

/// The infos of the topic manager's list, by index (0 for a topic without an info).
fn listed(e: &mut Engine, menu: u32) -> Vec<u32> {
    let topics = e.mem.u32(menu + addr::DIALOG_MENU_TOPICS_OFFSET);
    let mut out = Vec::new();
    if topics == 0 || !e.is_registered(addr::TOPICS_NEXT_TOPIC) {
        return out;
    }
    let mut more = e
        .call(addr::TOPICS_FIRST_TOPIC, &args![topics, 1u32])
        .bool();
    while more && out.len() < ROWS_MAX {
        let topic = e.call(addr::TOPICS_CURRENT_TOPIC, &args![topics]).u32();
        let info = if topic == 0 {
            0
        } else {
            e.mem.u32(topic + addr::MENU_TOPIC_INFO_OFFSET)
        };
        out.push(if info == 0 {
            0
        } else {
            e.mem.u32(info + addr::FORM_ID_OFFSET)
        });
        more = e.call(addr::TOPICS_NEXT_TOPIC, &args![topics]).bool();
    }
    out
}

/// The text of info `id` as an option: its prompt, or without one its topic's name.
fn option_text(e: &mut Engine, id: u32) -> Option<String> {
    let (_, prompt) = crate::talk::line(e, id)?;
    if !prompt.trim().is_empty() {
        return Some(prompt);
    }
    let f = crate::forms::form(e, id)?;
    let topic = e.call(addr::TOPIC_GET_OWNER_TOPIC, &args![f]).u32();
    if topic == 0 || !e.is_registered(addr::FULL_NAME_CHARS) {
        return None;
    }
    let chars = e
        .call(
            addr::FULL_NAME_CHARS,
            &args![topic + addr::FORM_FULL_NAME_OFFSET],
        )
        .u32();
    let name: String = if chars == 0 {
        String::new()
    } else {
        e.mem.cstr(chars).into_iter().map(char::from).collect()
    };
    (!name.trim().is_empty()).then_some(name)
}

/// Adds a row at `index` with `text` to the window's list, as `LoadTopicsList` adds its own:
/// named `topic_<index>`, its index kept in trait `0xfac`, at full brightness.
fn add_row(e: &mut Engine, menu: u32, index: u32, text: &str) -> bool {
    let mut bytes: Vec<u8> = text
        .chars()
        .map(|c| if (c as u32) < 0x100 { c as u8 } else { b'?' })
        .filter(|b| *b != 0)
        .collect();
    bytes.truncate(ROW_TEXT_MAX);
    let name = format!("topic_{index}");
    let list = menu + addr::DIALOG_MENU_LIST_OFFSET;
    let tile = e.with_stack(0x200, |e, buffer| {
        e.mem.set_cstr(buffer.addr(), &bytes);
        e.call(
            addr::LIST_BOX_ADD_ITEM,
            &args![list, index, buffer.addr(), 0u32, 0u32],
        )
        .u32()
    });
    if tile == 0 {
        return false;
    }
    e.with_stack(0x20, |e, buffer| {
        e.mem.set_cstr(buffer.addr(), name.as_bytes());
        e.call(addr::TILE_SET_NAME, &args![tile, buffer.addr()]);
    });
    e.call(
        addr::TILE_SET_INT,
        &args![tile, addr::TILE_TRAIT_LIST_INDEX, index],
    );
    let alpha = e
        .call(addr::TILE_TEXT_TO_TRAIT, &args![addr::LINE_ALPHA_TEXT])
        .u32();
    e.call(addr::TILE_SET_INT, &args![tile, alpha, 0xffu32]);
    true
}

fn log(text: impl FnOnce() -> String) {
    if std::env::var_os("NV_COOP_TALK_LOG").is_some() {
        eprintln!("co-op choices: {}", text());
    }
}

/// The wrap at `007638b0`, for the calls co-op makes. Its word: the menu.
pub(crate) fn load_topics(e: &mut Engine, a: &[u32]) -> Ret {
    let r = e.call_next(addr::DIALOG_MENU_LOAD_TOPICS_LIST, a);
    if let Some(&menu) = a.first().filter(|m| **m != 0) {
        take_list(e, menu);
    }
    r
}

/// After a step of the game's window (`DoIdle`, `DoClick`, `Create`): a list the game built in it
/// (the open menu's `uiIgnoreInputStartTime` set anew) is taken as [`load_topics`] takes one, when
/// the window lists it (state 2). Reading the list moves the topic manager's current topic, which
/// the game reads in every other state (the line it says next: read at the window's opening, the
/// greeting was lost and the conversation closed); in state 2 the game's own `LoadTopicsList`
/// has just moved it past the list's end, as reading it again leaves it, and a click selects its
/// topic by index.
pub(crate) fn after_game(e: &mut Engine) {
    let menu = e.mem.u32(addr::DIALOG_MENU_OPEN);
    if menu == 0 || e.mem.u32(menu + addr::DIALOG_MENU_STATE_OFFSET) != 2 {
        return;
    }
    let stamp = e.mem.u32(menu + addr::DIALOG_MENU_IGNORE_INPUT_OFFSET);
    let new = stamp != 0 && state(e, |s| s.menu != menu || s.stamp != stamp);
    if new {
        take_list(e, menu);
    }
}

/// A list the game just built in `menu`: its own options noted, the other players' added.
fn take_list(e: &mut Engine, menu: u32) {
    crate::watch::clear_injected(e);
    let own = listed(e, menu);
    let (foreign, leave): (Vec<(u32, String)>, bool) = state(e, |s| {
        s.menu = menu;
        s.own = own.clone();
        s.rows.clear();
        s.leave_row = None;
        let foreign = s
            .foreign
            .iter()
            .filter(|(i, _)| !own.contains(i))
            .cloned()
            .collect();
        (foreign, s.leave)
    });
    e.ext_free::<Choices>().lists.fetch_add(1, Ordering::SeqCst);
    let mut rows = Vec::new();
    let all = e.is_registered(addr::LIST_BOX_ADD_ITEM)
        && e.is_registered(addr::LIST_BOX_SYNCHRONIZE)
        && e.is_registered(addr::TILE_SET_NAME);
    for (info, who) in foreign.into_iter().take(ROWS_MAX.saturating_sub(own.len())) {
        let Some(text) = option_text(e, info).filter(|_| all) else {
            continue;
        };
        let index = (own.len() + rows.len()) as u32;
        if !add_row(e, menu, index, &format!("{text} ({who})")) {
            break;
        }
        rows.push(info);
    }
    let leave_index = own.len() + rows.len();
    let leave_row = (leave && all && leave_index < ROWS_MAX)
        .then(|| add_row(e, menu, leave_index as u32, LEAVE_TEXT))
        .filter(|added| *added)
        .map(|_| leave_index);
    if !rows.is_empty() || leave_row.is_some() {
        e.call(
            addr::LIST_BOX_SYNCHRONIZE,
            &args![menu + addr::DIALOG_MENU_LIST_OFFSET],
        );
        e.ext_free::<Choices>()
            .rows_added
            .fetch_add(rows.len() as u64, Ordering::SeqCst);
    }
    log(|| format!("list of {menu:08x}: own {own:08x?}, added {rows:08x?}"));
    let stamp = e.mem.u32(menu + addr::DIALOG_MENU_IGNORE_INPUT_OFFSET);
    state(e, |s| {
        s.rows = rows;
        s.leave_row = leave_row;
        s.stamp = stamp;
    });
}

/// The wrap at `007624f0`. Its words: the menu, the clicked id, the tile.
pub(crate) fn do_click(e: &mut Engine, a: &[u32]) -> Ret {
    let (menu, id, tile) = match a {
        [menu, id, tile, ..] => (*menu, *id as i32, *tile),
        _ => return e.call_next(addr::DIALOG_MENU_DO_CLICK, a),
    };
    let row = id == -1
        && menu != 0
        && e.is_registered(addr::LIST_BOX_BELONGS_TO)
        && e.call(
            addr::LIST_BOX_BELONGS_TO,
            &args![menu + addr::DIALOG_MENU_LIST_OFFSET, tile],
        )
        .bool();
    if !row {
        let r = e.call_next(addr::DIALOG_MENU_DO_CLICK, a);
        after_game(e);
        return r;
    }
    let index = e
        .call(
            addr::TILE_GET_FLOAT,
            &args![tile, addr::TILE_TRAIT_LIST_INDEX],
        )
        .f64();
    let index = if index.is_finite() && index >= 0.0 {
        index as usize
    } else {
        usize::MAX
    };
    // The `[Leave conversation]` row: noted, nothing said.
    if state(e, |s| {
        let hit = s.menu == menu && s.leave_row == Some(index);
        s.left |= hit;
        hit
    }) {
        log(|| "[Leave conversation] clicked".to_string());
        e.ext_free::<Choices>()
            .clicks_held
            .fetch_add(1, Ordering::SeqCst);
        return Ret::default();
    }
    let added = state(e, |s| {
        if s.menu != menu || index < s.own.len() {
            return None;
        }
        Some(s.rows.get(index - s.own.len()).copied())
    });
    let Some(info) = added else {
        // One of this game's own rows: said here, in the watching window too (its results this
        // game's, the line it said the step's pick).
        let watched = crate::watch::is_window(e, menu);
        if watched {
            crate::watch::begin_own_click(e);
        } else {
            crate::watch::clear_injected(e);
        }
        let r = e.call_next(addr::DIALOG_MENU_DO_CLICK, a);
        if watched {
            let last = e.mem.u32(addr::DIALOG_LAST_INFO);
            let said = (last != 0).then(|| e.mem.u32(last + addr::FORM_ID_OFFSET));
            crate::watch::end_own_click(e, said);
            log(|| format!("a row of the watching window said here: {said:08x?}"));
        }
        after_game(e);
        return r;
    };
    state(e, |s| s.clicked = info);
    log(|| format!("another player's option clicked: {info:08x?}"));
    e.ext_free::<Choices>()
        .clicks_held
        .fetch_add(1, Ordering::SeqCst);
    Ret::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u32 = 0x0300_0000;
    const MENU: u32 = BASE + 0x1000;
    const ROOT: u32 = BASE + 0x1800;
    const TOPICS: u32 = BASE + 0x2000;
    const MENU_TOPICS: u32 = BASE + 0x3000;
    const INFOS: u32 = BASE + 0x4000;
    const PROMPTS: u32 = BASE + 0x6000;
    const TOPIC: u32 = BASE + 0x7000;
    const TOPIC_NAME: u32 = BASE + 0x7100;
    const ROWS: u32 = BASE + 0x8000;
    const ROW: u32 = 0x100;
    const TILES: u32 = BASE + 0xa000;
    const TILE: u32 = 0x20;
    const COUNTS: u32 = BASE + 0x9f00;
    const SAID: u32 = COUNTS;
    const SYNCS: u32 = COUNTS + 4;
    const GAME_LISTS: u32 = COUNTS + 8;
    const HEAP: u32 = COUNTS + 0xc;
    const ALPHA: u32 = 0x1234;
    const ITEMS: u32 = BASE + 0xb000;
    const NODES: u32 = BASE + 0xb800;
    /// Infos 0..5: form ids, the first four with a prompt, the last without one.
    const IDS: [u32; 5] = [0x10_0001, 0x10_0002, 0x10_0003, 0x10_0004, 0x10_0005];

    fn info_at(n: usize) -> u32 {
        INFOS + 0x100 * n as u32
    }

    fn eax(v: u32) -> Ret {
        Ret {
            eax: v,
            ..Ret::default()
        }
    }

    fn rows(e: &Engine) -> Vec<(u32, String)> {
        (0..e.mem.u32(ROWS))
            .map(|n| {
                let at = ROWS + 4 + ROW * n;
                let text = e.mem.cstr(at + 4).into_iter().map(char::from).collect();
                (e.mem.u32(at), text)
            })
            .collect()
    }

    fn tile(n: u32) -> u32 {
        TILES + TILE * n
    }

    /// The game: a window on its option list (state 2), the topic manager's list being the infos
    /// `listed` (by index into `IDS`).
    fn world(listed: &[usize]) -> Engine {
        let mut e = Engine::new();
        e.mem.map(BASE, 0x2_0000);
        e.mem.map(0x011d_0000, 0x3_0000);
        e.mem.map(0x0107_0000, 0x1_0000);
        e.mem.set_u32(HEAP, BASE + 0xc000);
        e.mem.set_u32(addr::DIALOG_MENU_OPEN, MENU);
        e.mem.set_u32(MENU + 4, ROOT);
        e.mem.set_u32(MENU + addr::DIALOG_MENU_STATE_OFFSET, 2);
        e.mem
            .set_u32(MENU + addr::DIALOG_MENU_TOPICS_OFFSET, TOPICS);
        for (n, id) in IDS.into_iter().enumerate() {
            let f = info_at(n);
            e.mem
                .set_u8(f + addr::FORM_TYPE_OFFSET, addr::FORM_TYPE_INFO);
            e.mem.set_u32(f + addr::FORM_ID_OFFSET, id);
            if n < 4 {
                let chars = PROMPTS + 0x40 * n as u32;
                e.mem.set_cstr(chars, format!("Prompt {n}").as_bytes());
                e.mem.set_u32(f + addr::INFO_PROMPT_OFFSET, chars);
            }
        }
        e.mem.set_cstr(TOPIC_NAME, b"Ringo");
        set_listed(&mut e, listed);
        e.register(addr::LOOKUP_FORM, |_, a| {
            eax(IDS.iter().position(|i| *i == a[0]).map_or(0, info_at))
        });
        e.register(addr::OPERATOR_NEW, |e, a| {
            let block = e.mem.u32(HEAP);
            e.mem.set_u32(HEAP, block + a[0].max(4).next_multiple_of(4));
            eax(block)
        });
        e.register(addr::OPERATOR_DELETE, |_, _| Ret::default());
        e.register(addr::STRING_CONSTRUCT, |e, a| {
            e.mem.set_u32(a[0], 0);
            Ret::default()
        });
        e.register(addr::STRING_DESTROY, |_, _| Ret::default());
        e.register(addr::INFO_GET_DISPLAY_TEXT, |_, _| Ret::default());
        e.register(addr::TOPIC_GET_OWNER_TOPIC, |_, _| eax(TOPIC));
        e.register(addr::FULL_NAME_CHARS, |_, a| {
            assert_eq!(a[0], TOPIC + addr::FORM_FULL_NAME_OFFSET);
            eax(TOPIC_NAME)
        });
        // The topic manager: its list, a cursor.
        e.register(addr::TOPICS_FIRST_TOPIC, |e, a| {
            e.mem.set_u32(a[0] + 4, 0);
            eax(u32::from(e.mem.u32(a[0]) > 0))
        });
        e.register(addr::TOPICS_NEXT_TOPIC, |e, a| {
            let at = e.mem.u32(a[0] + 4) + 1;
            e.mem.set_u32(a[0] + 4, at);
            eax(u32::from(at < e.mem.u32(a[0])))
        });
        e.register(addr::TOPICS_CURRENT_TOPIC, |e, a| {
            let at = e.mem.u32(a[0] + 4);
            eax(if at < e.mem.u32(a[0]) {
                e.mem.u32(a[0] + 0x10 + 4 * at)
            } else {
                0
            })
        });
        // The list box: rows (index, text), each a tile.
        e.register(addr::LIST_BOX_ADD_ITEM, |e, a| {
            assert_eq!(
                a[0],
                e.mem.u32(addr::DIALOG_MENU_OPEN) + addr::DIALOG_MENU_LIST_OFFSET
            );
            let n = e.mem.u32(ROWS);
            let at = ROWS + 4 + ROW * n;
            e.mem.set_u32(at, a[1]);
            let text = e.mem.cstr(a[2]);
            e.mem.set_cstr(at + 4, &text);
            e.mem.set_u32(ROWS, n + 1);
            // The list box's items, a list from the node embedded at +4: (tile, data).
            let item = ITEMS + 0x10 * n;
            e.mem.set_u32(item, tile(n));
            e.mem.set_u32(item + 4, a[1]);
            let head = a[0] + 4;
            let node = if n == 0 { head } else { NODES + 8 * n };
            e.mem.set_u32(node, item);
            e.mem.set_u32(node + 4, 0);
            if n > 0 {
                let prev = if n == 1 { head } else { NODES + 8 * (n - 1) };
                e.mem.set_u32(prev + 4, node);
            }
            eax(tile(n))
        });
        e.register(addr::TILE_SET_NAME, |e, a| {
            let n = (a[0] - TILES) / TILE;
            let name = e.mem.cstr(a[1]);
            e.mem.set_cstr(ROWS + 4 + ROW * n + 0x80, &name);
            Ret::default()
        });
        e.register(addr::TILE_SET_INT, |e, a| {
            match a[1] {
                addr::TILE_TRAIT_LIST_INDEX => e.mem.set_f32(a[0], a[2] as f32),
                ALPHA => e.mem.set_u32(a[0] + 4, a[2]),
                _ => {}
            }
            Ret::default()
        });
        e.register(addr::TILE_TEXT_TO_TRAIT, |_, _| eax(ALPHA));
        e.register(addr::TILE_GET_FLOAT, |e, a| {
            assert_eq!(a[1], addr::TILE_TRAIT_LIST_INDEX);
            Ret {
                st0: f64::from(e.mem.f32(a[0])),
                ..Ret::default()
            }
        });
        e.register(addr::LIST_BOX_SYNCHRONIZE, |e, _| {
            e.mem.set_u32(SYNCS, e.mem.u32(SYNCS) + 1);
            Ret::default()
        });
        e.register(addr::LIST_BOX_BELONGS_TO, |e, a| {
            eax(u32::from(a[1] >= TILES && a[1] < tile(e.mem.u32(ROWS))))
        });
        // The game's LoadTopicsList: the list box emptied, a row per topic of the manager's list,
        // the time stamped on the menu.
        e.register(addr::DIALOG_MENU_LOAD_TOPICS_LIST, game_list);
        // The game's DoClick: the row's index said.
        e.register(addr::DIALOG_MENU_DO_CLICK, |e, a| {
            let index = e.mem.f32(a[2]) as u32;
            e.mem.set_u32(SAID, index + 1);
            Ret::default()
        });
        install(&mut e);
        e
    }

    fn game_list(e: &mut Engine, a: &[u32]) -> Ret {
        {
            e.mem.set_u32(GAME_LISTS, e.mem.u32(GAME_LISTS) + 1);
            e.mem.set_u32(ROWS, 0);
            let topics = e.mem.u32(a[0] + addr::DIALOG_MENU_TOPICS_OFFSET);
            let mut more = e
                .call(addr::TOPICS_FIRST_TOPIC, &args![topics, 1u32])
                .bool();
            let mut index = 0u32;
            while more {
                let t = e.call(addr::TOPICS_CURRENT_TOPIC, &args![topics]).u32();
                let info = e.mem.u32(t + addr::MENU_TOPIC_INFO_OFFSET);
                let text = e
                    .mem
                    .cstr(e.mem.u32(info + addr::INFO_PROMPT_OFFSET))
                    .to_vec();
                let tile = e.with_stack(0x40, |e, b| {
                    e.mem.set_cstr(b.addr(), &text);
                    e.call(
                        addr::LIST_BOX_ADD_ITEM,
                        &args![
                            a[0] + addr::DIALOG_MENU_LIST_OFFSET,
                            index,
                            b.addr(),
                            0u32,
                            0u32
                        ],
                    )
                    .u32()
                });
                e.call(
                    addr::TILE_SET_INT,
                    &args![tile, addr::TILE_TRAIT_LIST_INDEX, index],
                );
                index += 1;
                more = e.call(addr::TOPICS_NEXT_TOPIC, &args![topics]).bool();
            }
            let lists = e.mem.u32(GAME_LISTS);
            e.mem
                .set_u32(a[0] + addr::DIALOG_MENU_IGNORE_INPUT_OFFSET, 1000 + lists);
            Ret::default()
        }
    }

    fn set_listed(e: &mut Engine, listed: &[usize]) {
        e.mem.set_u32(TOPICS, listed.len() as u32);
        for (k, n) in listed.iter().enumerate() {
            let t = MENU_TOPICS + 0x40 * k as u32;
            e.mem.set_u32(t + addr::MENU_TOPIC_INFO_OFFSET, info_at(*n));
            e.mem.set_u32(TOPICS + 0x10 + 4 * k as u32, t);
        }
    }

    fn list(e: &mut Engine) {
        e.call(addr::DIALOG_MENU_LOAD_TOPICS_LIST, &args![MENU]);
    }

    fn click(e: &mut Engine, row: u32) {
        e.mem.set_u32(SAID, 0);
        e.call(
            addr::DIALOG_MENU_DO_CLICK,
            &args![MENU, -1i32 as u32, tile(row)],
        );
    }

    #[test]
    fn the_games_own_options_are_noted_and_the_others_added_with_their_names() {
        let mut e = world(&[0, 1]);
        assert_eq!(own(&mut e), None, "no list built yet");
        list(&mut e);
        assert_eq!(own(&mut e), Some(vec![IDS[0], IDS[1]]));
        // Another player's options: one this game has too (not added again), two it lacks, one
        // with no prompt (its topic's name).
        assert!(set_foreign(
            &mut e,
            vec![
                (IDS[1], "Player 1".into()),
                (IDS[2], "Player 2".into()),
                (IDS[4], "Player 2".into()),
            ]
        ));
        // The window lists its options: built again at once.
        assert_eq!(e.mem.u32(GAME_LISTS), 2);
        assert_eq!(
            rows(&e),
            [
                (0, "Prompt 0".into()),
                (1, "Prompt 1".into()),
                (2, "Prompt 2 (Player 2)".into()),
                (3, "Ringo (Player 2)".into()),
            ]
        );
        assert_eq!(e.mem.cstr(ROWS + 4 + ROW * 3 + 0x80), b"topic_3");
        assert_eq!(e.mem.f32(tile(3)), 3.0);
        assert_eq!(e.mem.u32(tile(3) + 4), 0xff);
        assert_eq!(e.mem.u32(SYNCS), 1);
        // The own options are the game's, not the added ones.
        assert_eq!(own(&mut e), Some(vec![IDS[0], IDS[1]]));
        assert_eq!(counts(&mut e), (2, 2, 0));
        // The same again: nothing built.
        assert!(!set_foreign(
            &mut e,
            vec![
                (IDS[1], "Player 1".into()),
                (IDS[2], "Player 2".into()),
                (IDS[4], "Player 2".into()),
            ]
        ));
        assert_eq!(e.mem.u32(GAME_LISTS), 2);
    }

    #[test]
    fn a_click_on_an_added_row_is_noted_not_said() {
        let mut e = world(&[0, 1]);
        set_foreign(&mut e, vec![(IDS[3], "Player 2".into())]);
        list(&mut e);
        click(&mut e, 1);
        assert_eq!(e.mem.u32(SAID), 2, "the game's own row: said");
        assert_eq!(take_clicked(&mut e), None);
        click(&mut e, 2);
        assert_eq!(e.mem.u32(SAID), 0, "another player's: not said here");
        assert_eq!(take_clicked(&mut e), Some(IDS[3]));
        assert_eq!(take_clicked(&mut e), None, "taken once");
        assert_eq!(counts(&mut e).2, 1);
        // Not a row (the text clicked): the game's.
        e.mem.set_u32(SAID, 0);
        e.call(addr::DIALOG_MENU_DO_CLICK, &args![MENU, 0u32, ROOT]);
        assert_eq!(take_clicked(&mut e), None);
    }

    #[test]
    fn the_leave_row_comes_last_and_a_click_on_it_is_noted_not_said() {
        let mut e = world(&[0, 1]);
        set_foreign(&mut e, vec![(IDS[3], "Player 2".into())]);
        list(&mut e);
        assert_eq!(rows(&e).len(), 3, "no leave row unless asked");
        // Asked while the window lists its options: built again at once, the row last.
        assert!(set_leave(&mut e, true));
        assert_eq!(e.mem.u32(GAME_LISTS), 2);
        assert!(!set_leave(&mut e, true), "unchanged");
        assert_eq!(e.mem.u32(GAME_LISTS), 2);
        assert_eq!(
            rows(&e).last(),
            Some(&(3, LEAVE_TEXT.to_string())),
            "{:?}",
            rows(&e)
        );
        assert_eq!(e.mem.f32(tile(3)), 3.0);
        // Its click: noted, not said, not another player's option.
        click(&mut e, 3);
        assert_eq!(e.mem.u32(SAID), 0);
        assert!(take_leave(&mut e));
        assert!(!take_leave(&mut e), "taken once");
        assert_eq!(take_clicked(&mut e), None);
        // The other rows as before.
        click(&mut e, 2);
        assert!(!take_leave(&mut e));
        assert_eq!(take_clicked(&mut e), Some(IDS[3]));
        click(&mut e, 0);
        assert!(!take_leave(&mut e));
        assert_eq!(e.mem.u32(SAID), 1);
        // Taken away: gone from the next list; a click on that row is the game's again.
        assert!(set_leave(&mut e, false));
        assert_eq!(rows(&e).len(), 3);
        click(&mut e, 3);
        assert!(!take_leave(&mut e));
    }

    #[test]
    fn the_list_is_built_again_only_when_the_window_lists_its_options() {
        let mut e = world(&[0]);
        list(&mut e);
        // The window says a line (state 3): the new options wait for its next list.
        e.mem.set_u32(MENU + addr::DIALOG_MENU_STATE_OFFSET, 3);
        assert!(set_foreign(&mut e, vec![(IDS[2], "Player 2".into())]));
        assert_eq!(e.mem.u32(GAME_LISTS), 1);
        e.mem.set_u32(MENU + addr::DIALOG_MENU_STATE_OFFSET, 2);
        set_listed(&mut e, &[0, 1]);
        list(&mut e);
        assert_eq!(rows(&e).len(), 3);
        // Another window open (its list not built): not this one's, nothing built.
        e.mem.set_u32(addr::DIALOG_MENU_OPEN, BASE + 0x1400);
        assert_eq!(own(&mut e), None);
        assert!(set_foreign(&mut e, vec![]));
        assert_eq!(e.mem.u32(GAME_LISTS), 2);
        // No window: nothing.
        e.mem.set_u32(addr::DIALOG_MENU_OPEN, 0);
        assert_eq!(own(&mut e), None);
    }

    #[test]
    fn an_option_this_game_cannot_name_is_not_added() {
        let mut e = world(&[0]);
        // An id that is no info here.
        set_foreign(
            &mut e,
            vec![(0x77, "Player 2".into()), (IDS[2], "Player 2".into())],
        );
        list(&mut e);
        assert_eq!(
            rows(&e),
            [(0, "Prompt 0".into()), (1, "Prompt 2 (Player 2)".into())]
        );
        click(&mut e, 1);
        assert_eq!(take_clicked(&mut e), Some(IDS[2]));
    }

    /// The game builds its lists by direct calls, which no wrap sees: a list built in a step of
    /// the window is taken after it, by the time it stamped on the menu.
    #[test]
    fn a_list_the_game_built_itself_is_taken_after_its_step() {
        let mut e = world(&[0]);
        set_foreign(&mut e, vec![(IDS[2], "Player 2".into())]);
        // A step with no list built: nothing.
        after_game(&mut e);
        assert_eq!(own(&mut e), None);
        // A list built while the window is about to say a line (state 3): not read (its topic
        // manager's current topic is the line's).
        game_list(&mut e, &[MENU]);
        e.mem.set_u32(MENU + addr::DIALOG_MENU_STATE_OFFSET, 3);
        after_game(&mut e);
        assert_eq!(own(&mut e), None);
        e.mem.set_u32(MENU + addr::DIALOG_MENU_STATE_OFFSET, 2);
        // The game's DoIdle built one (no wrap called).
        game_list(&mut e, &[MENU]);
        assert_eq!(rows(&e).len(), 1);
        after_game(&mut e);
        assert_eq!(own(&mut e), Some(vec![IDS[0]]));
        assert_eq!(rows(&e).len(), 2, "the other player's row added");
        // The next steps, the same list: nothing added again.
        after_game(&mut e);
        after_game(&mut e);
        assert_eq!(rows(&e).len(), 2);
        assert_eq!(counts(&mut e).0, 1);
        // DoClick clears the time half a second on: not a new list.
        e.mem
            .set_u32(MENU + addr::DIALOG_MENU_IGNORE_INPUT_OFFSET, 0);
        after_game(&mut e);
        assert_eq!(counts(&mut e).0, 1);
        // A new list: taken.
        set_listed(&mut e, &[0, 1]);
        game_list(&mut e, &[MENU]);
        after_game(&mut e);
        assert_eq!(own(&mut e), Some(vec![IDS[0], IDS[1]]));
        assert_eq!(rows(&e).len(), 3);
        // Another window's list (a new menu, its time as the last one's): taken.
        e.mem.set_u32(addr::DIALOG_MENU_OPEN, BASE + 0x1400);
        e.mem.set_u32(
            BASE + 0x1400 + addr::DIALOG_MENU_IGNORE_INPUT_OFFSET,
            e.mem.u32(MENU + addr::DIALOG_MENU_IGNORE_INPUT_OFFSET),
        );
        e.mem
            .set_u32(BASE + 0x1400 + addr::DIALOG_MENU_TOPICS_OFFSET, TOPICS);
        e.mem
            .set_u32(BASE + 0x1400 + addr::DIALOG_MENU_STATE_OFFSET, 2);
        after_game(&mut e);
        assert_eq!(counts(&mut e).0, 3);
    }

    /// S42 step 2c: the table asks this game to say an option of its own: its row is clicked as
    /// the player would, the half-second guard after a new list lifted.
    #[test]
    fn an_option_of_this_games_own_is_said_as_a_click_on_its_row() {
        let mut e = world(&[0, 1]);
        set_foreign(&mut e, vec![(IDS[3], "Player 2".into())]);
        list(&mut e);
        e.mem
            .set_u32(MENU + addr::DIALOG_MENU_IGNORE_INPUT_OFFSET, 1234);
        assert!(say(&mut e, IDS[1]));
        assert_eq!(e.mem.u32(SAID), 2, "row 1 clicked");
        assert_eq!(e.mem.u32(MENU + addr::DIALOG_MENU_IGNORE_INPUT_OFFSET), 0);
        // Another player's option (an added row), or one listed nowhere: not this game's.
        e.mem.set_u32(SAID, 0);
        assert!(!say(&mut e, IDS[3]));
        assert!(!say(&mut e, IDS[2]));
        assert!(!say(&mut e, 0));
        assert_eq!(e.mem.u32(SAID), 0);
        // Not while the window says a line.
        e.mem.set_u32(MENU + addr::DIALOG_MENU_STATE_OFFSET, 3);
        assert!(!say(&mut e, IDS[0]));
        // Another window: its list was not taken.
        e.mem.set_u32(MENU + addr::DIALOG_MENU_STATE_OFFSET, 2);
        e.mem.set_u32(addr::DIALOG_MENU_OPEN, BASE + 0x1400);
        e.mem
            .set_u32(BASE + 0x1400 + addr::DIALOG_MENU_STATE_OFFSET, 2);
        assert!(!say(&mut e, IDS[0]));
        assert_eq!(e.mem.u32(SAID), 0);
        // The first row.
        e.mem.set_u32(addr::DIALOG_MENU_OPEN, MENU);
        assert!(say(&mut e, IDS[0]));
        assert_eq!(e.mem.u32(SAID), 1);
    }
}
