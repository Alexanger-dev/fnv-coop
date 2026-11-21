//! Conversations in a session (docs/coop/SPECS.md S16): coop-core's dialogue protocol driven by
//! the game's own dialogue menu.
//!
//! - [`Talker`] runs on every machine. What this game's own conversation says
//!   (`coop_engine::talk`'s events) becomes the protocol's moves: the first shared line asks the
//!   host for the NPC (`Open`), each line said is a `Pick` of its info, its end a `LineDone`, the
//!   menu closing a `Close`. A machine that watches another player's conversation shows each
//!   line picked when its player is in the NPC's cell: in the game's own dialogue window on its
//!   NPC (`coop_engine::watch`, S42), else (another menu open, the player closed the window)
//!   as the game's own HUD messages. It reports the line done at once, so the talker never
//!   waits for a watcher. The window closes when the conversation ends, the player leaves the
//!   NPC's cell, or nothing new is said for [`WATCH_QUIET_STEPS`]. The NPC another
//!   player talks to is named busy to the game, which then opens no menu on it; a conversation
//!   the host refuses is closed through the game.
//! - [`Enactor`] runs on the host: a guest's pick of a line of the shared story has its result
//!   run in the host's game, once, when the protocol says so (the begin result at the pick, the
//!   end result once every machine has reported the line done).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use coop_core::dialogue::{DialogueClient, DialogueEvent, DialogueMsg};
use coop_core::ids::{FormId, PlayerId};
use coop_engine::talk::{self, Event};
use engine_core::Engine;

/// A line of this game's conversation waiting for its turn in the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Said {
    npc: u32,
    info: u32,
    end: bool,
}

/// One machine's part in the dialogue protocol.
#[derive(Debug, Default)]
pub struct Talker {
    queue: VecDeque<Said>,
    /// An `Open` sent and not answered: (request id, the NPC).
    pending: Option<(u32, u32)>,
    /// A `Pick` sent for this step and not confirmed.
    awaiting: Option<u32>,
    closing: bool,
    /// The watched line last shown: (session, step).
    shown: Option<(u32, u32)>,
    /// Conversations of this player the host opened.
    pub opened: u32,
    /// Conversations the host refused (the NPC was taken, or dead).
    pub denied: u32,
    /// Conversations the game did not start here because another player has the NPC.
    pub blocked: u32,
    /// Conversations a copy of an actor another game runs began itself with this player, left to
    /// the owner's game (it greets its own player).
    pub not_greeted: u32,
    /// The actors whose copies came to greet this player since the last read: asked of the host,
    /// which has this player's game run them (it talks to whichever player comes to it).
    pub greet_asks: Vec<u32>,
    /// Lines of this player's conversations sent as picks.
    pub picks: u32,
    /// Other players' lines marked as said in this game.
    pub lines_marked: u32,
    /// Other players' lines shown here.
    pub lines_shown: u32,
    /// Other players' lines said while this player was not with the NPC (shown as text, or held
    /// back while busy: [`Self::lines_far_shown`], [`Self::lines_far_held`]).
    pub lines_far: u32,
    /// Other players' lines shown in this game's own dialogue window (S42), not as text.
    pub lines_windowed: u32,
    /// Watching windows closed by co-op: the conversation ended, this player left the NPC, or
    /// it went quiet for [`WATCH_QUIET_STEPS`].
    pub windows_closed: u32,
    /// The conversation the window shows: (session, the steps since its last new line).
    watched: Option<(u32, u32)>,
    /// Another player's conversation this player is far from (S42: shown as text, held back
    /// while this player is busy).
    far: Option<Far>,
    /// "... finished talking to ...", waiting for this player to be free, oldest first.
    finished: Vec<(PlayerId, u32)>,
    /// Other players' lines shown as text to this player, far from the NPC.
    pub lines_far_shown: u32,
    /// Other players' lines held back: this player was far from the NPC and busy.
    pub lines_far_held: u32,
    /// "... is talking to ..." and "... finished talking to ..." notices shown instead.
    pub far_notices: u32,
    /// Other players' lines as text, waiting to be shown one subtitle at a time.
    subtitles: VecDeque<String>,
    /// Steps the first waiting subtitle has waited for the HUD's own to end.
    subtitle_waited: u32,
    /// Lines shown as the game's subtitles.
    pub subtitles_shown: u32,
    /// Lines shown as HUD messages instead (the game's subtitles are off).
    pub subtitles_as_messages: u32,
    /// S42 step 2: the options last sent to the table (session, step, infos).
    offered: Option<(u32, u32, Vec<u32>)>,
    /// The game's option lists built when the step being played was first seen: options are
    /// sent only from a list built after it.
    list_mark: Option<(u32, u32, u64)>,
    /// Whether the window has the other players' options of a conversation.
    foreign_set: bool,
    /// Option lists sent to the table.
    pub options_sent: u32,
    /// Watching windows that listed this player's options.
    pub windows_listed: u32,
    /// The other players' options put to the window (each time they changed).
    pub menus_shown: u32,
    /// Clicks on another player's option (S42 step 2c says them; until then not said).
    pub foreign_clicks: u32,
    /// A quest of this player's own and one of its stages: until that stage is done in this game
    /// the player is in its own opening, and its conversations are its own (each game has its own
    /// Doc Mitchell for it): none of its lines goes to the protocol, and no NPC is refused to it.
    pub opening: Option<(u32, u8)>,
    /// Lines of this player's conversations kept its own because its opening runs.
    pub opening_lines: u32,
}

