//! Watching another player's conversation in the game's own dialogue window (docs/coop/SPECS.md
//! S42, step 1; the council's choice, 3-0, 2026-11-18).
//!
//! When another player talks to an NPC and this player is with it, this game opens its own
//! `DialogMenu` on its NPC (a copy, where another game runs it) and shows each line the talker's
//! game said, with the NPC's voice and lip movement, the way the talker sees it. The window only
//! shows: nothing the player does in it says a line.
//!
//! - **Opening.** [`watch_line`] opens the window through the game's `DialogMenu::Create`, given
//!   the line's topic; the game's pick of the topic's line it opens with is the line itself (a
//!   wrap of `TESTopic::GetMatchingInfo_ov2`, [`matching_info`]), not this game's own pick. While
//!   it opens, co-op's own refusals of a conversation (the NPC is another player's, a copy came
//!   to greet) are not applied ([`opening`]).
//! - **Showing a line.** A line is put to the window as the game puts the next line of a chain
//!   (`DialogMenu::LoadNextScreen`'s own code): a new `MenuTopic` of the line's quest, topic and
//!   info for the speaker, a shown topic list holding it, its first topic made current, then
//!   `LoadNextScreen`, which has the speaker say the response and shows its text.
//! - **Holding.** A wrap of `DialogMenu::DoIdle` keeps a window with nothing more to say on its
//!   last line (state 3, its countdown kept above zero), so the game never moves the
//!   conversation on by itself: no topic list, no chained line of this game's own choosing, no
//!   goodbye. The next line is put to it once the speaker has done saying the last one (or at
//!   once when lines are waiting, or a line has shown for [`LINE_MAX_MS`]).
//! - **No results.** No line's result runs in a watching game (`talk::run_result` asks
//!   [`is_watch_speaker`]): the line ran where it was said, and the host's game runs the story's
//!   part. A result the host's game runs for a guest (`talk::enact`) is not this window's.
//! - **Closing.** [`end_watch`] closes the window through the game's `DialogMenu::Close`; the
//!   session calls it when the conversation ends, when the player is no longer with the NPC, or
//!   when the conversation has gone quiet for too long. A window the player closed is noted
//!   ([`step`]): no other opens for that conversation, its lines are shown as text instead.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use engine_core::extend::{Owner, WrapSlot};
use engine_core::prelude::*;

use crate::{actors, addr, forms};

/// Co-op's place in front of `DialogMenu::DoIdle`.
pub(crate) const DO_IDLE: WrapSlot = WrapSlot::new(Owner::Coop, addr::DIALOG_MENU_DO_IDLE);
/// Co-op's place in front of `TESTopic::GetMatchingInfo_ov2`.
pub(crate) const MATCHING_INFO: WrapSlot =
    WrapSlot::new(Owner::Coop, addr::TOPIC_GET_MATCHING_INFO);

/// The countdown a held window is kept at (seconds; any value above zero holds it).
const HOLD_SECONDS: f32 = 1.0;
/// The longest a line is shown before the next waiting one replaces it, when the speaker never
/// says it is done (a line without a voice file, a speaker without a process for speech).
pub const LINE_MAX_MS: u32 = 20_000;
/// The most lines kept waiting (a talker far ahead): the oldest go first.
const QUEUE_MAX: usize = 16;

/// How [`watch_line`] went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The line is in the window (shown, or waiting for the one before to end).
    Window,
    /// This player closed the window on this conversation: the line is not shown in one.
    Left,
    /// The NPC is not in this game now, or the data have no such line.
    NotHere,
    /// Another window or menu is open, or the game did not open one.
    Busy,
}

#[derive(Debug, Default)]
struct State {
    /// The NPC watched (its reference id); 0 for none.
    npc: u32,
    /// `DialogMenu::Create` runs for the window now.
    opening: bool,
    /// A window was open for `npc` (so its closing is the player's, unless [`end_watch`]).
    open: bool,
    /// The player closed the window on `npc`'s conversation.
    left: bool,
    /// Lines (info form ids) waiting, oldest first.
    queue: VecDeque<u32>,
    /// The line shown now and since when (the game's milliseconds).
    shown: Option<(u32, u32)>,
}

/// The game-wide state of the watching window.
#[derive(Debug, Default)]
pub struct Watch {
    state: Mutex<State>,
    /// Lines shown in a window, ever.
    lines: AtomicU64,
    /// Results not run because they were a watched line's.
    results_skipped: AtomicU64,
    /// Windows the player closed.
    left: AtomicU64,
}

fn state<R>(e: &mut Engine, f: impl FnOnce(&mut State) -> R) -> R {
    let w = e.ext_free::<Watch>();
    let mut s = w.state.lock().unwrap_or_else(PoisonError::into_inner);
    f(&mut s)
}

/// Installs the wrap where the game has the function; called by [`crate::install`].
pub(crate) fn install(e: &mut Engine) {
    if e.is_registered(addr::DIALOG_MENU_DO_IDLE) {
        e.wrap(DO_IDLE, do_idle);
    }
    if e.is_registered(addr::TOPIC_GET_MATCHING_INFO) {
        e.wrap(MATCHING_INFO, matching_info);
    }
    let _ = e.ext_free::<Watch>();
}

