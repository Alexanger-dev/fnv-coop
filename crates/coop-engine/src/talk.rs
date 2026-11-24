//! Conversations (docs/coop/SPECS.md S16): what the player's own conversation says and does is
//! seen where the game does it, and its results run once.
//!
//! Every result of a line of dialogue goes through `TESTopicInfo::RunResult` (`0061f170`: the
//! info, index 0 when the line is said and 1 when it ends, the speaker). Co-op wraps it as
//! [`Owner::Coop`]. While a session shares conversations ([`set_sharing`]) and the call belongs
//! to the player's own conversation (the dialogue menu is open on that speaker):
//!
//! - every call is noted as an [`Event::Said`], so the session can tell the other players;
//! - on a guest, a line of the shared story (its quest is not one of the player's own) keeps its
//!   place in the guest's menu, but its result script does not run there: the call goes on inside
//!   the apply scope, and the host's game runs the result instead ([`enact`]). A line of one of
//!   the player's own quests (the opening), or one whose quest cannot be told, runs on the guest
//!   as it always did.
//!
//! `DialogMenu::Create` (`00761a20`) is wrapped too: an NPC another player is talking to
//! ([`set_busy`]) gets no menu (the answer 0, the game's own for a conversation that could not
//! start), and the refusal is noted as an [`Event::Blocked`]. Nor does a copy of an actor another
//! game runs (`puppet`) that comes to this game's player on a Dialogue package of its own (a
//! force greet): the conversation is the owner's game's, with the player it runs the actor for
//! (PC run 31: the guest ended Sunny's tutorial, while in the host's game her copy came to the
//! host, and her first line there started the campfire quest for both). A player who walks up to
//! a copy and talks to it is never refused. The refusal is noted as an [`Event::NotGreeted`].
//!
//! With sharing off, both wraps pass every call on untouched.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError, RwLock};

use engine_core::extend::{Owner, SiteSlot, WrapSlot};
use engine_core::prelude::*;

use crate::{actors, addr, forms, gate};

/// Co-op's place in front of `TESTopicInfo::RunResult`.
pub(crate) const RUN_RESULT: WrapSlot = WrapSlot::new(Owner::Coop, addr::INFO_RUN_RESULT);
/// Co-op's place in front of `DialogMenu::Create`.
pub(crate) const CREATE: WrapSlot = WrapSlot::new(Owner::Coop, addr::DIALOG_MENU_CREATE);
/// Co-op's place at the start of `TESObjectREFR::Activate`.
pub(crate) const ACTIVATE: SiteSlot = SiteSlot::new(Owner::Coop, addr::REFR_ACTIVATE_SITE);

/// The most events kept between two takes (a conversation makes a few per line).
const EVENTS_MAX: usize = 256;
/// How long a message stays on the HUD, in seconds (the game's own messages use 2.0).
const MESSAGE_SECONDS: f32 = 4.0;

/// What happened in this game's own conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The game ran a line's result: the speaker's reference id, the info's form id, whether it
    /// was the line's end (index 1), and whether the result was held back here for the host to
    /// run (a guest, a line of the shared story).
    Said {
        /// The speaker's reference id.
        npc: u32,
        /// The info's form id.
        info: u32,
        /// False when the line is said, true when it ends.
        end: bool,
        /// The result script did not run here: the host is to run it.
        held: bool,
        /// Said by a click on one of the window's rows (a player chose it), not begun by the NPC
        /// itself ([`crate::choices::clicking`]).
        chosen: bool,
    },
    /// A conversation a copy of an actor another game runs began itself with this game's player
    /// was refused: the owner's game greets its own player.
    NotGreeted {
        /// The speaker's reference id.
        npc: u32,
    },
    /// A conversation with this reference was refused: another player is talking to it.
    Blocked {
        /// The speaker's reference id.
        npc: u32,
    },
    /// This game's player talked to (activated) an actor another player is talking to: it joins
    /// that conversation (S42, D12). An actor's own greeting is not this: the game queues its
    /// menu without an activation, and it is only [`Event::Blocked`].
    Asked {
        /// The actor's reference id.
        npc: u32,
    },
    /// This game's player talked to (activated) an actor where another player is busy (in a
    /// menu, fighting): the conversation waits for that player (D13); its menu is refused.
    Held {
        /// The actor's reference id.
        npc: u32,
    },
    /// An actor came to this game's player to talk (a force greet) and the conversation began:
    /// it greeted with this package. Told to the other games ([`set_greeted`]).
    Greeted {
        /// The actor's reference id.
        npc: u32,
        /// The Dialogue package it runs.
        package: u32,
    },
    /// An actor came to this game's player to talk with a package it had already greeted a
    /// player with ([`set_greeted`]): refused, the greeting was given (PC run, 2026-11-23:
    /// Ringo, who had thanked the host, ran to the guest and thanked her again).
    GreetGiven {
        /// The actor's reference id.
        npc: u32,
    },
}

/// The game-wide state (a lock-free shared slot, as the journal's).
#[derive(Debug, Default)]
pub struct Talk {
    sharing: AtomicBool,
    /// How deep the host is in running a guest's result: those calls are not this game's own
    /// conversation.
    enacting: AtomicU32,
    busy: RwLock<BTreeSet<u32>>,
    /// The cells where another player is busy (D13): this player talking to an actor in one
    /// waits for it ([`set_hold`]).
    hold: RwLock<BTreeSet<u32>>,
    /// The actor this player talked to that waits (its menu is refused once): its reference id.
    held_npc: AtomicU32,
    /// This player's own opening runs: its conversations are its own (see [`set_own_opening`]).
    opening: AtomicBool,
    events: Mutex<Vec<Event>>,
    held: AtomicU64,
    own: AtomicU64,
    unknown: AtomicU64,
    /// The quests of the lines whose results are running now, innermost last (a line's result
    /// script names no owner quest: its info's quest is the one).
    results: Mutex<Vec<u32>>,
    /// The greetings another player's game gave (an actor's reference id to the Dialogue package
    /// it greeted with: [`set_greeted`]).
    greeted: RwLock<BTreeMap<u32, u32>>,
}

/// Actor `npc` greeted another player with Dialogue package `package` in that player's game:
/// its coming to greet this game's player with the same package is refused (council 3-0, A).
/// This game's own greetings are its own game's to repeat or not, as in a game alone.
pub fn set_greeted(e: &mut Engine, npc: u32, package: u32) {
    e.ext_free::<Talk>()
        .greeted
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(npc, package);
}

/// The Dialogue package `speaker` (a pointer) runs: it came to the player to talk (a force
/// greet), rather than the player walking up to it. 0 when it runs none.
fn greeting_package(e: &mut Engine, speaker: u32) -> u32 {
    if !e.is_registered(addr::ACTOR_GET_CURRENT_EDITOR_PACKAGE)
        || !e.is_registered(addr::PACKAGE_TYPE)
    {
        return 0;
    }
    let package = e
        .call(addr::ACTOR_GET_CURRENT_EDITOR_PACKAGE, &args![speaker])
        .u32();
    if package != 0
        && e.call(addr::PACKAGE_TYPE, &args![package]).u32() & 0xff == addr::PACKAGE_TYPE_DIALOGUE
    {
        e.mem.u32(package.wrapping_add(addr::FORM_ID_OFFSET))
    } else {
        0
    }
}

impl Talk {
    fn push(&self, ev: Event) {
        let mut q = self.events.lock().unwrap_or_else(PoisonError::into_inner);
        if q.len() < EVENTS_MAX {
            q.push(ev);
        }
    }
}