/// Another player's conversation, as this player, far from the NPC, follows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Far {
    session: u32,
    initiator: PlayerId,
    npc: u32,
    /// "... is talking to ..." was shown (once per conversation).
    told: bool,
    /// A line was held back: the end is told when this player is free.
    held: bool,
}

fn player_name(p: PlayerId) -> String {
    format!("Player {}", u32::from(p.0) + 1)
}

/// A watching window whose conversation has said nothing new for this many steps (frames) is
/// closed: about five minutes at 60 frames a second (a talker may read a long list of topics).
pub const WATCH_QUIET_STEPS: u32 = 18_000;

/// How a watched line is shown as text: what the player chose, when the info has a prompt, then
/// each of the NPC's responses (after the NPC's name, when it has one), each as a subtitle of its
/// own ([`talk::subtitle_parts`]: a long one in several).
fn messages(initiator: PlayerId, npc: Option<&str>, text: &str, prompt: &str) -> Vec<String> {
    let mut out = Vec::new();
    if !prompt.is_empty() {
        out.extend(talk::subtitle_parts(&format!(
            "{}: {prompt}",
            player_name(initiator)
        )));
    }
    for t in text.split(" | ").filter(|t| !t.trim().is_empty()) {
        let t = match npc {
            Some(name) => format!("{name}: {}", t.trim()),
            None => t.trim().to_string(),
        };
        out.extend(talk::subtitle_parts(&t));
    }
    out
}

/// Subtitles waiting to be shown at most: more drops the oldest.
pub const SUBTITLES_QUEUED_MAX: usize = 32;
/// How many steps (frames) a waiting subtitle waits for the HUD's own (an NPC's nearby) to end
/// before it is shown anyway.
pub const SUBTITLE_WAIT_STEPS: u32 = 600;

impl Talker {
    fn queue_subtitles(&mut self, lines: Vec<String>) {
        self.subtitles.extend(lines);
        while self.subtitles.len() > SUBTITLES_QUEUED_MAX {
            self.subtitles.pop_front();
        }
    }

    /// Shows the next waiting subtitle once the HUD's last has ended (each its full time: the
    /// game counts a subtitle's time from when it is queued), not while the player is busy. Where
    /// the game shows no subtitles, as a HUD message.
    fn show_subtitles(&mut self, e: &mut Engine) {
        if self.subtitles.is_empty() || talk::busy(e) {
            return;
        }
        if talk::subtitle_showing(e) && self.subtitle_waited < SUBTITLE_WAIT_STEPS {
            self.subtitle_waited += 1;
            return;
        }
        self.subtitle_waited = 0;
        let Some(line) = self.subtitles.pop_front() else {
            return;
        };
        if talk::subtitle(e, &line) {
            self.subtitles_shown += 1;
        } else {
            talk::show(e, &line);
            self.subtitles_as_messages += 1;
        }
    }