/// `(lines shown, results skipped, windows the player closed)` since the game started.
pub fn counts(e: &mut Engine) -> (u64, u64, u64) {
    let w = e.ext_free::<Watch>();
    (
        w.lines.load(Ordering::SeqCst),
        w.results_skipped.load(Ordering::SeqCst),
        w.left.load(Ordering::SeqCst),
    )
}

/// The NPC whose conversation this game shows in a window (its reference id).
pub fn watching(e: &mut Engine) -> Option<u32> {
    state(e, |s| (s.npc != 0 && !s.left).then_some(s.npc))
}

/// Whether `DialogMenu::Create` runs for the watching window now (co-op's refusals do not apply).
pub(crate) fn opening(e: &mut Engine) -> bool {
    state(e, |s| s.opening)
}

/// Whether `speaker` (a pointer) is the watched NPC and the window is open on it (or opening):
/// a line's result on it is not run here.
pub(crate) fn is_watch_speaker(e: &mut Engine, speaker: u32) -> bool {
    let (npc, opening, left) = state(e, |s| (s.npc, s.opening, s.left));
    if npc == 0 || left || speaker == 0 || actors::actor(e, npc) != Some(speaker) {
        return false;
    }
    opening || open_speaker(e) == Some(speaker)
}

/// Notes a result not run because it was a watched line's.
pub(crate) fn note_skipped(e: &mut Engine) {
    e.ext_free::<Watch>()
        .results_skipped
        .fetch_add(1, Ordering::SeqCst);
}

/// The speaker of the open dialogue menu (a pointer).
fn open_speaker(e: &mut Engine) -> Option<u32> {
    let menu = e.mem.u32(addr::DIALOG_MENU_OPEN);
    if menu == 0 {
        return None;
    }
    Some(e.mem.u32(menu + addr::DIALOG_MENU_SPEAKER_OFFSET)).filter(|s| *s != 0)
}

fn log(text: impl FnOnce() -> String) {
    if std::env::var_os("NV_COOP_TALK_LOG").is_some() {
        eprintln!("co-op watch: {}", text());
    }
}

fn menu_state(e: &mut Engine) -> u32 {
    match e.mem.u32(addr::DIALOG_MENU_OPEN) {
        0 => u32::MAX,
        m => e.mem.u32(m + addr::DIALOG_MENU_STATE_OFFSET),
    }
}

fn game_ms(e: &mut Engine) -> u32 {
    e.mem.u32(addr::GAME_MS)
}

/// The game's functions the window needs: without one of them (a cut-down game, a test's
/// stand-ins) no window opens and the lines are shown as text.
const NEEDED: [u32; 13] = [
    addr::DIALOG_MENU_DO_IDLE,
    addr::DIALOG_MENU_CREATE,
    addr::DIALOG_MENU_CLOSE,
    addr::DIALOG_MENU_LOAD_NEXT_SCREEN,
    addr::INTERFACE_IS_IN_MENU_MODE,
    addr::TOPIC_GET_OWNER_TOPIC,
    addr::TOPIC_INFO_QUEST,
    addr::MENU_TOPIC_CONSTRUCT,
    addr::MENU_TOPIC_START,
    addr::MENU_TOPIC_CURRENT_RESPONSE,
    addr::TOPICS_CURRENT_TOPIC,
    addr::TOPICS_SET_LIST,
    addr::TOPICS_FIRST_TOPIC,
];

/// Shows line `info` (a form id) of the conversation with NPC `npc` (a reference id) in this
/// game's watching window, opening it when none is.
pub fn watch_line(e: &mut Engine, npc: u32, info: u32) -> Outcome {
    if !NEEDED.iter().all(|a| e.is_registered(*a))
        || !e.is_registered(addr::SIMPLE_LIST_CONSTRUCT)
        || !e.is_registered(addr::SIMPLE_LIST_ADD)
    {
        return Outcome::Busy;
    }
    if state(e, |s| s.left && s.npc == npc) {
        return Outcome::Left;
    }
    let (Some(actor), Some(_)) = (actors::actor(e, npc), info_form(e, info)) else {
        return Outcome::NotHere;
    };
    if let Some(speaker) = open_speaker(e) {
        return if speaker == actor && state(e, |s| s.npc == npc && s.open) {
            state(e, |s| {
                s.queue.push_back(info);
                while s.queue.len() > QUEUE_MAX {
                    s.queue.pop_front();
                }
            });
            Outcome::Window
        } else {
            Outcome::Busy
        };
    }
    if e.call(addr::INTERFACE_IS_IN_MENU_MODE, &args![]).bool() {
        return Outcome::Busy;
    }
    // A window on another NPC that is gone (closed by the game): forgotten.
    state(e, |s| {
        *s = State {
            npc,
            opening: true,
            queue: VecDeque::from([info]),
            ..State::default()
        }
    });
    let topic = info_topic(e, info);
    let made = e
        .call(addr::DIALOG_MENU_CREATE, &args![actor, topic, 0u32])
        .u32();
    let open = made != 0 && open_speaker(e) == Some(actor);
    state(e, |s| {
        s.opening = false;
        s.open = open;
        if !open {
            *s = State::default();
        }
    });
    log(|| format!("window on {npc:08x} for line {info:08x}: opened {open}"));
    if !open {
        return Outcome::Busy;
    }
    // The game greeted with the line itself: shown.
    if current_info(e) == info_form(e, info) {
        let now = game_ms(e);
        state(e, |s| {
            s.queue.pop_front();
            s.shown = Some((info, now));
        });
        e.ext_free::<Watch>().lines.fetch_add(1, Ordering::SeqCst);
    }
    Outcome::Window
}