/// Starts or stops sharing conversations: with it off the wraps change nothing.
pub fn set_sharing(e: &mut Engine, on: bool) {
    e.ext_free::<Talk>().sharing.store(on, Ordering::SeqCst);
}

/// Whether conversations are shared.
pub fn sharing(e: &mut Engine) -> bool {
    e.ext_free::<Talk>().sharing.load(Ordering::SeqCst)
}

/// Names the references other players are talking to: a conversation with one of them does not
/// start here.
/// Says whether this player's own opening runs in this game (Doc Mitchell's house, a quest each
/// player plays for itself). Its conversations are then its own, whoever it talks to: on a guest
/// a line of the shared story is run here, not held for the host, and no NPC is refused as
/// another player's (the owner, 2026-11-14: both players sat for Doc Mitchell's questions; he
/// asked the host's and stared at the guest until the host was done). Each game has its own Doc
/// Mitchell for it.
pub fn set_own_opening(e: &mut Engine, on: bool) {
    e.ext_free::<Talk>().opening.store(on, Ordering::SeqCst);
}

pub fn set_busy(e: &mut Engine, ids: impl IntoIterator<Item = u32>) {
    let t = e.ext_free::<Talk>();
    *t.busy.write().unwrap_or_else(PoisonError::into_inner) = ids.into_iter().collect();
}

/// The events since the last call, in order.
/// Whether this game is running a guest's line ([`enact`]) now.
pub fn enacting(e: &mut Engine) -> bool {
    e.ext_free::<Talk>().enacting.load(Ordering::SeqCst) > 0
}

pub fn take_events(e: &mut Engine) -> Vec<Event> {
    let t = e.ext_free::<Talk>();
    std::mem::take(&mut *t.events.lock().unwrap_or_else(PoisonError::into_inner))
}

/// `(held for the host, run here as the player's own, run here with no quest known)`: how the
/// results of this game's own conversations were classed since the game started.
pub fn counts(e: &mut Engine) -> (u64, u64, u64) {
    let t = e.ext_free::<Talk>();
    (
        t.held.load(Ordering::SeqCst),
        t.own.load(Ordering::SeqCst),
        t.unknown.load(Ordering::SeqCst),
    )
}

/// The reference id of the speaker of the open dialogue menu; `None` with no menu open.
/// Which response of its line the open dialogue menu's NPC began last (the owner, 2026-11-22:
/// a watching player's subtitles fell a response behind the voice). The menu says a line's
/// responses one after another (`DialogMenu::LoadNextScreen`: the current topic's current
/// response said, the topic moved on to the next, `0083e110`); each move of the current topic's
/// current response (`MenuTopic` `0083e170`, 0 once every response was said) is the next
/// response begun. Looked at once a frame; only once the menu has said a response of its current
/// topic (`bResponseShown`): between a topic made current and its first response (the greeting
/// while the menu fades in) nothing is begun yet.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Responses {
    /// The current topic, its info and its current response when last looked at.
    last: Option<(u32, u32, u32)>,
    /// The response of that line begun last.
    index: u32,
}

impl Responses {
    /// The line the menu says now (its info's form id) and the response of it begun last (0 for
    /// the first); `None` with no menu open, before its first response, or with no topic.
    pub fn observe(&mut self, e: &mut Engine) -> Option<(u32, u32)> {
        let menu = e.mem.u32(addr::DIALOG_MENU_OPEN);
        if menu == 0 {
            self.last = None;
            return None;
        }
        if e.mem.u8(menu + addr::DIALOG_MENU_RESPONSE_SHOWN_OFFSET) == 0 {
            return None;
        }
        let topic = crate::watch::current_topic(e);
        if topic == 0 {
            return None;
        }
        let info = e.mem.u32(topic + addr::MENU_TOPIC_INFO_OFFSET);
        if info == 0 {
            return None;
        }
        let response = e
            .call(addr::MENU_TOPIC_CURRENT_RESPONSE, &args![topic])
            .u32();
        self.index = match self.last {
            Some((t, i, r)) if t == topic && i == info => {
                if r != 0 && response != r {
                    // The line moved on to its next response.
                    self.index + 1
                } else if r == 0 && response != 0 {
                    // Done, then responses again: the same line said again.
                    0
                } else {
                    self.index
                }
            }
            // Another line: its first response.
            _ => 0,
        };
        self.last = Some((topic, info, response));
        Some((e.mem.u32(info + addr::FORM_ID_OFFSET), self.index))
    }
}

pub fn menu_npc(e: &mut Engine) -> Option<u32> {
    let speaker = menu_speaker(e)?;
    Some(e.mem.u32(speaker.wrapping_add(addr::FORM_ID_OFFSET)))
}

fn menu_speaker(e: &mut Engine) -> Option<u32> {
    let menu = e.mem.u32(addr::DIALOG_MENU_OPEN);
    if menu == 0 {
        return None;
    }
    Some(
        e.mem
            .u32(menu.wrapping_add(addr::DIALOG_MENU_SPEAKER_OFFSET)),
    )
    .filter(|s| *s != 0)
}

/// Ends the open conversation through the game's own `DialogMenu::Close`; false with none open.
pub fn close_menu(e: &mut Engine) -> bool {
    if menu_speaker(e).is_none() {
        return false;
    }
    e.call(addr::DIALOG_MENU_CLOSE, &[]);
    true
}

/// The info `id`, or `None` when the data lack it or the form is not an info.
fn info(e: &mut Engine, id: u32) -> Option<u32> {
    let f = forms::form(e, id)?;
    (e.mem.u8(f + addr::FORM_TYPE_OFFSET) == addr::FORM_TYPE_INFO).then_some(f)
}

/// The form id of the quest the info at `info` belongs to: the quest its topic holds it under,
/// else the info's own owner quest; 0 when neither is known.
fn quest_of(e: &mut Engine, info: u32) -> u32 {
    let topic = e.call(addr::TOPIC_GET_OWNER_TOPIC, &args![info]).u32();
    let mut quest = match topic {
        0 => 0,
        t => e.call(addr::TOPIC_INFO_QUEST, &args![t, info]).u32(),
    };
    if quest == 0 {
        quest = e.mem.u32(info.wrapping_add(addr::INFO_OWNER_QUEST_OFFSET));
    }
    match quest {
        0 => 0,
        q => e.mem.u32(q.wrapping_add(addr::FORM_ID_OFFSET)),
    }
}

/// The form id of the quest info `id` belongs to (`Some(0)`: an info with no quest known; `None`:
/// not an info of this game's data).
pub fn info_quest(e: &mut Engine, id: u32) -> Option<u32> {
    let f = info(e, id)?;
    Some(quest_of(e, f))
}

/// Why the host's game did not run a guest's result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The data have no such info.
    NoInfo(u32),
    /// The speaker is not in this game now (its cell is not loaded), or is not an actor.
    NoSpeaker(u32),
}