    /// One step. `share` says which of this game's lines are the shared story's (on a guest the
    /// ones the game held back for the host; on the host the ones of a shared quest). Returns the
    /// protocol messages to send to the host's dialogue table.
    pub fn step(
        &mut self,
        e: &mut Engine,
        me: PlayerId,
        client: &mut DialogueClient,
        share: &dyn Fn(&mut Engine, u32, bool) -> bool,
    ) -> Vec<DialogueMsg> {
        let mut out = Vec::new();
        let opening = self.opening.is_some_and(|(quest, stage)| {
            coop_engine::quest::record(e, quest).is_some_and(|r| !r.done.contains(&stage))
        });
        talk::set_own_opening(e, opening);
        for ev in talk::take_events(e) {
            match ev {
                Event::Said {
                    npc,
                    info,
                    end,
                    held,
                } => {
                    if opening {
                        self.opening_lines += 1;
                    } else if share(e, info, held) {
                        self.queue.push_back(Said { npc, info, end });
                    }
                }
                Event::NotGreeted { npc } => {
                    self.not_greeted += 1;
                    if !self.greet_asks.contains(&npc) {
                        self.greet_asks.push(npc);
                    }
                }
                Event::Blocked { .. } => {
                    self.blocked += 1;
                    talk::show(e, "Another player is talking to them.");
                }
            }
        }
        // The NPC another player talks to gets no menu here.
        // (In this player's opening the game refuses no NPC: `talk::set_own_opening`.)
        let busy = client.view().filter(|v| v.initiator != me).map(|v| v.npc.0);
        talk::set_busy(e, busy);
        self.watch(e, me, client, &mut out);
        self.drive(e, client, &mut out);
        self.options(e, me, client, &mut out);
        out
    }

    /// S42 step 2: this player's own options to the table, when its window (its own
    /// conversation's, or the one showing another player's) lists them between two steps; the
    /// other players' options to the window, with their names; a watching window lists its
    /// options once the table has the conversation's list.
    fn options(
        &mut self,
        e: &mut Engine,
        me: PlayerId,
        client: &mut DialogueClient,
        out: &mut Vec<DialogueMsg>,
    ) {
        if coop_engine::choices::take_clicked(e).is_some() {
            self.foreign_clicks += 1;
        }
        let view = client.view().copied();
        let mine = view.filter(|v| {
            if v.initiator == me {
                talk::menu_npc(e) == Some(v.npc.0)
            } else {
                self.watched.is_some_and(|(s, _)| s == v.session)
                    && coop_engine::watch::watching(e) == Some(v.npc.0)
            }
        });
        let Some(v) = mine else {
            if std::mem::take(&mut self.foreign_set) {
                coop_engine::choices::set_foreign(e, Vec::new());
            }
            self.offered = None;
            self.list_mark = None;
            return;
        };
        let foreign: Vec<(u32, String)> = client
            .menu()
            .unwrap_or_default()
            .iter()
            .filter(|i| !i.owners.contains(&me))
            .filter_map(|i| Some((i.choice, player_name(*i.owners.first()?))))
            .collect();
        self.foreign_set = !foreign.is_empty();
        if coop_engine::choices::set_foreign(e, foreign) {
            self.menus_shown += 1;
        }
        let lists = coop_engine::choices::counts(e).0;
        if v.playing.is_some() {
            if self
                .list_mark
                .map_or(true, |(s, step, _)| (s, step) != (v.session, v.step))
            {
                self.list_mark = Some((v.session, v.step, lists));
            }
            return;
        }
        if v.initiator != me && client.menu().is_some() && coop_engine::watch::show_topics(e) {
            self.windows_listed += 1;
        }
        let lists = coop_engine::choices::counts(e).0;
        let fresh = self.list_mark.map_or(true, |(s, step, at)| {
            (s, step) != (v.session, v.step) || lists > at
        });
        if !fresh {
            return;
        }
        let Some(own) = coop_engine::choices::own(e) else {
            return;
        };
        let key = (v.session, v.step + 1, own.clone());
        if self.offered.as_ref() == Some(&key) {
            return;
        }
        if let Some(msg) = client.options(own) {
            out.push(msg);
            self.offered = Some(key);
            self.options_sent += 1;
        }
    }