/// Closes the watching window as `DoIdle` closes one (`Close`, then the fade-out: state 4, its
/// time from zero, which removes the menu once faded) and forgets the conversation; true when a
/// window was open. Its goodbye state is cleared first, so `Close` runs no line's result and adds
/// no topics here. (PC run 34: `Close` alone left the menu open and unseen, the player held in
/// dialogue, Sunny's copy waiting at the saloon door until the console's `CloseAllMenus`.)
pub fn end_watch(e: &mut Engine) -> bool {
    let npc = state(e, std::mem::take).npc;
    if npc == 0 {
        return false;
    }
    let actor = actors::actor(e, npc);
    let menu = e.mem.u32(addr::DIALOG_MENU_OPEN);
    if actor.is_none() || open_speaker(e) != actor {
        return false;
    }
    e.mem.set_u32(menu + addr::DIALOG_MENU_GOODBYE_OFFSET, 0);
    e.call(addr::DIALOG_MENU_CLOSE, &[]);
    e.mem.set_f32(menu + addr::DIALOG_MENU_ELAPSED_OFFSET, 0.0);
    e.mem.set_u32(menu + addr::DIALOG_MENU_STATE_OFFSET, 4);
    true
}

/// Once a frame: a window the player closed is noted, and the conversation's later lines are
/// not shown in one.
pub fn step(e: &mut Engine) {
    let (npc, open) = state(e, |s| (s.npc, s.open));
    if npc == 0 || !open {
        return;
    }
    let actor = actors::actor(e, npc);
    if actor.is_none() || open_speaker(e) != actor {
        let st = menu_state(e);
        log(|| {
            format!("window on {npc:08x} closed by the game or the player (menu state {st:#x})")
        });
        state(e, |s| {
            s.open = false;
            s.left = true;
            s.queue.clear();
            s.shown = None;
        });
        e.ext_free::<Watch>().left.fetch_add(1, Ordering::SeqCst);
    }
}

fn info_form(e: &mut Engine, id: u32) -> Option<u32> {
    let f = forms::form(e, id)?;
    (e.mem.u8(f + addr::FORM_TYPE_OFFSET) == addr::FORM_TYPE_INFO).then_some(f)
}

/// The topic holding info `id` (a pointer), 0 when none.
fn info_topic(e: &mut Engine, id: u32) -> u32 {
    match info_form(e, id) {
        Some(f) => e.call(addr::TOPIC_GET_OWNER_TOPIC, &args![f]).u32(),
        None => 0,
    }
}

/// The open menu's topic manager's current topic (a `MenuTopic`), 0 when none.
fn current_topic(e: &mut Engine) -> u32 {
    let menu = e.mem.u32(addr::DIALOG_MENU_OPEN);
    if menu == 0 {
        return 0;
    }
    let topics = e.mem.u32(menu + addr::DIALOG_MENU_TOPICS_OFFSET);
    if topics == 0 {
        return 0;
    }
    e.call(addr::TOPICS_CURRENT_TOPIC, &args![topics]).u32()
}

/// The info (a pointer) of the open menu's current topic.
fn current_info(e: &mut Engine) -> Option<u32> {
    match current_topic(e) {
        0 => None,
        t => Some(e.mem.u32(t + addr::MENU_TOPIC_INFO_OFFSET)).filter(|i| *i != 0),
    }
}

/// Whether the open menu's current topic has a response still to say.
fn more_to_say(e: &mut Engine) -> bool {
    match current_topic(e) {
        0 => false,
        t => e.call(addr::MENU_TOPIC_CURRENT_RESPONSE, &args![t]).u32() != 0,
    }
}

/// The wrap at `0061a760` (topic, said byte's address, speaker, target). While the window opens,
/// the game's pick of its first line for the line's topic is the line itself: the other player's
/// game said it, and this game's own pick may differ (its conditions read this game's state,
/// which the line's result in the other game has often moved on already). Without this the
/// window greeted with this game's own line first (PC run 35: Sunny's "Do you need anything?"
/// before each of her lines, on the watching player's screen).
pub(crate) fn matching_info(e: &mut Engine, a: &[u32]) -> Ret {
    let (topic, speaker) = (
        a.first().copied().unwrap_or(0),
        a.get(2).copied().unwrap_or(0),
    );
    let (npc, line) = state(e, |s| {
        (s.npc, s.queue.front().copied().filter(|_| s.opening))
    });
    if let Some(line) = line {
        if topic != 0 && speaker != 0 && actors::actor(e, npc) == Some(speaker) {
            if let Some(f) = info_form(e, line) {
                if e.call(addr::TOPIC_GET_OWNER_TOPIC, &args![f]).u32() == topic {
                    log(|| format!("window on {npc:08x} opens with line {line:08x}"));
                    return Ret {
                        eax: f,
                        ..Ret::default()
                    };
                }
            }
        }
    }
    e.call_next(addr::TOPIC_GET_MATCHING_INFO, a)
}