/// Runs info `info`'s result on the reference `npc` in this game (the host's), through the game's
/// own `TESTopicInfo::RunResult`: `end` false for the result of the line being said, true for its
/// end. The call is not taken for this game's own conversation.
pub fn enact(e: &mut Engine, npc: u32, info_id: u32, end: bool) -> Result<(), Refusal> {
    let f = info(e, info_id).ok_or(Refusal::NoInfo(info_id))?;
    let speaker = actors::actor(e, npc).ok_or(Refusal::NoSpeaker(npc))?;
    e.ext_free::<Talk>().enacting.fetch_add(1, Ordering::SeqCst);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        e.call(addr::INFO_RUN_RESULT, &args![f, u32::from(end), speaker]);
    }));
    e.ext_free::<Talk>().enacting.fetch_sub(1, Ordering::SeqCst);
    match result {
        Ok(()) => Ok(()),
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// Marks info `id` as said in this game, as the game does when the line is said in it (the
/// owner, 2026-11-16: Sunny's "say once" line after the bottles, said to the host, was said
/// again to the guest at the well instead of her line about the geckos, because the guest's
/// game had never said it). False when this game has no such info.
pub fn mark_said(e: &mut Engine, id: u32) -> bool {
    let Some(f) = info(e, id) else {
        return false;
    };
    e.call(addr::INFO_MARK_SAID, &args![f]);
    true
}

/// Whether info `id` is marked as said in this game.
pub fn said(e: &mut Engine, id: u32) -> bool {
    info(e, id).is_some_and(|f| e.mem.u8(f + addr::INFO_SAID_OFFSET) != 0)
}

/// The text of info `id`'s responses as the game builds it (`" | "` between two responses), and
/// its prompt (what the player chose) when the info has one, both without the directions to the
/// voice actor ([`without_directions`]). `None`: not an info.
pub fn line(e: &mut Engine, id: u32) -> Option<(String, String)> {
    let f = info(e, id)?;
    let string = e.call(addr::OPERATOR_NEW, &args![addr::STRING_SIZE]).u32();
    if string == 0 {
        return None;
    }
    e.call(addr::STRING_CONSTRUCT, &args![string]);
    e.call(addr::INFO_GET_DISPLAY_TEXT, &args![f, string, 0u32]);
    let text = text_at(e, e.mem.u32(string));
    e.call(addr::STRING_DESTROY, &args![string]);
    e.call(addr::OPERATOR_DELETE, &args![string]);
    let prompt = text_at(e, e.mem.u32(f + addr::INFO_PROMPT_OFFSET));
    Some((without_directions(&text), without_directions(&prompt)))
}

/// `text` without its directions to the voice actor, the parts in braces ("{not in charge of
/// Goodsprings}What do you want now?"), which the game leaves out of its own subtitles (the
/// owner, 2026-11-22: shown in co-op's, they clutter the screen); the spaces they leave, one.
/// An unclosed brace leaves the rest as it is.
pub fn without_directions(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        let Some(close) = rest[open..].find('}') else {
            break;
        };
        out.push_str(&rest[..open]);
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    out.split(' ')
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn text_at(e: &Engine, chars: u32) -> String {
    if chars == 0 {
        return String::new();
    }
    // The game's text is one byte per character (Windows-1252): each byte is that code point,
    // near enough for a message.
    e.mem.cstr(chars).into_iter().map(char::from).collect()
}

/// Shows `text` as one of the game's own HUD messages (cut to what a message holds). False when
/// the game gave no memory for it.
pub fn show(e: &mut Engine, text: &str) -> bool {
    let mut bytes: Vec<u8> = text
        .chars()
        .map(|c| if (c as u32) < 0x100 { c as u8 } else { b'?' })
        .filter(|b| *b != 0)
        .collect();
    bytes.truncate(addr::HUD_MESSAGE_MAX);
    let buffer = e
        .call(addr::OPERATOR_NEW, &args![bytes.len() as u32 + 1])
        .u32();
    if buffer == 0 {
        return false;
    }
    e.mem.set_cstr(buffer, &bytes);
    e.call(
        addr::SHOW_HUD_MESSAGE,
        &args![buffer, 0u32, 0u32, 0u32, MESSAGE_SECONDS, 0u32],
    );
    e.call(addr::OPERATOR_DELETE, &args![buffer]);
    true
}

/// Shows `text` as one of the game's own subtitles, at the foot of the screen as an NPC's speech
/// (`HUDMainMenu::ShowSubtitle`, with no sound: its time comes from its length), placed at the
/// player so the HUD's distance test always passes. Text longer than a subtitle holds is cut at
/// a space (the caller splits it, [`subtitle_parts`]). False when the game shows no subtitles (the
/// setting is off, or there is no HUD or player): the caller shows it another way.
pub fn subtitle(e: &mut Engine, text: &str) -> bool {
    let hud = e.mem.u32(addr::HUD_SINGLETON);
    let Some(player) = crate::live::player(e) else {
        return false;
    };
    if hud == 0 || !e.is_registered(addr::HUD_SHOW_SUBTITLE) {
        return false;
    }
    let bytes = subtitle_bytes(text);
    let at = [0, 4, 8].map(|o| e.mem.u32(player.wrapping_add(addr::REF_POS_OFFSET + o)));
    let len = (bytes.len() as u32 + 4) & !3;
    e.with_stack(len, |e, buffer| {
        e.mem.set_cstr(buffer.addr(), &bytes);
        e.call(
            addr::HUD_SHOW_SUBTITLE,
            &args![
                hud,
                buffer.addr(),
                0u32,
                0u32,
                0u32,
                at[0],
                at[1],
                at[2],
                0u32,
                0u32
            ],
        )
        .bool()
    })
}

/// `text` in parts a subtitle holds ([`addr::SUBTITLE_MAX`] characters, one byte each in the
/// game), each cut at a space where it can be.
pub fn subtitle_parts(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest: Vec<char> = text.trim().chars().collect();
    while rest.len() > addr::SUBTITLE_MAX {
        let cut = rest[..=addr::SUBTITLE_MAX]
            .iter()
            .rposition(|c| *c == ' ')
            .unwrap_or(addr::SUBTITLE_MAX);
        out.push(
            rest[..cut]
                .iter()
                .collect::<String>()
                .trim_end()
                .to_string(),
        );
        rest = rest[cut..]
            .iter()
            .collect::<String>()
            .trim_start()
            .chars()
            .collect();
    }
    if !rest.is_empty() {
        out.push(rest.into_iter().collect());
    }
    out
}

/// Ends at once the HUD's subtitles whose text is one of `texts` (co-op's, still showing or
/// waiting their time): their time set to nothing (`+0x108`, seconds; the HUD drops a subtitle
/// whose time has run out at its next update, `+0x104` its start). How many.
pub fn end_subtitles(e: &mut Engine, texts: &[String]) -> u32 {
    let hud = e.mem.u32(addr::HUD_SINGLETON);
    if hud == 0 {
        return 0;
    }
    let list = hud + addr::HUD_SUBTITLES_OFFSET;
    let (data, count) = (e.mem.u32(list + 4), e.mem.u32(list + 8));
    let wanted: Vec<Vec<u8>> = texts.iter().map(|t| subtitle_bytes(t)).collect();
    let mut ended = 0;
    for i in 0..count.min(64) {
        let record = e.mem.u32(data + 4 * i);
        let text = e.mem.cstr(record);
        if wanted.contains(&text) {
            e.mem.set_f32(record + addr::SUBTITLE_TIME_OFFSET, 0.0);
            ended += 1;
        }
    }
    ended
}

/// `text` as a subtitle's bytes: one a character (others `?`), no NUL, at most
/// [`addr::SUBTITLE_MAX`].
fn subtitle_bytes(text: &str) -> Vec<u8> {
    let mut bytes: Vec<u8> = text
        .chars()
        .map(|c| if (c as u32) < 0x100 { c as u8 } else { b'?' })
        .filter(|b| *b != 0)
        .collect();
    bytes.truncate(addr::SUBTITLE_MAX);
    bytes
}

/// Whether the HUD has a subtitle queued (one of co-op's, or an NPC's).
pub fn subtitle_showing(e: &mut Engine) -> bool {
    let hud = e.mem.u32(addr::HUD_SINGLETON);
    hud != 0 && e.mem.u32(hud + addr::HUD_SUBTITLES_OFFSET + 8) != 0
}

/// Whether this game's player is busy: a menu is open (the Pip-Boy, a trade, a conversation of
/// its own) or the player is in a fight. Another player's conversation is then not shown line by
/// line (S42, the owner's choice of 2026-11-21).
pub fn busy(e: &mut Engine) -> bool {
    in_menu(e) || in_combat(e)
}

/// Whether a menu is up (the dialogue window included).
pub fn in_menu(e: &mut Engine) -> bool {
    e.is_registered(addr::INTERFACE_IS_IN_MENU_MODE)
        && e.call(addr::INTERFACE_IS_IN_MENU_MODE, &args![]).bool()
}

/// Whether this game's player is in combat.
pub fn in_combat(e: &mut Engine) -> bool {
    let Some(player) = crate::live::player(e) else {
        return false;
    };
    e.is_registered(addr::ACTOR_IN_COMBAT) && e.call(addr::ACTOR_IN_COMBAT, &args![player]).bool()
}

/// The name of reference `id`'s base form (an NPC's name), `None` when it has none here.
pub fn name_of(e: &mut Engine, id: u32) -> Option<String> {
    let r = forms::form(e, id)?;
    let base = e.mem.u32(r.wrapping_add(addr::REF_BASE_FORM_OFFSET));
    if base == 0 || !e.is_registered(addr::FULL_NAME_TEXT) {
        return None;
    }
    let text = e.call(addr::FULL_NAME_TEXT, &args![base]).u32();
    Some(text_at(e, text)).filter(|n| !n.trim().is_empty())
}

/// Whether reference `npc` is in this game now and in the cell the player is in.
pub fn near(e: &mut Engine, npc: u32) -> bool {
    let (Some(player), Some(r)) = (crate::live::player(e), forms::form(e, npc)) else {
        return false;
    };
    let cell = e.mem.u32(player + addr::REF_CELL_OFFSET);
    cell != 0 && e.mem.u32(r + addr::REF_CELL_OFFSET) == cell
}

/// The wrap at `0061f170`. Its words: the info, the index, the reference.
pub(crate) fn run_result(e: &mut Engine, a: &[u32]) -> Ret {
    let info = a.first().copied().unwrap_or(0);
    let quest = if info == 0 { 0 } else { quest_of(e, info) };
    let depth = {
        let mut r = e
            .ext_free::<Talk>()
            .results
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        r.push(quest);
        r.len() - 1
    };
    // Unwound or not, the stack goes back to where it was.
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_result_shared(e, a)));
    e.ext_free::<Talk>()
        .results
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .truncate(depth);
    match out {
        Ok(r) => r,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// Whether a line's result is running now on `reference` (a pointer) in this player's own
/// conversation: a result runs and `reference` is the open dialogue menu's speaker.
pub(crate) fn own_result_on(e: &mut Engine, reference: u32) -> bool {
    result_quest(e).is_some() && menu_speaker(e) == Some(reference)
}

/// The quest of the line whose result is running now (the innermost), 0 for a line of no quest;
/// `None` when no line's result runs.
pub fn result_quest(e: &mut Engine) -> Option<u32> {
    e.ext_free::<Talk>()
        .results
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .last()
        .copied()
}

fn run_result_shared(e: &mut Engine, a: &[u32]) -> Ret {
    // A line said here for one the host's game said to its player too (a script's `SayTo`): its
    // result ran there; here the line is only heard. (Never one of the dialogue window's.)
    let speaker = a.get(2).copied().unwrap_or(0);
    // (Only a guest's game says such lines; only the host's enacts another's result.)
    if speaker != 0
        && menu_speaker(e) != Some(speaker)
        && crate::commands::skips_echoed_result(
            e,
            e.mem.u32(speaker.wrapping_add(addr::FORM_ID_OFFSET)),
            a.get(1).copied().unwrap_or(0),
        )
    {
        return Ret::default();
    }
    // A line shown in the window watching another player's conversation, or one of another
    // player's put to this player's own window: its result ran where it was said (S42). One the
    // host's game runs for a guest is not the window's; one this game's player said in the window
    // by a click is this game's (S42 step 2c).
    if !enacting(e) && crate::watch::skips_result(e, a.get(2).copied().unwrap_or(0)) {
        crate::watch::note_skipped(e);
        return Ret::default();
    }
    let t = e.ext_free::<Talk>();
    if !t.sharing.load(Ordering::SeqCst) || t.enacting.load(Ordering::SeqCst) > 0 {
        return e.call_next(addr::INFO_RUN_RESULT, a);
    }
    let word = |i: usize| a.get(i).copied().unwrap_or(0);
    let (info, index, speaker) = (word(0), word(1), word(2));
    if std::env::var_os("NV_COOP_TALK_LOG").is_some() && info != 0 {
        let id = e.mem.u32(info.wrapping_add(addr::FORM_ID_OFFSET));
        let who = if speaker == 0 {
            0
        } else {
            e.mem.u32(speaker.wrapping_add(addr::FORM_ID_OFFSET))
        };
        eprintln!(
            "co-op talk: result of line {id:08x} (part {index}) for {who:08x}, menu speaker {}, said before {}",
            menu_speaker(e) == Some(speaker),
            e.mem.u8(info + addr::INFO_SAID_OFFSET)
        );
    }
    // Only the player's own conversation: the open menu's speaker. Other lines (NPCs talking to
    // each other, combat shouts) are each game's own.
    if info == 0 || speaker == 0 || menu_speaker(e) != Some(speaker) {
        return e.call_next(addr::INFO_RUN_RESULT, a);
    }
    let npc = e.mem.u32(speaker.wrapping_add(addr::FORM_ID_OFFSET));
    let id = e.mem.u32(info.wrapping_add(addr::FORM_ID_OFFSET));
    let quest = quest_of(e, info);
    let opening = e.ext_free::<Talk>().opening.load(Ordering::SeqCst);
    let held = gate::is_guest(e) && quest != 0 && !gate::is_own_quest(e, quest) && !opening;
    let t = e.ext_free::<Talk>();
    match (held, quest) {
        (true, _) => t.held.fetch_add(1, Ordering::SeqCst),
        (false, 0) => t.unknown.fetch_add(1, Ordering::SeqCst),
        (false, _) => t.own.fetch_add(1, Ordering::SeqCst),
    };
    let chosen = crate::choices::clicking(e);
    let t = e.ext_free::<Talk>();
    t.push(Event::Said {
        npc,
        info: id,
        end: index != 0,
        held,
        chosen,
    });
    if held {
        gate::applying(e, |e| e.call_next(addr::INFO_RUN_RESULT, a))
    } else {
        e.call_next(addr::INFO_RUN_RESULT, a)
    }
}

/// Whether `speaker` (a pointer) runs a Dialogue package of the data files: it came to the
/// player to talk (a force greet), rather than the player walking up to it.
fn greets(e: &mut Engine, speaker: u32) -> bool {
    if !e.is_registered(addr::ACTOR_GET_CURRENT_EDITOR_PACKAGE)
        || !e.is_registered(addr::PACKAGE_TYPE)
    {
        return false;
    }
    let package = e
        .call(addr::ACTOR_GET_CURRENT_EDITOR_PACKAGE, &args![speaker])
        .u32();
    package != 0
        && e.call(addr::PACKAGE_TYPE, &args![package]).u32() & 0xff == addr::PACKAGE_TYPE_DIALOGUE
}

/// The cells where another player is busy now (D13): this player talking to an actor in one of
/// them is held ([`Event::Held`]) until that player is free.
pub fn set_hold(e: &mut Engine, cells: impl IntoIterator<Item = u32>) {
    let t = e.ext_free::<Talk>();
    *t.hold.write().unwrap_or_else(PoisonError::into_inner) = cells.into_iter().collect();
}

/// The cell reference `id` is in (0 when it is not in this game).
pub fn cell_of(e: &mut Engine, id: u32) -> u32 {
    forms::form(e, id).map_or(0, |r| e.mem.u32(r + addr::REF_CELL_OFFSET))
}

/// This game's player talks to (activates) actor `npc`, as the activate key does (D13: a held
/// conversation starts once the busy player is free). Whether the game took it.
pub fn activate(e: &mut Engine, npc: u32) -> bool {
    let (Some(r), player) = (forms::form(e, npc), e.mem.u32(addr::PLAYER_SINGLETON)) else {
        return false;
    };
    player != 0
        && e.is_registered(addr::REFR_ACTIVATE)
        && e.call(addr::REFR_ACTIVATE, &args![r, player, 0u32, 0u32, 1u32])
            .u32()
            & 0xff
            != 0
}

/// The seam at `0057318e`, the start of every activation. Its words: the reference, the
/// activator. This game's player activating an actor another player talks to is noted
/// ([`Event::Asked`]); the activation goes on (the game then refuses its menu: busy).
pub(crate) fn activate_site(e: &mut Engine, a: &[u32]) -> Option<u32> {
    let &[reference, activator, ..] = a else {
        return None;
    };
    let player = e.mem.u32(addr::PLAYER_SINGLETON);
    if reference == 0 || player == 0 || activator != player {
        return None;
    }
    let npc = e.mem.u32(reference.wrapping_add(addr::FORM_ID_OFFSET));
    let cell = e.mem.u32(reference.wrapping_add(addr::REF_CELL_OFFSET));
    let actor = actors::actor(e, npc).is_some();
    let t = e.ext_free::<Talk>();
    if t.busy
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .contains(&npc)
    {
        t.push(Event::Asked { npc });
    } else if actor
        && cell != 0
        && t.hold
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&cell)
    {
        t.held_npc.store(npc, Ordering::SeqCst);
        t.push(Event::Held { npc });
    }
    None
}

/// The wrap at `00761a20`. Its words: the speaker, a word for the topic manager, a target.
pub(crate) fn create(e: &mut Engine, a: &[u32]) -> Ret {
    // The window watching another player's conversation (S42): the NPC is that player's, and
    // this game only shows it.
    if crate::watch::opening(e) {
        let r = e.call_next(addr::DIALOG_MENU_CREATE, a);
        crate::choices::after_game(e);
        return r;
    }
    let speaker = a.first().copied().unwrap_or(0);
    let t = e.ext_free::<Talk>();
    if speaker != 0
        && t.sharing.load(Ordering::SeqCst)
        && !t.opening.load(Ordering::SeqCst)
        && menu_speaker(e).is_none()
    {
        let npc = e.mem.u32(speaker.wrapping_add(addr::FORM_ID_OFFSET));
        if std::env::var_os("NV_COOP_TALK_LOG").is_some() {
            let (copy, greets) = (crate::puppet::is_copy(e, npc), greets(e, speaker));
            let busy = e
                .ext_free::<Talk>()
                .busy
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .contains(&npc);
            let queued = queued(e, speaker);
            eprintln!(
                "co-op talk: conversation with {npc:08x} asked of the game: a copy {copy}, came to greet {greets}, busy {busy}, queued {queued}"
            );
        }
        // An NPC in another player's conversation: this player joins it (S42, D13), whatever
        // package its copy here runs (PC run, 2026-11-24: the guest pressed E on Ringo, whose
        // copy was on his thank-you greeting, and was refused as a greeting; she could not vote;
        // council 3-0, C).
        let joining = e
            .ext_free::<Talk>()
            .busy
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&npc);
        if joining {
            e.ext_free::<Talk>().push(Event::Blocked { npc });
            unqueue(e, speaker);
            return Ret::default();
        }
        if crate::puppet::is_copy(e, npc) && greets(e, speaker) {
            unqueue(e, speaker);
            e.ext_free::<Talk>().push(Event::NotGreeted { npc });
            return Ret::default();
        }
        // A greeting another player's game gave: not again here.
        let package = greeting_package(e, speaker);
        if package != 0
            && e.ext_free::<Talk>()
                .greeted
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&npc)
                == Some(&package)
        {
            unqueue(e, speaker);
            e.ext_free::<Talk>().push(Event::GreetGiven { npc });
            return Ret::default();
        }
        let t = e.ext_free::<Talk>();
        // The conversation this player asked for waits for a busy player (D13).
        if t.held_npc
            .compare_exchange(npc, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            unqueue(e, speaker);
            return Ret::default();
        }
    }
    let package = if speaker != 0 && e.ext_free::<Talk>().sharing.load(Ordering::SeqCst) {
        greeting_package(e, speaker)
    } else {
        0
    };
    let r = e.call_next(addr::DIALOG_MENU_CREATE, a);
    crate::choices::after_game(e);
    // The greeting began: told to the other games.
    if package != 0 && r.eax != 0 {
        let npc = e.mem.u32(speaker.wrapping_add(addr::FORM_ID_OFFSET));
        e.ext_free::<Talk>().push(Event::Greeted { npc, package });
    }
    r
}