    /// A line of another player's conversation: shown once, and reported done.
    fn watch(
        &mut self,
        e: &mut Engine,
        me: PlayerId,
        client: &mut DialogueClient,
        out: &mut Vec<DialogueMsg>,
    ) {
        coop_engine::watch::step(e);
        self.show_subtitles(e);
        let view = client.view().copied().filter(|v| v.initiator != me);
        // The far conversation is over: its end is told when lines were held back.
        if let Some(f) = self.far {
            if view.map_or(true, |v| v.session != f.session) {
                self.far = None;
                if f.held {
                    self.finished.push((f.initiator, f.npc));
                }
            }
        }
        if !self.finished.is_empty() && !talk::busy(e) {
            for (who, npc) in std::mem::take(&mut self.finished) {
                let name = talk::name_of(e, npc).unwrap_or_else(|| "someone".into());
                talk::show(
                    e,
                    &format!("{} finished talking to {name}.", player_name(who)),
                );
                self.far_notices += 1;
            }
        }
        // The window's conversation is over, another began, or this player left the NPC.
        if let Some((session, quiet)) = self.watched {
            let over = view.map_or(true, |v| v.session != session || !talk::near(e, v.npc.0));
            let quiet = quiet + 1;
            if over || quiet >= WATCH_QUIET_STEPS {
                self.watched = None;
                if coop_engine::watch::end_watch(e) {
                    self.windows_closed += 1;
                }
            } else {
                self.watched = Some((session, quiet));
            }
        }
        let Some(v) = view else {
            return;
        };
        let Some(choice) = v.playing.filter(|_| !v.reported) else {
            return;
        };
        if self.shown != Some((v.session, v.step)) {
            self.shown = Some((v.session, v.step));
            // Said in the story once, for every player: a line the game says once is not said
            // again to this player.
            if talk::mark_said(e, choice) {
                self.lines_marked += 1;
            }
            let window = talk::near(e, v.npc.0)
                && coop_engine::watch::watch_line(e, v.npc.0, choice)
                    == coop_engine::watch::Outcome::Window;
            if window {
                self.watched = Some((v.session, 0));
                self.lines_windowed += 1;
            } else if talk::near(e, v.npc.0) {
                if let Some((text, prompt)) = talk::line(e, choice) {
                    let name = talk::name_of(e, v.npc.0);
                    self.queue_subtitles(messages(v.initiator, name.as_deref(), &text, &prompt));
                    self.lines_shown += 1;
                }
            } else {
                // Far from the NPC (S42, the owner's choice of 2026-11-21): the line as text, or,
                // while this player is busy, held back and a word that the conversation goes on.
                self.lines_far += 1;
                let f = self.far.filter(|f| f.session == v.session).unwrap_or(Far {
                    session: v.session,
                    initiator: v.initiator,
                    npc: v.npc.0,
                    told: false,
                    held: false,
                });
                let f = if talk::busy(e) {
                    self.lines_far_held += 1;
                    if !f.told {
                        let name = talk::name_of(e, v.npc.0).unwrap_or_else(|| "someone".into());
                        talk::show(
                            e,
                            &format!("{} is talking to {name}.", player_name(v.initiator)),
                        );
                        self.far_notices += 1;
                    }
                    Far {
                        told: true,
                        held: true,
                        ..f
                    }
                } else {
                    if let Some((text, prompt)) = talk::line(e, choice) {
                        let name = talk::name_of(e, v.npc.0);
                        self.queue_subtitles(messages(
                            v.initiator,
                            name.as_deref(),
                            &text,
                            &prompt,
                        ));
                        self.lines_far_shown += 1;
                    }
                    f
                };
                self.far = Some(f);
            }
        }
        out.extend(client.line_done());
    }

    /// This player's own conversation.
    fn drive(&mut self, e: &mut Engine, client: &mut DialogueClient, out: &mut Vec<DialogueMsg>) {
        // An open asked for: granted, refused, or still on its way.
        if let Some((req, _)) = self.pending {
            if client.is_initiator() {
                self.pending = None;
                self.opened += 1;
            } else if client.denied.is_some_and(|(r, _)| r == req) {
                self.pending = None;
                self.denied += 1;
                self.queue.clear();
                talk::close_menu(e);
                talk::show(e, "They are talking to another player.");
            } else {
                return;
            }
        }
        if !client.is_initiator() {
            self.awaiting = None;
            self.closing = false;
            // The first shared line of a conversation asks the host for the NPC.
            if let Some(first) = self.queue.front() {
                let (req, msg) = client.open(FormId(first.npc));
                self.pending = Some((req, first.npc));
                out.push(msg);
            }
            return;
        }
        let Some(v) = client.view().copied() else {
            return;
        };
        if self.closing {
            // The close is on its way: what is said now belongs to the next conversation.
            return;
        }
        if let Some(step) = self.awaiting {
            if v.step < step {
                return;
            }
            self.awaiting = None;
        }
        while let Some(next) = self.queue.front().copied() {
            if next.npc != v.npc.0 {
                // A line of a conversation that is over.
                self.queue.pop_front();
                continue;
            }
            match (next.end, v.playing) {
                // The line ends: every machine may finish it.
                (true, Some(choice)) if choice == next.info => {
                    self.queue.pop_front();
                    out.extend(client.line_done());
                    return;
                }
                // An end with no line playing, or of another line: nothing to finish.
                (true, _) => {
                    self.queue.pop_front();
                }
                // The next line while the last still plays: the last is done first.
                (false, Some(_)) => {
                    out.extend(client.line_done());
                    return;
                }
                (false, None) => {
                    let Some(msg) = client.pick(next.info) else {
                        return;
                    };
                    self.queue.pop_front();
                    self.awaiting = Some(v.step + 1);
                    self.picks += 1;
                    out.push(msg);
                    return;
                }
            }
        }
        // Nothing left to say and the menu is closed: the conversation is over.
        if talk::menu_npc(e).is_none() {
            if v.playing.is_some() {
                out.extend(client.line_done());
            } else if !self.closing {
                self.closing = true;
                out.extend(client.close());
            }
        }
    }
}