/// The wrap at `00762950`. Its word: the menu.
pub(crate) fn do_idle(e: &mut Engine, a: &[u32]) -> Ret {
    let menu = a.first().copied().unwrap_or(0);
    let (npc, open) = state(e, |s| (s.npc, s.open));
    let actor = if npc == 0 || !open {
        None
    } else {
        actors::actor(e, npc)
    };
    let ours = menu != 0
        && actor.is_some()
        && e.mem.u32(addr::DIALOG_MENU_OPEN) == menu
        && e.mem.u32(menu + addr::DIALOG_MENU_SPEAKER_OFFSET) == actor.unwrap_or(0);
    if !ours {
        return e.call_next(addr::DIALOG_MENU_DO_IDLE, a);
    }
    let speaker = actor.unwrap_or(0);
    let at = menu + addr::DIALOG_MENU_STATE_OFFSET;
    // Nothing more to say: the window stays on its line.
    if !more_to_say(e) && matches!(e.mem.u32(at), 1 | 3) {
        e.mem.set_u32(at, 3);
        e.mem.set_f32(addr::DIALOG_COUNTDOWN, HOLD_SECONDS);
    }
    let r = e.call_next(addr::DIALOG_MENU_DO_IDLE, a);
    if e.mem.u32(addr::DIALOG_MENU_OPEN) != menu || !matches!(e.mem.u32(at), 1..=3) {
        return r;
    }
    let now = game_ms(e);
    let next = state(e, |s| {
        let first = *s.queue.front()?;
        let overdue = s
            .shown
            .map_or(true, |(_, since)| now.wrapping_sub(since) >= LINE_MAX_MS);
        let waiting = s.queue.len() >= 2;
        Some((first, overdue || waiting))
    });
    let Some((info, hurry)) = next else {
        return r;
    };
    let done = !more_to_say(e) && e.mem.u8(speaker + addr::REF_LINE_DONE_OFFSET) != 0;
    if done || hurry {
        state(e, |s| {
            s.queue.pop_front();
        });
        let shown = show(e, menu, speaker, info);
        let st = menu_state(e);
        log(|| format!("line {info:08x} put to the window: {shown}, menu state {st}"));
        if shown {
            state(e, |s| s.shown = Some((info, now)));
            e.ext_free::<Watch>().lines.fetch_add(1, Ordering::SeqCst);
        }
    }
    r
}