/// Whether the game's queue of menus to open holds a conversation with `speaker` (an NPC's
/// greeting waits there until its menu is made).
fn queued(e: &mut Engine, speaker: u32) -> bool {
    e.mem.is_mapped(addr::QUEUED_MENU_FIRST + 3)
        && e.mem.u32(addr::QUEUED_MENU_KIND) == addr::QUEUED_MENU_DIALOGUE
        && e.mem.u32(addr::QUEUED_MENU_FIRST) == speaker
}

/// Takes a refused conversation with `speaker` out of the game's queue of menus to open. The
/// game tries a queued menu again on every frame until it is made, so a refused one stayed
/// queued and opened later, when what refused it had changed: Sunny, told to walk outside
/// after her talk with one player, then greeted the other with the menu her force greet had
/// queued, and stood there in both games until that player answered (the owner, 2026-11-16);
/// Doc Mitchell turned to the second player the moment the first was done.
fn unqueue(e: &mut Engine, speaker: u32) {
    if queued(e, speaker) {
        e.mem.set_u32(addr::QUEUED_MENU_KIND, 0);
        e.mem.set_u32(addr::QUEUED_MENU_FIRST, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{Story, StorySpec};

    const MQ: u32 = 0x0010_4c0c;
    const OWN: u32 = 0x0010_4c1c;
    const NPC: u32 = 0x0010_4f25;
    const OTHER: u32 = 0x0010_4e85;
    const CELL: u32 = 0x0010_5a00;
    /// A line of the shared story that sets MQ's stage 10, one of the player's own quest that
    /// sets OWN's stage 5, and one with no quest.
    const SHARED_LINE: u32 = 0x0010_9001;
    const OWN_LINE: u32 = 0x0010_9002;
    const LOOSE_LINE: u32 = 0x0010_9003;
    /// A force greet (a Dialogue package) and a package of another type.
    const GREET: u32 = 0x0010_a219;
    const WANDER: u32 = 0x0010_a21c;

    fn story() -> Story {
        let mut s = Story::new(&StorySpec {
            quests: vec![(MQ, vec![5, 10, 20]), (OWN, vec![5])],
            cells: vec![CELL],
            actors: vec![NPC, OTHER],
            packages: vec![GREET, WANDER],
            ..StorySpec::default()
        });
        s.add_info(
            SHARED_LINE,
            MQ,
            "Head on up to the saloon.",
            "Where is Sunny?",
        );
        s.set_info_result(SHARED_LINE, false, MQ, 10);
        s.set_info_result(SHARED_LINE, true, MQ, 20);
        s.add_info(OWN_LINE, OWN, "You have been out cold.", "");
        s.set_info_result(OWN_LINE, false, OWN, 5);
        s.add_info(LOOSE_LINE, 0, "Howdy.", "");
        s.move_player(CELL, [0.0; 3], 0.0);
        s.place(NPC, CELL);
        s
    }

    #[test]
    fn with_sharing_off_a_conversation_is_the_games_own() {
        let mut s = story();
        gate::set_guest(&mut s.e, true);
        assert!(s.talk_to(NPC));
        s.say(SHARED_LINE, false);
        assert_eq!(forms::quest_stage(&mut s.e, MQ), Some(10));
        assert!(take_events(&mut s.e).is_empty());
        assert_eq!(counts(&mut s.e), (0, 0, 0));
    }

    #[test]
    fn a_host_notes_its_lines_and_runs_their_results() {
        let mut s = story();
        set_sharing(&mut s.e, true);
        assert_eq!(menu_npc(&mut s.e), None);
        assert!(s.talk_to(NPC));
        assert_eq!(menu_npc(&mut s.e), Some(NPC));
        s.say(SHARED_LINE, false);
        s.say(SHARED_LINE, true);
        assert_eq!(forms::quest_stage(&mut s.e, MQ), Some(20));
        assert_eq!(
            take_events(&mut s.e),
            [false, true].map(|end| Event::Said {
                npc: NPC,
                info: SHARED_LINE,
                end,
                held: false,
                chosen: !end,
            })
        );
        assert!(take_events(&mut s.e).is_empty());
    }

    /// S30 (harness, 2026-11-13): a line's result script names no owner quest, so Trudy's
    /// `RewardXP 50` that ends VCG03 was not seen as the story's. The line's own quest is the one,
    /// for this game's own conversation and for a guest's line the host runs.
    #[test]
    fn a_lines_experience_is_its_quests() {
        let mut s = story();
        set_sharing(&mut s.e, true);
        s.set_info_reward_xp(SHARED_LINE, true, 50);
        assert!(s.talk_to(NPC));
        s.say(SHARED_LINE, true);
        assert_eq!(
            crate::reward::take(&mut s.e),
            vec![crate::reward::Given {
                quest: MQ,
                what: crate::reward::What::Xp(50)
            }]
        );
        s.leave();
        enact(&mut s.e, NPC, SHARED_LINE, true).unwrap();
        assert_eq!(
            crate::reward::take(&mut s.e),
            vec![crate::reward::Given {
                quest: MQ,
                what: crate::reward::What::Xp(50)
            }]
        );
        assert_eq!(s.xp(), 100);
        assert_eq!(result_quest(&mut s.e), None);
    }

    /// S30: while this game runs another player's line, what it takes from this game's player is
    /// kept for that player and not taken here; from any other reference, or a form that is not
    /// an item itself (a form list), the command runs as ever. This game's own line pays itself.
    #[test]
    fn a_line_run_for_another_player_keeps_what_it_takes_from_the_player() {
        const CAPS: u32 = 0xf;
        const LIST: u32 = 0x0010_7000;
        let mut s = story();
        set_sharing(&mut s.e, true);
        s.add_item_form(CAPS);
        s.add_non_item_form(LIST);
        s.set_player_items(CAPS, 50);
        s.set_info_remove_item(SHARED_LINE, true, CAPS, 30, false);
        enact(&mut s.e, NPC, SHARED_LINE, true).unwrap();
        assert_eq!(
            crate::pay::drain(&mut s.e),
            vec![crate::pay::Payment {
                item: CAPS,
                count: 30,
                silent: false
            }]
        );
        assert_eq!((s.items(CAPS), s.remove_item_runs()), (50, 0));
        // From the NPC: the command's own.
        s.set_info_remove_item_from(SHARED_LINE, true, NPC, CAPS, 30, false);
        enact(&mut s.e, NPC, SHARED_LINE, true).unwrap();
        assert!(crate::pay::drain(&mut s.e).is_empty());
        assert_eq!(s.remove_item_runs(), 1);
        // A form list: the command's own.
        s.set_info_remove_item(SHARED_LINE, true, LIST, 1, false);
        enact(&mut s.e, NPC, SHARED_LINE, true).unwrap();
        assert!(crate::pay::drain(&mut s.e).is_empty());
        assert_eq!(s.remove_item_runs(), 2);
        // This game's own line.
        s.set_info_remove_item(SHARED_LINE, true, CAPS, 30, false);
        assert!(s.talk_to(NPC));
        s.say(SHARED_LINE, true);
        assert!(crate::pay::drain(&mut s.e).is_empty());
        assert_eq!((s.items(CAPS), s.remove_item_runs()), (20, 3));
    }

    /// The owner, 2026-11-23: karma and perks the story gives are noted for every
    /// player, as experience is; a perk on another reference is not; a personal perk is not, and
    /// in another player's line it is kept for that player and not given here. Each is given in
    /// another game as the game gives it.
    #[test]
    fn a_lines_karma_and_perks_are_noted_and_given_alike() {
        use crate::reward::{give, take, What};
        const PERK: u32 = 0x0010_8002;
        const IMPLANT: u32 = 0x0010_8003;
        let mut s = story();
        set_sharing(&mut s.e, true);
        s.add_perk_form(PERK, 2);
        s.add_perk_form(IMPLANT, 1);
        crate::reward::set_personal_perks(&mut s.e, [IMPLANT]);
        let script = s.info_script(SHARED_LINE, true);
        s.script_karma(script, -5);
        // (No change, nothing to note.)
        s.script_karma(script, 0);
        s.script_perk(script, PERK, true, false, 0);
        s.script_perk(script, PERK, false, false, NPC);
        s.script_perk(script, IMPLANT, false, false, 0);
        s.script_perk(script, IMPLANT, false, true, 0);
        let shared = vec![
            What::Karma(-5),
            What::Perk {
                perk: PERK,
                teammate: true,
            },
        ];
        // This game's own line: given here, the shared ones noted, the personal one here alone.
        assert!(s.talk_to(NPC));
        s.say(SHARED_LINE, true);
        s.leave();
        let noted: Vec<What> = take(&mut s.e).into_iter().map(|g| g.what).collect();
        assert_eq!(noted, shared);
        assert!(crate::reward::drain_personal(&mut s.e).is_empty());
        assert_eq!(s.karma(), -5);
        assert_eq!(
            (s.perk_rank(PERK, true), s.perk_rank(IMPLANT, false)),
            (1, 0)
        );
        // Another player's line: the same noted; the personal perk kept for that player.
        enact(&mut s.e, NPC, SHARED_LINE, true).unwrap();
        let noted: Vec<What> = take(&mut s.e).into_iter().map(|g| g.what).collect();
        assert_eq!(noted, shared);
        assert_eq!(
            crate::reward::drain_personal(&mut s.e),
            vec![
                What::Perk {
                    perk: IMPLANT,
                    teammate: false
                },
                What::PerkRemoved {
                    perk: IMPLANT,
                    teammate: false
                }
            ]
        );
        assert_eq!((s.perk_rank(PERK, true), s.karma()), (2, -10));
        // Given in another game, the game's own way.
        let mut other = story();
        other.add_perk_form(PERK, 2);
        for w in shared {
            assert!(give(&mut other.e, w));
        }
        assert_eq!(other.karma(), -5);
        assert_eq!(other.perk_rank(PERK, true), 1);
        // No more than the perk's highest rank.
        for _ in 0..3 {
            assert!(give(
                &mut other.e,
                What::Perk {
                    perk: PERK,
                    teammate: false
                }
            ));
        }
        assert_eq!(other.perk_rank(PERK, false), 2);
        assert!(give(
            &mut other.e,
            What::PerkRemoved {
                perk: PERK,
                teammate: true
            }
        ));
        assert_eq!(other.perk_rank(PERK, true), 0);
        assert!(!give(
            &mut other.e,
            What::Perk {
                perk: IMPLANT,
                teammate: false
            }
        ));
    }

    /// PC run 30: Sunny hands over her rifle at the bottles by speaking up, outside the guest's
    /// conversation; the line's result ran in the guest's game, whose player alone got the rifle.
    /// On a guest, a story line said outside its player's conversation is noted; its own
    /// conversation's results are held for the host and give it nothing here.
    #[test]
    fn a_guests_story_line_said_outside_its_conversation_notes_its_reward() {
        let mut s = story();
        set_sharing(&mut s.e, true);
        gate::set_guest(&mut s.e, true);
        s.set_info_reward_xp(SHARED_LINE, true, 50);
        s.say_as(NPC, SHARED_LINE, true);
        assert_eq!(s.xp(), 50);
        assert_eq!(
            crate::reward::take(&mut s.e),
            vec![crate::reward::Given {
                quest: MQ,
                what: crate::reward::What::Xp(50)
            }]
        );
        assert!(s.talk_to(NPC));
        s.say(SHARED_LINE, true);
        assert_eq!(s.xp(), 50);
        assert_eq!(crate::reward::take(&mut s.e), vec![]);
    }

    #[test]
    fn a_guest_holds_the_shared_story_s_results_and_runs_its_own() {
        let mut s = story();
        gate::set_guest(&mut s.e, true);
        gate::set_own_quests(&mut s.e, [OWN]);
        set_sharing(&mut s.e, true);
        assert!(s.talk_to(NPC));
        s.say(SHARED_LINE, false);
        s.say(OWN_LINE, false);
        s.say(LOOSE_LINE, false);
        // The shared line's result did not run here; the menu's bookkeeping did (the line is
        // said), the player's own quest moved, the loose line ran as before.
        assert_eq!(forms::quest_stage(&mut s.e, MQ), Some(0));
        assert_eq!(forms::quest_stage(&mut s.e, OWN), Some(5));
        assert_eq!(s.said(SHARED_LINE), 1);
        assert_eq!(counts(&mut s.e), (1, 1, 1));
        let held: Vec<bool> = take_events(&mut s.e)
            .into_iter()
            .map(|ev| matches!(ev, Event::Said { held: true, .. }))
            .collect();
        assert_eq!(held, [true, false, false]);
        // Twin: the same line with the wrap passing it on runs the result on the guest.
        set_sharing(&mut s.e, false);
        s.say(SHARED_LINE, false);
        assert_eq!(forms::quest_stage(&mut s.e, MQ), Some(10));
    }

    #[test]
    fn a_line_outside_the_players_conversation_is_left_alone() {
        let mut s = story();
        gate::set_guest(&mut s.e, true);
        set_sharing(&mut s.e, true);
        // No menu open: an NPC's own line.
        s.say_as(NPC, SHARED_LINE, false);
        assert_eq!(forms::quest_stage(&mut s.e, MQ), Some(10));
        // A menu open on one NPC, a line of another.
        assert!(s.talk_to(NPC));
        s.say_as(OTHER, SHARED_LINE, true);
        assert_eq!(forms::quest_stage(&mut s.e, MQ), Some(20));
        assert!(take_events(&mut s.e).is_empty());
    }

    #[test]
    fn the_host_runs_a_guests_result_without_taking_it_for_its_own_conversation() {
        let mut s = story();
        set_sharing(&mut s.e, true);
        // The host is itself talking to that NPC's neighbour.
        s.place(OTHER, CELL);
        assert!(s.talk_to(OTHER));
        assert_eq!(enact(&mut s.e, NPC, SHARED_LINE, false), Ok(()));
        assert_eq!(forms::quest_stage(&mut s.e, MQ), Some(10));
        assert!(take_events(&mut s.e).is_empty());
        assert_eq!(
            enact(&mut s.e, NPC, MQ, false),
            Err(Refusal::NoInfo(MQ)),
            "a quest is not an info"
        );
        assert_eq!(
            enact(&mut s.e, 0x0010_ffff, SHARED_LINE, false),
            Err(Refusal::NoSpeaker(0x0010_ffff))
        );
        assert_eq!(info_quest(&mut s.e, SHARED_LINE), Some(MQ));
        assert_eq!(info_quest(&mut s.e, LOOSE_LINE), Some(0));
        assert_eq!(info_quest(&mut s.e, NPC), None);
    }

    #[test]
    fn an_npc_another_player_talks_to_gets_no_menu() {
        let mut s = story();
        set_sharing(&mut s.e, true);
        set_busy(&mut s.e, [NPC]);
        assert!(!s.talk_to(NPC));
        assert_eq!(menu_npc(&mut s.e), None);
        assert_eq!(take_events(&mut s.e), [Event::Blocked { npc: NPC }]);
        set_busy(&mut s.e, []);
        assert!(s.talk_to(NPC));
        assert!(close_menu(&mut s.e));
        assert_eq!(menu_npc(&mut s.e), None);
        assert!(!close_menu(&mut s.e));
        // Twin: with sharing off the busy list is not consulted.
        set_sharing(&mut s.e, false);
        set_busy(&mut s.e, [NPC]);
        assert!(s.talk_to(NPC));
    }

    /// PC run 31: the guest ended Sunny's tutorial; in the host's game her copy (the guest's game
    /// ran her) came to the host on her Dialogue package, and the first line there started the
    /// campfire quest for both. A copy's own force greet gets no menu; the player walking up to a
    /// copy, or an actor this game runs coming to it, does.
    #[test]
    fn a_copy_does_not_greet_this_games_player_on_its_own() {
        let mut s = story();
        set_sharing(&mut s.e, true);
        s.set_package_type(GREET, addr::PACKAGE_TYPE_DIALOGUE);
        s.place(NPC, CELL);
        s.set_own_package(NPC, GREET);
        crate::puppet::set_copies(&mut s.e, [(NPC, 0)]);
        assert!(!s.talk_to(NPC));
        assert_eq!(menu_npc(&mut s.e), None);
        assert_eq!(take_events(&mut s.e), [Event::NotGreeted { npc: NPC }]);
        // On another package it is the player who walked up: the conversation goes on.
        s.set_own_package(NPC, WANDER);
        assert!(s.talk_to(NPC));
        assert!(close_menu(&mut s.e));
        // This game's own actor greets as ever (and the greeting is told to every game).
        s.set_own_package(NPC, GREET);
        crate::puppet::set_copies(&mut s.e, []);
        assert!(s.talk_to(NPC));
        assert!(close_menu(&mut s.e));
        assert_eq!(
            take_events(&mut s.e),
            [Event::Greeted {
                npc: NPC,
                package: GREET
            }]
        );
        // Without sharing the game has it as ever.
        crate::puppet::set_copies(&mut s.e, [(NPC, 0)]);
        set_sharing(&mut s.e, false);
        assert!(s.talk_to(NPC));
        assert!(close_menu(&mut s.e));
        assert!(take_events(&mut s.e).is_empty());
    }

    /// PC run, 2026-11-23: Ringo thanked the host, then ran to the guest and thanked her again.
    /// A greeting another player's game gave ([`set_greeted`]) is not given again with the same
    /// package; with another package, or the player walking up, the conversation is had.
    #[test]
    fn a_greeting_given_is_not_given_again() {
        let mut s = story();
        set_sharing(&mut s.e, true);
        s.set_package_type(GREET, addr::PACKAGE_TYPE_DIALOGUE);
        s.place(NPC, CELL);
        s.set_own_package(NPC, GREET);
        set_greeted(&mut s.e, NPC, GREET);
        assert!(!s.talk_to(NPC));
        assert_eq!(menu_npc(&mut s.e), None);
        assert_eq!(take_events(&mut s.e), [Event::GreetGiven { npc: NPC }]);
        // The player walks up to it on another package: had.
        s.set_own_package(NPC, WANDER);
        assert!(s.talk_to(NPC));
        assert!(close_menu(&mut s.e));
        assert!(take_events(&mut s.e).is_empty());
        // A greeting on another Dialogue package: had, and told.
        set_greeted(&mut s.e, NPC, WANDER);
        s.set_own_package(NPC, GREET);
        assert!(s.talk_to(NPC));
        assert!(close_menu(&mut s.e));
        assert_eq!(
            take_events(&mut s.e),
            [Event::Greeted {
                npc: NPC,
                package: GREET
            }]
        );
        // This game's own greeting is its own game's to give again.
        assert!(s.talk_to(NPC));
        assert!(close_menu(&mut s.e));
    }

    /// The owner, 2026-11-16: a refused conversation stayed in the game's queue of menus and
    /// opened later (Sunny greeted the second player after her talk with the first, and stood in
    /// both games). A refused one is taken out of the queue; another speaker's, or another kind
    /// of menu, is left.
    #[test]
    fn a_refused_conversation_does_not_stay_queued() {
        let mut s = story();
        s.e.mem.map(addr::QUEUED_MENU_KIND & !0xfff, 0x1000);
        set_sharing(&mut s.e, true);
        s.set_package_type(GREET, addr::PACKAGE_TYPE_DIALOGUE);
        s.place(NPC, CELL);
        s.place(OTHER, CELL);
        s.set_own_package(NPC, GREET);
        crate::puppet::set_copies(&mut s.e, [(NPC, 0)]);
        let queue = |s: &mut Story, kind: u32, who: u32| {
            let p = crate::forms::form(&mut s.e, who).unwrap();
            s.e.mem.set_u32(addr::QUEUED_MENU_KIND, kind);
            s.e.mem.set_u32(addr::QUEUED_MENU_FIRST, p);
        };
        let queued = |s: &mut Story| s.e.mem.u32(addr::QUEUED_MENU_KIND);
        // The copy's force greet, queued by the game: refused and no longer queued.
        queue(&mut s, addr::QUEUED_MENU_DIALOGUE, NPC);
        assert!(!s.talk_to(NPC));
        assert_eq!(queued(&mut s), 0);
        assert_eq!(s.e.mem.u32(addr::QUEUED_MENU_FIRST), 0);
        // Another speaker's queued conversation, and another kind of menu, are left.
        queue(&mut s, addr::QUEUED_MENU_DIALOGUE, OTHER);
        assert!(!s.talk_to(NPC));
        assert_eq!(queued(&mut s), addr::QUEUED_MENU_DIALOGUE);
        queue(&mut s, 1, NPC);
        assert!(!s.talk_to(NPC));
        assert_eq!(queued(&mut s), 1);
        // An NPC another player talks to: this player's click is dropped, not kept for later.
        crate::puppet::set_copies(&mut s.e, []);
        set_busy(&mut s.e, [NPC]);
        queue(&mut s, addr::QUEUED_MENU_DIALOGUE, NPC);
        assert!(!s.talk_to(NPC));
        assert_eq!(queued(&mut s), 0);
        // A conversation that opens is the game's to take out of its queue.
        set_busy(&mut s.e, []);
        queue(&mut s, addr::QUEUED_MENU_DIALOGUE, NPC);
        assert!(s.talk_to(NPC));
        assert_eq!(queued(&mut s), addr::QUEUED_MENU_DIALOGUE);
    }

    /// The owner, 2026-11-14: both players sat for Doc Mitchell's questions; he asked the host's
    /// and stared at the guest until the host was done (the host's conversation made him busy for
    /// the guest). While this player's own opening runs its conversations are its own: no NPC is
    /// refused it, and on a guest a line of the shared story runs here instead of being held.
    #[test]
    fn in_its_own_opening_a_players_conversations_are_its_own() {
        let mut s = story();
        gate::set_guest(&mut s.e, true);
        gate::set_own_quests(&mut s.e, [OWN]);
        set_sharing(&mut s.e, true);
        set_busy(&mut s.e, [NPC]);
        set_own_opening(&mut s.e, true);
        assert!(s.talk_to(NPC));
        s.say(SHARED_LINE, false);
        assert_eq!(forms::quest_stage(&mut s.e, MQ), Some(10));
        let held: Vec<bool> = take_events(&mut s.e)
            .into_iter()
            .map(|ev| matches!(ev, Event::Said { held: true, .. }))
            .collect();
        assert_eq!(held, [false]);
        assert!(close_menu(&mut s.e));
        // The opening over: refused again, and held again.
        set_own_opening(&mut s.e, false);
        assert!(!s.talk_to(NPC));
        set_busy(&mut s.e, []);
        assert!(s.talk_to(NPC));
        s.say(SHARED_LINE, true);
        assert_eq!(forms::quest_stage(&mut s.e, MQ), Some(10));
    }

    #[test]
    fn a_line_and_its_prompt_are_read_and_shown_through_the_game() {
        let mut s = story();
        assert_eq!(
            line(&mut s.e, SHARED_LINE),
            Some((
                "Head on up to the saloon.".to_string(),
                "Where is Sunny?".to_string()
            ))
        );
        assert_eq!(
            line(&mut s.e, LOOSE_LINE),
            Some(("Howdy.".to_string(), String::new()))
        );
        assert_eq!(line(&mut s.e, MQ), None);
        assert!(show(&mut s.e, "Howdy."));
        let long = "x".repeat(600);
        assert!(show(&mut s.e, &long));
        let shown = s.messages();
        assert_eq!(shown[0], "Howdy.");
        assert_eq!(shown[1].len(), addr::HUD_MESSAGE_MAX);
        assert!(near(&mut s.e, NPC));
        assert!(!near(&mut s.e, OTHER));
    }
}