/// The host's game runs the results of the guests' lines.
#[derive(Debug, Default)]
pub struct Enactor {
    initiators: BTreeMap<u32, PlayerId>,
    /// Results the host's game ran for a guest.
    pub enacted: u32,
    /// Results left alone: not a line of the shared story, or the NPC is not in the host's game.
    pub left: u32,
    /// Why, for the log (taken by the caller).
    pub notes: Vec<String>,
}

impl Enactor {
    /// Takes one of the dialogue table's events. `shared` are the shared story's quests.
    pub fn on_event(&mut self, e: &mut Engine, ev: DialogueEvent, shared: &BTreeSet<u32>) {
        let (session, npc, choice, end) = match ev {
            DialogueEvent::RunBegin {
                session,
                npc,
                choice,
                initiator,
                ..
            } => {
                self.initiators.insert(session, initiator);
                (session, npc, choice, false)
            }
            DialogueEvent::RunEnd {
                session,
                npc,
                choice,
                ..
            } => (session, npc, choice, true),
            DialogueEvent::Closed { session, .. } => {
                self.initiators.remove(&session);
                return;
            }
        };
        // The host's own lines ran in its game when it said them.
        let from = match self.initiators.get(&session) {
            Some(p) if !p.is_host() => *p,
            _ => return,
        };
        let quest = talk::info_quest(e, choice);
        if !quest.is_some_and(|q| q != 0 && shared.contains(&q)) {
            self.left += 1;
            self.notes.push(format!(
                "player {}'s line {choice:08x} is not a line of the shared story: not run here",
                from.0
            ));
            return;
        }
        // companions: a hire in this line is that player's.
        let enacted =
            coop_engine::companion::with_hirer(e, from.0, |e| talk::enact(e, npc.0, choice, end));
        match enacted {
            Ok(()) => self.enacted += 1,
            Err(why) => {
                self.left += 1;
                self.notes.push(format!(
                    "player {}'s line {choice:08x} to {:08x} could not be run here: {why:?}",
                    from.0, npc.0
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_watched_line_is_the_prompt_then_each_response() {
        assert_eq!(
            messages(
                PlayerId(1),
                None,
                "Howdy. | Welcome to Goodsprings.",
                "Hello."
            ),
            ["Player 2: Hello.", "Howdy.", "Welcome to Goodsprings."]
        );
        assert_eq!(
            messages(PlayerId(1), Some("Sunny Smiles"), "Howdy. | ", "Hello."),
            ["Player 2: Hello.", "Sunny Smiles: Howdy."]
        );
        assert_eq!(messages(PlayerId(0), None, "Howdy.", ""), ["Howdy."]);
        assert!(messages(PlayerId(0), Some("Sunny"), "", "").is_empty());
    }

    #[test]
    fn at_most_so_many_subtitles_wait_the_oldest_dropped() {
        let mut t = Talker::default();
        t.queue_subtitles(
            (0..SUBTITLES_QUEUED_MAX + 8)
                .map(|n| n.to_string())
                .collect(),
        );
        assert_eq!(t.subtitles.len(), SUBTITLES_QUEUED_MAX);
        assert_eq!(t.subtitles.front().map(String::as_str), Some("8"));
    }

    #[test]
    fn a_long_line_is_cut_at_spaces_into_subtitles() {
        let word = "gecko ".repeat(100);
        let long = format!("Sunny: {}", word.trim());
        let parts = talk::subtitle_parts(&long);
        assert!(parts.len() > 1);
        assert!(parts
            .iter()
            .all(|p| p.chars().count() <= coop_engine::addr::SUBTITLE_MAX));
        assert!(parts
            .iter()
            .all(|p| !p.starts_with(' ') && !p.ends_with(' ')));
        assert_eq!(parts.join(" "), long);
        // A word longer than a subtitle: cut inside it.
        let one = "é".repeat(coop_engine::addr::SUBTITLE_MAX + 5);
        let parts = talk::subtitle_parts(&one);
        assert_eq!(
            parts.iter().map(|p| p.chars().count()).collect::<Vec<_>>(),
            [coop_engine::addr::SUBTITLE_MAX, 5]
        );
        assert_eq!(talk::subtitle_parts("  Howdy.  "), ["Howdy."]);
        assert!(talk::subtitle_parts("   ").is_empty());
    }
}