/// Puts line `info` (a form id) to the open window on `speaker`, as `LoadNextScreen` puts the
/// next line of a chain; false when the data or the game's memory did not allow it.
fn show(e: &mut Engine, menu: u32, speaker: u32, info: u32) -> bool {
    let Some(f) = info_form(e, info) else {
        return false;
    };
    let topic = e.call(addr::TOPIC_GET_OWNER_TOPIC, &args![f]).u32();
    if topic == 0 {
        return false;
    }
    let quest = e.call(addr::TOPIC_INFO_QUEST, &args![topic, f]).u32();
    let block = e
        .call(addr::OPERATOR_NEW, &args![addr::MENU_TOPIC_SIZE])
        .u32();
    let list_block = e.call(addr::OPERATOR_NEW, &args![8u32]).u32();
    if block == 0 || list_block == 0 {
        return false;
    }
    let item = e
        .call(
            addr::MENU_TOPIC_CONSTRUCT,
            &args![block, quest, topic, f, speaker, 0u32],
        )
        .u32();
    e.call(addr::MENU_TOPIC_START, &args![item]);
    let list = e
        .call(addr::SIMPLE_LIST_CONSTRUCT, &args![list_block])
        .u32();
    e.with_stack(4, |e, slot| {
        e.mem.set_u32(slot.addr(), item);
        e.call(addr::SIMPLE_LIST_ADD, &args![list, slot.addr()]);
    });
    let topics = e.mem.u32(menu + addr::DIALOG_MENU_TOPICS_OFFSET);
    e.call(addr::TOPICS_SET_LIST, &args![topics, list]);
    // The list holds the line alone: its first node (the flag would skip it, as the game's
    // chain does over its greeting).
    e.call(addr::TOPICS_FIRST_TOPIC, &args![topics, 0u32]);
    e.mem
        .set_u8(menu + addr::DIALOG_MENU_RESPONSE_SHOWN_OFFSET, 0);
    e.mem.set_u32(addr::DIALOG_LAST_INFO, f);
    let next = e
        .call(addr::DIALOG_MENU_LOAD_NEXT_SCREEN, &args![menu, 0u32])
        .u32();
    e.mem.set_u32(menu + addr::DIALOG_MENU_STATE_OFFSET, next);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::talk;

    // A small game: an NPC (an actor), two lines of one topic, a dialogue menu and its topic
    // manager, and stand-ins for the game's functions that note what they were asked.
    const BASE: u32 = 0x0300_0000;
    const ACTOR: u32 = BASE;
    const NPC: u32 = 0x0010_4e85;
    const INFOS: [u32; 3] = [BASE + 0x1000, BASE + 0x1100, BASE + 0x1200];
    const IDS: [u32; 3] = [0x0010_a1e0, 0x0010_a1e1, 0x0010_a1e2];
    const TOPIC: u32 = BASE + 0x2000;
    const QUEST: u32 = BASE + 0x2100;
    const MENU: u32 = BASE + 0x3000;
    const TOPICS: u32 = BASE + 0x4000;
    // Notes: the next free block, Create's calls, results run, lines said (info pointers, in
    // order), the menu moving on by itself, the greeting Create says.
    const HEAP: u32 = BASE + 0x5000;
    const CREATES: u32 = BASE + 0x4f00;
    const RESULTS: u32 = BASE + 0x4f04;
    const MOVED_ON: u32 = BASE + 0x4f08;
    const GREETING: u32 = BASE + 0x4f0c;
    const SAID_COUNT: u32 = BASE + 0x4f10;
    const SAID: u32 = BASE + 0x4e00;
    const RESPONSES: u32 = 2;

    fn eax(v: u32) -> Ret {
        Ret {
            eax: v,
            ..Ret::default()
        }
    }

    fn said(e: &Engine) -> Vec<u32> {
        (0..e.mem.u32(SAID_COUNT))
            .map(|i| e.mem.u32(SAID + 4 * i))
            .collect()
    }

    fn menu_topic(e: &mut Engine, info: u32) -> u32 {
        let block = e.mem.u32(HEAP);
        e.mem.set_u32(HEAP, block + 0x40);
        e.mem.set_u32(block + addr::MENU_TOPIC_INFO_OFFSET, info);
        e.mem.set_u32(block + 0x20, RESPONSES);
        block
    }

    fn world() -> Engine {
        let mut e = Engine::new();
        e.mem.map(BASE, 0x1_0000);
        e.mem.map(0x011d_0000, 0x3_0000);
        e.mem.set_u32(HEAP, BASE + 0x8000);
        e.mem
            .set_u8(ACTOR + addr::FORM_TYPE_OFFSET, addr::FORM_TYPE_ACHR);
        e.mem.set_u32(ACTOR + addr::FORM_ID_OFFSET, NPC);
        for (f, id) in INFOS.into_iter().zip(IDS) {
            e.mem
                .set_u8(f + addr::FORM_TYPE_OFFSET, addr::FORM_TYPE_INFO);
            e.mem.set_u32(f + addr::FORM_ID_OFFSET, id);
        }
        e.register(addr::LOOKUP_FORM, |_, a| {
            eax(match a[0] {
                NPC => ACTOR,
                id => IDS.iter().position(|i| *i == id).map_or(0, |n| INFOS[n]),
            })
        });
        e.register(addr::INTERFACE_IS_IN_MENU_MODE, |_, _| eax(0));
        e.register(addr::TOPIC_GET_OWNER_TOPIC, |_, _| eax(TOPIC));
        e.register(addr::TOPIC_INFO_QUEST, |_, _| eax(QUEST));
        e.register(addr::OPERATOR_NEW, |e, _| {
            let block = e.mem.u32(HEAP);
            e.mem.set_u32(HEAP, block + 0x40);
            eax(block)
        });
        // The game's own pick of a topic's line: GREETING's (the stand-in's conditions), for
        // its one topic only.
        e.register(addr::TOPIC_GET_MATCHING_INFO, |e, a| {
            eax(if a[0] == TOPIC {
                e.mem.u32(GREETING)
            } else {
                0
            })
        });
        // The game's Create: the menu open on the speaker, fading in, its greeting (the
        // topic's matching line, `MenuTopicManager::Initialize`) the current topic, the
        // greeting's result run.
        e.register(addr::DIALOG_MENU_CREATE, |e, a| {
            e.mem.set_u32(CREATES, e.mem.u32(CREATES) + 1);
            e.mem.set_u32(CREATES + 0x20, a[1]);
            e.mem.set_u32(addr::DIALOG_MENU_OPEN, MENU);
            e.mem.set_u32(MENU + addr::DIALOG_MENU_SPEAKER_OFFSET, a[0]);
            e.mem
                .set_u32(MENU + addr::DIALOG_MENU_TOPICS_OFFSET, TOPICS);
            e.mem.set_u32(MENU + addr::DIALOG_MENU_STATE_OFFSET, 0);
            let greeting = e
                .call(
                    addr::TOPIC_GET_MATCHING_INFO,
                    &args![a[1], 0u32, a[0], 0u32],
                )
                .u32();
            let t = menu_topic(e, greeting);
            e.mem.set_u32(TOPICS, t);
            e.call(addr::INFO_RUN_RESULT, &args![greeting, 0u32, a[0]]);
            eax(MENU + 0x10)
        });
        // The game's Close: the speaker's dialogue stopped, the stored line's end result run with
        // a goodbye state 2 or 3; the menu stays open (DoIdle's fade-out, state 4, removes it).
        e.register(addr::DIALOG_MENU_CLOSE, |e, _| {
            let menu = e.mem.u32(addr::DIALOG_MENU_OPEN);
            if matches!(e.mem.u32(menu + addr::DIALOG_MENU_GOODBYE_OFFSET), 2 | 3) {
                let speaker = e.mem.u32(menu + addr::DIALOG_MENU_SPEAKER_OFFSET);
                let info = e.mem.u32(addr::DIALOG_LAST_INFO);
                e.call(addr::INFO_RUN_RESULT, &args![info, 1u32, speaker]);
            }
            Ret::default()
        });
        e.register(addr::INFO_RUN_RESULT, |e, _| {
            e.mem.set_u32(RESULTS, e.mem.u32(RESULTS) + 1);
            Ret::default()
        });
        e.register(addr::TOPICS_CURRENT_TOPIC, |e, a| eax(e.mem.u32(a[0])));
        e.register(addr::MENU_TOPIC_CONSTRUCT, |e, a| {
            e.mem.set_u32(a[0] + addr::MENU_TOPIC_INFO_OFFSET, a[3]);
            e.mem.set_u32(a[0] + 0x20, RESPONSES);
            eax(a[0])
        });
        e.register(addr::MENU_TOPIC_START, |_, _| Ret::default());
        e.register(addr::MENU_TOPIC_CURRENT_RESPONSE, |e, a| {
            eax(e.mem.u32(a[0] + 0x20))
        });
        e.register(addr::SIMPLE_LIST_CONSTRUCT, |e, a| {
            e.mem.set_u32(a[0], 0);
            eax(a[0])
        });
        e.register(addr::SIMPLE_LIST_ADD, |e, a| {
            e.mem.set_u32(a[0], e.mem.u32(a[1]));
            Ret::default()
        });
        e.register(addr::TOPICS_SET_LIST, |e, a| {
            e.mem.set_u32(a[0] + 4, a[1]);
            Ret::default()
        });
        // The game's FirstTopic: the shown list's first topic, its second with the flag (these
        // lists hold one).
        e.register(addr::TOPICS_FIRST_TOPIC, |e, a| {
            let first = if a[1] == 0 {
                e.mem.u32(e.mem.u32(a[0] + 4))
            } else {
                0
            };
            e.mem.set_u32(a[0], first);
            eax(u32::from(first != 0))
        });
        // The game's LoadNextScreen: the next response said (the speaker busy saying it), or,
        // with none left, the conversation moved on (the topic list).
        e.register(addr::DIALOG_MENU_LOAD_NEXT_SCREEN, |e, a| {
            let t = e.mem.u32(e.mem.u32(a[0] + addr::DIALOG_MENU_TOPICS_OFFSET));
            let left = if t == 0 { 0 } else { e.mem.u32(t + 0x20) };
            if left == 0 {
                e.mem.set_u32(MOVED_ON, e.mem.u32(MOVED_ON) + 1);
                return eax(2);
            }
            e.mem.set_u32(t + 0x20, left - 1);
            let n = e.mem.u32(SAID_COUNT);
            e.mem
                .set_u32(SAID + 4 * n, e.mem.u32(t + addr::MENU_TOPIC_INFO_OFFSET));
            e.mem.set_u32(SAID_COUNT, n + 1);
            let speaker = e.mem.u32(a[0] + addr::DIALOG_MENU_SPEAKER_OFFSET);
            e.mem.set_u8(speaker + addr::REF_LINE_DONE_OFFSET, 0);
            eax(3)
        });
        // The game's DoIdle: fading in, then a line counted down, said, the next screen.
        e.register(addr::DIALOG_MENU_DO_IDLE, |e, a| {
            let at = a[0] + addr::DIALOG_MENU_STATE_OFFSET;
            let speaker = e.mem.u32(a[0] + addr::DIALOG_MENU_SPEAKER_OFFSET);
            let done = e.mem.u8(speaker + addr::REF_LINE_DONE_OFFSET) != 0;
            let elapsed = a[0] + addr::DIALOG_MENU_ELAPSED_OFFSET;
            match e.mem.u32(at) {
                0 => {
                    // Faded in (its time grown to the fade's length).
                    e.mem.set_f32(elapsed, 0.5);
                    let s = e
                        .call(addr::DIALOG_MENU_LOAD_NEXT_SCREEN, &args![a[0], 0u32])
                        .u32();
                    e.mem.set_u32(at, s);
                }
                3 => {
                    let left = e.mem.f32(addr::DIALOG_COUNTDOWN) - 0.25;
                    e.mem.set_f32(addr::DIALOG_COUNTDOWN, left);
                    if left <= 0.0 && done {
                        e.mem.set_u32(at, 1);
                    }
                }
                1 if done => {
                    let s = e
                        .call(addr::DIALOG_MENU_LOAD_NEXT_SCREEN, &args![a[0], 0u32])
                        .u32();
                    e.mem.set_u32(at, s);
                }
                // Fading out for half a second (a quarter a frame), then gone.
                4 => {
                    let t = e.mem.f32(elapsed) + 0.25;
                    e.mem.set_f32(elapsed, t);
                    if t >= 0.5 {
                        e.mem.set_u32(addr::DIALOG_MENU_OPEN, 0);
                    }
                }
                _ => {}
            }
            Ret::default()
        });
        e.wrap(talk::CREATE, talk::create);
        e.wrap(talk::RUN_RESULT, talk::run_result);
        install(&mut e);
        talk::set_sharing(&mut e, true);
        // The NPC is the other player's: this game refuses its own conversations with it.
        talk::set_busy(&mut e, [NPC]);
        e
    }

    fn idle(e: &mut Engine, frames: u32) {
        for _ in 0..frames {
            e.call(addr::DIALOG_MENU_DO_IDLE, &args![MENU]);
            step(e);
        }
    }

    /// The speaker has done saying its line (its voice ended).
    fn line_done(e: &mut Engine) {
        e.mem.set_u8(ACTOR + addr::REF_LINE_DONE_OFFSET, 1);
    }

    #[test]
    fn a_watched_line_opens_the_window_and_is_said_there_with_no_result() {
        let mut e = world();
        // This game's own pick for the topic is another line (PC run 35: Sunny's "Do you need
        // anything?"): the window opens with the watched one all the same, and says it alone.
        e.mem.set_u32(GREETING, INFOS[2]);
        assert_eq!(watch_line(&mut e, NPC, IDS[0]), Outcome::Window);
        assert_eq!(e.mem.u32(CREATES), 1);
        assert_eq!(
            e.mem.u32(CREATES + 0x20),
            TOPIC,
            "opened on the line's topic"
        );
        assert_eq!(watching(&mut e), Some(NPC));
        assert_eq!(counts(&mut e).0, 1, "the line was the window's first");
        idle(&mut e, 1);
        assert_eq!(said(&e), vec![INFOS[0]]);
        // No result ran here: the line's was skipped.
        assert_eq!(e.mem.u32(RESULTS), 0);
        assert_eq!(counts(&mut e).1, 1);
        // Its second response once the first is said; then the window holds.
        line_done(&mut e);
        idle(&mut e, 10);
        assert_eq!(said(&e), vec![INFOS[0], INFOS[0]]);
        line_done(&mut e);
        idle(&mut e, 200);
        assert_eq!(
            e.mem.u32(MOVED_ON),
            0,
            "the game never moved the conversation on"
        );
        assert_eq!(e.mem.u32(MENU + addr::DIALOG_MENU_STATE_OFFSET), 3);
        assert_eq!(e.mem.u32(addr::DIALOG_MENU_OPEN), MENU);
    }

    #[test]
    fn a_greeting_that_is_the_line_is_not_said_twice() {
        let mut e = world();
        e.mem.set_u32(GREETING, INFOS[0]);
        assert_eq!(watch_line(&mut e, NPC, IDS[0]), Outcome::Window);
        idle(&mut e, 1);
        line_done(&mut e);
        idle(&mut e, 50);
        assert_eq!(said(&e), vec![INFOS[0], INFOS[0]]);
        assert_eq!(counts(&mut e).0, 1);
    }

    #[test]
    fn the_games_own_pick_stands_outside_the_window_opening() {
        let mut e = world();
        e.mem.set_u32(GREETING, INFOS[2]);
        let pick = |e: &mut Engine, topic: u32, speaker: u32| {
            e.call(
                addr::TOPIC_GET_MATCHING_INFO,
                &args![topic, 0u32, speaker, 0u32],
            )
            .u32()
        };
        // No window opening: the game's own pick, for this NPC too.
        assert_eq!(pick(&mut e, TOPIC, ACTOR), INFOS[2]);
        // A window opening on another NPC, or for another topic: the game's own pick there.
        state(&mut e, |s| {
            *s = State {
                npc: NPC,
                opening: true,
                queue: VecDeque::from([IDS[0]]),
                ..State::default()
            }
        });
        assert_eq!(pick(&mut e, TOPIC, BASE + 0x100), INFOS[2]);
        assert_eq!(pick(&mut e, TOPIC + 4, ACTOR), 0);
        assert_eq!(pick(&mut e, TOPIC, ACTOR), INFOS[0]);
        // The window open (no longer opening): the game's own pick again.
        state(&mut e, |s| s.opening = false);
        assert_eq!(pick(&mut e, TOPIC, ACTOR), INFOS[2]);
        // The player's own conversation, after the watched one ended: the game's own pick.
        assert_eq!(watch_line(&mut e, NPC, IDS[0]), Outcome::Window);
        end_watch(&mut e);
        assert_eq!(pick(&mut e, TOPIC, ACTOR), INFOS[2]);
    }

    #[test]
    fn the_next_line_waits_for_the_last_to_be_said_unless_lines_pile_up() {
        let mut e = world();
        e.mem.set_u32(GREETING, INFOS[0]);
        watch_line(&mut e, NPC, IDS[0]);
        idle(&mut e, 1);
        assert_eq!(watch_line(&mut e, NPC, IDS[1]), Outcome::Window);
        // The first line's first response is still being said.
        idle(&mut e, 20);
        assert_eq!(said(&e), vec![INFOS[0]]);
        line_done(&mut e);
        idle(&mut e, 10);
        assert_eq!(said(&e), vec![INFOS[0], INFOS[0]]);
        line_done(&mut e);
        idle(&mut e, 1);
        assert_eq!(said(&e), vec![INFOS[0], INFOS[0], INFOS[1]]);
        // Two waiting: the first goes at once, before the speaker is done.
        watch_line(&mut e, NPC, IDS[2]);
        watch_line(&mut e, NPC, IDS[0]);
        idle(&mut e, 1);
        assert_eq!(said(&e).last(), Some(&INFOS[2]));
        assert_eq!(watch_line(&mut e, NPC, IDS[0]), Outcome::Window);
        assert_eq!(e.mem.u32(CREATES), 1);
    }

    #[test]
    fn a_line_that_never_ends_gives_way_after_a_while() {
        let mut e = world();
        e.mem.set_u32(GREETING, INFOS[0]);
        watch_line(&mut e, NPC, IDS[0]);
        idle(&mut e, 1);
        watch_line(&mut e, NPC, IDS[1]);
        e.mem.set_u32(addr::GAME_MS, LINE_MAX_MS - 1);
        idle(&mut e, 5);
        assert_eq!(said(&e), vec![INFOS[0]]);
        e.mem.set_u32(addr::GAME_MS, LINE_MAX_MS);
        idle(&mut e, 1);
        assert_eq!(said(&e), vec![INFOS[0], INFOS[1]]);
    }

    #[test]
    fn the_window_closes_when_asked_and_a_player_who_closed_it_is_left_alone() {
        let mut e = world();
        e.mem.set_u32(GREETING, INFOS[0]);
        watch_line(&mut e, NPC, IDS[0]);
        idle(&mut e, 1);
        // A goodbye line: the game's Close would run its end result.
        e.mem.set_u32(MENU + addr::DIALOG_MENU_GOODBYE_OFFSET, 2);
        assert!(end_watch(&mut e));
        assert_eq!(e.mem.u32(RESULTS), 0, "no result run by the close");
        // Fading out from the start, then gone.
        assert_eq!(e.mem.u32(MENU + addr::DIALOG_MENU_STATE_OFFSET), 4);
        idle(&mut e, 1);
        assert_eq!(e.mem.u32(addr::DIALOG_MENU_OPEN), MENU, "still fading");
        idle(&mut e, 1);
        assert_eq!(e.mem.u32(addr::DIALOG_MENU_OPEN), 0);
        assert_eq!(watching(&mut e), None);
        assert!(!end_watch(&mut e));
        idle(&mut e, 1);
        assert_eq!(counts(&mut e).2, 0, "closed by co-op, not left");
        // Opened again, then closed by the player (the game's Close and fade).
        assert_eq!(watch_line(&mut e, NPC, IDS[0]), Outcome::Window);
        e.call(addr::DIALOG_MENU_CLOSE, &[]);
        e.mem.set_f32(MENU + addr::DIALOG_MENU_ELAPSED_OFFSET, 0.0);
        e.mem.set_u32(MENU + addr::DIALOG_MENU_STATE_OFFSET, 4);
        idle(&mut e, 2);
        assert_eq!(counts(&mut e).2, 1);
        assert_eq!(watch_line(&mut e, NPC, IDS[1]), Outcome::Left);
        assert_eq!(e.mem.u32(CREATES), 2);
        // The conversation over, the next one is shown again.
        end_watch(&mut e);
        assert_eq!(watch_line(&mut e, NPC, IDS[1]), Outcome::Window);
    }

    #[test]
    fn results_on_the_watched_npc_are_skipped_only_while_the_window_is_open() {
        let mut e = world();
        e.mem.set_u32(GREETING, INFOS[0]);
        watch_line(&mut e, NPC, IDS[0]);
        e.call(addr::INFO_RUN_RESULT, &args![INFOS[1], 1u32, ACTOR]);
        assert_eq!(e.mem.u32(RESULTS), 0);
        // One the host's game runs for a guest is not the window's.
        talk::enact(&mut e, NPC, IDS[1], true).unwrap();
        assert_eq!(e.mem.u32(RESULTS), 1);
        end_watch(&mut e);
        e.call(addr::INFO_RUN_RESULT, &args![INFOS[1], 1u32, ACTOR]);
        assert_eq!(e.mem.u32(RESULTS), 2);
    }

    #[test]
    fn no_window_over_another_menu_or_for_what_this_game_lacks() {
        // A game without one of the functions the window needs: none.
        let mut bare = Engine::new();
        assert_eq!(watch_line(&mut bare, NPC, IDS[0]), Outcome::Busy);
        let mut e = world();
        assert_eq!(watch_line(&mut e, 0x0099_0001, IDS[0]), Outcome::NotHere);
        assert_eq!(watch_line(&mut e, NPC, 0x0099_0002), Outcome::NotHere);
        e.register(addr::INTERFACE_IS_IN_MENU_MODE, |_, _| eax(1));
        assert_eq!(watch_line(&mut e, NPC, IDS[0]), Outcome::Busy);
        e.register(addr::INTERFACE_IS_IN_MENU_MODE, |_, _| eax(0));
        // The player's own conversation is open (with someone else, or not this window).
        e.mem.set_u32(addr::DIALOG_MENU_OPEN, MENU);
        e.mem
            .set_u32(MENU + addr::DIALOG_MENU_SPEAKER_OFFSET, ACTOR);
        assert_eq!(watch_line(&mut e, NPC, IDS[0]), Outcome::Busy);
        assert_eq!(e.mem.u32(CREATES), 0);
        // A game that opens no menu.
        e.mem.set_u32(addr::DIALOG_MENU_OPEN, 0);
        e.register(addr::DIALOG_MENU_CREATE, |_, _| eax(0));
        e.wrap(talk::CREATE, talk::create);
        assert_eq!(watch_line(&mut e, NPC, IDS[0]), Outcome::Busy);
        assert_eq!(watching(&mut e), None);
    }
}
