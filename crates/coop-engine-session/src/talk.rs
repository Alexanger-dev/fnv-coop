//! Conversations in a session (docs/coop/SPECS.md S16): coop-core's dialogue protocol driven by
//! the game's own dialogue menu.
//!
//! - [`Talker`] runs on every machine. What this game's own conversation says
//!   (`coop_engine::talk`'s events) becomes the protocol's moves: the first shared line asks the
//!   host for the NPC (`Open`), each line said is a `Pick` of its info, its end a `LineDone`, the
//!   menu closing a `Close`. A player takes part only in a conversation it started or joined
//!   (S42, D12): talking to the NPC of another player's conversation joins it (the NPC is named
//!   busy to the game, which opens no menu on it, and the refusal becomes a `Join`). A machine
//!   in another player's conversation shows each line picked in the game's own dialogue window
//!   on its NPC when its player is in the NPC's cell (`coop_engine::watch`, S42), else as
//!   subtitles; a player not in it follows it as subtitles (held back while busy), with a word
//!   that it may join. Each window of a conversation another player is in too ends its list with
//!   `[Leave conversation]`: this player is out (its window closed, the table told), and the
//!   conversation goes on for the others; closing the window does the same. A machine reports
//!   another player's line done at once, so the talker never waits for a watcher. The window
//!   closes when the conversation ends, the player leaves it or the NPC's cell, or nothing new
//!   is said for [`WATCH_QUIET_STEPS`]; a conversation the host refuses is closed through the
//!   game.
//! - [`Enactor`] runs on the host: a guest's pick of a line of the shared story has its result
//!   run in the host's game, once, when the protocol says so (the begin result at the pick, the
//!   end result once every machine has reported the line done).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use coop_core::dialogue::{Busy, DialogueClient, DialogueEvent, DialogueMsg};
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
    /// Other players' conversations this player joined (it talked to the NPC, S42 D12).
    pub joins: u32,
    /// Conversations this player left (`[Leave conversation]`, or it closed the window).
    pub leaves: u32,
    /// "... is talking to ... Talk to them to join." shown to a player near a conversation it is
    /// not in.
    pub join_hints: u32,
    /// Windows opened on a conversation's last line for a player who joined between two steps.
    pub join_windows: u32,
    /// The conversation whose join hint was shown.
    hinted: Option<u32>,
    /// The NPC this player talked to, to join its conversation, until the table took it in.
    joining: Option<u32>,
    /// The other players' avatars in this game (player, reference id), set by the caller each
    /// step: a conversation near one that is busy waits for it (D13).
    pub avatars: Vec<(PlayerId, u32)>,
    /// The NPC this player talked to that waits for a busy player near it (D13).
    waiting: Option<u32>,
    /// The conversation this player left (`[Leave conversation]`): it is not pulled in again.
    left_session: Option<u32>,
    /// A conversation this player is being pulled into (the `Join` on its way).
    pulling: Option<u32>,
    /// Players pulled into a conversation near them (D13).
    pub pulled_in: u32,
    /// Conversations this player asked for that waited for a busy player, and those started
    /// once that player was free.
    pub waits: u32,
    /// See [`Self::waits`].
    pub waits_started: u32,
    /// The time, set by the caller each step (the dialogue client's clock): a vote's seconds left.
    pub now_ms: u64,
    /// Votes this player cast (S42 step 3b), and the options chosen by votes in its
    /// conversations.
    pub votes_cast: u32,
    /// See [`Self::votes_cast`].
    pub votes_chosen: u32,
    /// The vote this player, not in the conversation, was told of: (session, step).
    vote_told: Option<(u32, u32)>,
    /// The step whose last line was shown to a player who joined: (session, step).
    join_shown: Option<(u32, u32)>,
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
    /// A new line came: what is still on screen of the lines before it is ended first.
    subtitle_new_line: bool,
    /// The subtitles of this game's on the HUD that may still be showing (their texts).
    subtitles_up: Vec<String>,
    /// When the last line shown as a HUD message has had its time (ms).
    message_until_ms: u64,
    /// Subtitles ended early, a new line come (keeping pace with the player talking).
    pub subtitles_cut: u32,
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
    /// Clicks on another player's option, sent to the table for its owner to say.
    pub foreign_clicks: u32,
    /// The step this player's game picked last (session, step): its line is not shown again.
    my_pick: Option<(u32, u32)>,
    /// The step another player picked whose line was put to this player's own window.
    injected: Option<(u32, u32)>,
    /// Lines of other players' picks put to this player's own conversation window.
    pub lines_injected: u32,
    /// Options of this player's another player picked, said here (and those that could not be).
    pub asks_said: u32,
    /// Options of this player's another player picked that this game could not say.
    pub asks_failed: u32,
    /// The story's revision in this game (a guest's: the host's changes it has), set by the
    /// caller each step.
    pub story_rev: u64,
    /// The step after which this player's window waits to list its options (session, step, the
    /// story's revision then, steps waited).
    settle: Option<(u32, u32, u64, u32)>,
    /// Windows closed because another player's line ended the conversation (its goodbye mark).
    pub goodbye_closes: u32,
    /// Conversations this player's line ended in its game, told to the table.
    pub ends_told: u32,
    /// This player's own windows closed when another player's line ended its conversation.
    pub own_windows_closed: u32,
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
    p.label()
}

/// Line `choice`'s response text and the option the player chose: its prompt, or without one its
/// topic's name, as the option list shows it (the owner, 2026-11-22: "I wanted to ask you some
/// questions." is a topic's name, and its subtitle was missing).
fn line_text(e: &mut Engine, choice: u32) -> Option<(String, String)> {
    let (text, prompt) = talk::line(e, choice)?;
    let prompt = coop_engine::choices::option_text(e, choice).unwrap_or(prompt);
    Some((text, prompt))
}

/// A watching window whose conversation has said nothing new for this many steps (frames) is
/// closed: about five minutes at 60 frames a second (a talker may read a long list of topics).
pub const WATCH_QUIET_STEPS: u32 = 18_000;

/// How a watched line is shown as text: what the player who picked it chose (named), when the info
/// has a prompt, then
/// each of the NPC's responses (after the NPC's name, when it has one), each as a subtitle of its
/// own ([`talk::subtitle_parts`]: a long one in several).
fn messages(picker: PlayerId, npc: Option<&str>, text: &str, prompt: &str) -> Vec<String> {
    let mut out = Vec::new();
    if !prompt.is_empty() {
        out.extend(talk::subtitle_parts(&format!(
            "{}: {prompt}",
            player_name(picker)
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

/// How many steps (frames) a guest's window waits, after another player's line, for that line's
/// changes to the story (its quest's stage) before it lists its options anyway: the line may have
/// changed nothing.
pub const SETTLE_STEPS: u32 = 60;

/// How long a line shown as a HUD message is up (`talk::show`'s four seconds).
pub const MESSAGE_MS: u64 = 4000;
/// Subtitles waiting to be shown at most: more drops the oldest.
pub const SUBTITLES_QUEUED_MAX: usize = 32;
/// How many steps (frames) a waiting subtitle waits for the HUD's own (an NPC's nearby) to end
/// before it is shown anyway.
pub const SUBTITLE_WAIT_STEPS: u32 = 600;

impl Talker {
    /// A line said in the conversation, as subtitles: it takes the place of what is left of the
    /// lines before it (the owner, 2026-11-22: each subtitle shown its full reading time, one after
    /// the other, fell two lines behind the voice in the talker's game). Kept pace with the player
    /// talking: at most one line behind it.
    fn queue_subtitles(&mut self, lines: Vec<String>) {
        self.subtitles.clear();
        self.subtitles.extend(lines);
        while self.subtitles.len() > SUBTITLES_QUEUED_MAX {
            self.subtitles.pop_front();
        }
        self.subtitle_new_line = true;
    }

    /// Shows the next waiting subtitle once the HUD's last has ended (the parts of one line each
    /// their full time: the game counts a subtitle's time from when it is queued), not while the
    /// player is busy. A new line first ends this game's subtitles of the lines before it. Where
    /// the game shows no subtitles, as a HUD message, each once the last has had its time.
    fn show_subtitles(&mut self, e: &mut Engine) {
        if self.subtitles.is_empty() || talk::busy(e) {
            return;
        }
        if self.subtitle_new_line {
            self.subtitle_new_line = false;
            let up = std::mem::take(&mut self.subtitles_up);
            self.subtitles_cut += talk::end_subtitles(e, &up);
            self.subtitle_waited = 0;
            self.message_until_ms = 0;
        }
        if talk::subtitle_showing(e) && self.subtitle_waited < SUBTITLE_WAIT_STEPS {
            self.subtitle_waited += 1;
            return;
        }
        if self.now_ms < self.message_until_ms {
            return;
        }
        self.subtitle_waited = 0;
        let Some(line) = self.subtitles.pop_front() else {
            return;
        };
        if talk::subtitle(e, &line) {
            self.subtitles_shown += 1;
            self.subtitles_up.push(line);
        } else {
            talk::show(e, &line);
            self.subtitles_as_messages += 1;
            self.message_until_ms = self.now_ms + MESSAGE_MS;
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
                // Talking to the NPC of another player's conversation joins it (S42, D12); the
                // NPC's own greeting does not (an actor came to greet this player while busy).
                Event::Asked { npc } => {
                    if let Some(msg) = client.join(FormId(npc)) {
                        coop_engine::watch::forget_left(e);
                        out.push(msg);
                        self.joins += 1;
                        self.joining = Some(npc);
                    }
                }
                // The menu the game then asked for is refused, as joining shows the
                // conversation in its own window.
                Event::Blocked { npc } if self.joining == Some(npc) => {}
                // D13: another player near the NPC is busy: the conversation waits for it.
                Event::Held { npc } => {
                    self.waiting = Some(npc);
                    self.waits += 1;
                    let cell = talk::cell_of(e, npc);
                    let who = self.busy_near(e, client, cell);
                    if let Some((who, busy)) = who {
                        let doing = if busy == Busy::Combat {
                            "in battle"
                        } else {
                            "in a menu"
                        };
                        talk::show(
                            e,
                            &format!("{} is currently {doing}. Please wait.", player_name(who)),
                        );
                    }
                    out.push(client.waiting(FormId(npc)));
                }
                Event::Blocked { .. } => {
                    self.blocked += 1;
                    talk::show(e, "Another player is talking to them.");
                }
            }
        }
        // A join the table took in (or a conversation that ended first): done.
        if let Some(npc) = self.joining {
            if client.join(FormId(npc)).is_none() {
                self.joining = None;
            }
        }
        // The NPCs of the conversations this player is not in get no menu here (talking to one
        // joins it). (In this player's opening the game refuses no NPC: `talk::set_own_opening`.)
        talk::set_busy(e, client.busy_npcs().into_iter().map(|n| n.0));
        self.leave(e, client, &mut out);
        self.vote(e, client, &mut out);
        self.share(e, me, client, &mut out);
        self.watch(e, me, client, &mut out);
        self.drive(e, client, &mut out);
        self.options(e, me, client, &mut out);
        out
    }

    /// S42 step 3b: a click on a line that matters is this player's vote; the votes so far show
    /// after the rows' text ("(1/2 Votes, 18s)"), the choice is told; a player not in the
    /// conversation is told once that a vote is open and how to take part.
    fn vote(&mut self, e: &mut Engine, client: &mut DialogueClient, out: &mut Vec<DialogueMsg>) {
        if let Some(info) = coop_engine::choices::take_voted(e) {
            if let Some(msg) = client.vote(info) {
                out.push(msg);
                self.votes_cast += 1;
            }
        }
        for choice in client.take_chosen() {
            self.votes_chosen += 1;
            if client.joined() {
                let text = coop_engine::choices::option_text(e, choice).unwrap_or_default();
                talk::show(e, &format!("Chosen: {text}"));
            }
        }
        let view = client.view().copied();
        let votes = client
            .votes(self.now_ms)
            .map(|(v, of, left)| (v.to_vec(), of, left));
        let mut marks = Vec::new();
        if let (Some(v), Some((votes, of, left))) = (view, votes) {
            let secs = left.div_ceil(1000);
            if client.joined() {
                for &(_, choice) in &votes {
                    if marks.iter().any(|(c, _)| *c == choice) {
                        continue;
                    }
                    let n = votes.iter().filter(|(_, c)| *c == choice).count();
                    marks.push((choice, format!("({n}/{of} Votes, {secs}s)")));
                }
            } else if let Some(&(who, choice)) = votes
                .first()
                .filter(|_| self.vote_told != Some((v.session, v.step)))
            {
                self.vote_told = Some((v.session, v.step));
                let text = coop_engine::choices::option_text(e, choice).unwrap_or_default();
                let npc = talk::name_of(e, v.npc.0).unwrap_or_else(|| "them".into());
                talk::show(
                    e,
                    &format!(
                        "{} voted for \"{text}\". Talk to {npc} to vote ({secs}s).",
                        player_name(who)
                    ),
                );
            }
        }
        coop_engine::choices::set_marks(e, marks);
    }

    /// The other player busy (in a menu, fighting) whose avatar is in cell `cell`, if any.
    fn busy_near(
        &self,
        e: &mut Engine,
        client: &DialogueClient,
        cell: u32,
    ) -> Option<(PlayerId, Busy)> {
        self.avatars.iter().find_map(|&(p, id)| {
            let busy = client.busy_of(p);
            (matches!(busy, Busy::Menu | Busy::Combat) && cell != 0 && talk::cell_of(e, id) == cell)
                .then_some((p, busy))
        })
    }

    /// D13 (the owner, 2026-11-22): every player near a conversation hears it together. This
    /// player's doing is told to the others; a conversation this player asks for near a busy
    /// player waits for it, and starts once it is free (or is given up when this player walks
    /// away); a conversation near this player, free and not left by it, takes it in.
    fn share(
        &mut self,
        e: &mut Engine,
        me: PlayerId,
        client: &mut DialogueClient,
        out: &mut Vec<DialogueMsg>,
    ) {
        let busy = if client.joined() || talk::menu_npc(e).is_some() {
            Busy::Talking
        } else if talk::in_combat(e) {
            Busy::Combat
        } else if talk::in_menu(e) {
            Busy::Menu
        } else {
            Busy::Free
        };
        out.extend(client.set_busy(busy));
        // The cells where another player is busy: talking to an actor there waits.
        let mut cells = Vec::new();
        for &(p, id) in &self.avatars {
            if p != me && matches!(client.busy_of(p), Busy::Menu | Busy::Combat) {
                cells.push(talk::cell_of(e, id));
            }
        }
        talk::set_hold(e, cells.into_iter().filter(|c| *c != 0));
        // Another player waits to talk near this busy player: told who.
        for (who, npc) in client.take_waiting() {
            if talk::busy(e) && talk::near(e, npc.0) {
                let name = talk::name_of(e, npc.0).unwrap_or_else(|| "someone".into());
                talk::show(e, &format!("{} wants to talk to {name}.", player_name(who)));
            }
        }
        // A held conversation: given up when this player walked away, started once no busy
        // player is near it.
        if let Some(npc) = self.waiting {
            if !talk::near(e, npc) {
                self.waiting = None;
            } else if !talk::busy(e) && {
                let cell = talk::cell_of(e, npc);
                self.busy_near(e, client, cell).is_none()
            } {
                self.waiting = None;
                if talk::activate(e, npc) {
                    self.waits_started += 1;
                }
            }
        }
        // A conversation near this player: taken in when free, unless it left that one.
        let Some(v) = client.view().copied() else {
            self.pulling = None;
            return;
        };
        if client.joined() {
            self.pulling = None;
            return;
        }
        // The `Join` that pulls it in is on its way: not asked again.
        if let Some(s) = self.pulling {
            self.pulling = (s == v.session).then_some(s);
            return;
        }
        if self.left_session != Some(v.session)
            && !talk::busy(e)
            && talk::near(e, v.npc.0)
            && !self.opening.is_some_and(|(quest, stage)| {
                coop_engine::quest::record(e, quest).is_some_and(|r| !r.done.contains(&stage))
            })
        {
            if let Some(msg) = client.join(v.npc) {
                coop_engine::watch::forget_left(e);
                out.push(msg);
                self.pulled_in += 1;
                self.pulling = Some(v.session);
            }
        }
    }

    /// S42, D12: this player out of its conversation, by the `[Leave conversation]` row or by
    /// closing the watching window: its window closed, the table told; the conversation goes on
    /// for the others. The row is in each window of a conversation another player is in too (not
    /// for a player alone in it: its goodbye ends it).
    fn leave(&mut self, e: &mut Engine, client: &mut DialogueClient, out: &mut Vec<DialogueMsg>) {
        let clicked = coop_engine::choices::take_leave(e);
        let closed = coop_engine::watch::take_left(e);
        if (clicked || closed) && client.joined() {
            let own = client.is_initiator();
            let npc = client.view().map(|v| v.npc.0);
            self.left_session = client.view().map(|v| v.session);
            if let Some(msg) = client.leave() {
                out.push(msg);
                self.leaves += 1;
            }
            if clicked {
                if own {
                    coop_engine::watch::leave_own_window(e);
                } else {
                    coop_engine::watch::end_watch(e);
                }
            }
            self.watched = None;
            // Nothing more of it is this player's to say.
            self.queue.retain(|q| Some(q.npc) != npc);
            self.awaiting = None;
        }
        let row = client.joined() && client.players_in().len() > 1;
        coop_engine::choices::set_leave(e, row);
    }

    /// Whether the story is settled after step `v`'s line, for this player's window to list its
    /// options: on the host, a step later (the host's game ran the line's result for a guest by
    /// then); on a guest, once a change of the host's story came after the step ended, or after
    /// [`SETTLE_STEPS`].
    fn settled(&mut self, me: PlayerId, v: coop_core::dialogue::View) -> bool {
        let key = (v.session, v.step);
        let (rev, waited) = match self.settle {
            Some((s, step, rev, n)) if (s, step) == key => (rev, n + 1),
            _ => (self.story_rev, 0),
        };
        self.settle = Some((key.0, key.1, rev, waited));
        if me.is_host() {
            waited >= 1
        } else {
            self.story_rev > rev || waited >= SETTLE_STEPS
        }
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
        // Another player's option clicked: the table asks its owner to say it (S42 step 2c).
        if let Some(info) = coop_engine::choices::take_clicked(e) {
            if let Some(msg) = client.want(info) {
                out.push(msg);
                self.foreign_clicks += 1;
            }
        }
        // An option of this player's another player picked: said here as a click on its row.
        if let Some(asked) = client.take_asked() {
            if coop_engine::choices::say(e, asked.choice) {
                self.asks_said += 1;
            } else {
                self.asks_failed += 1;
            }
        }
        // This player's line ended the conversation in its game (the window it drove closed): the
        // table ends it for every player.
        if coop_engine::watch::take_ended(e) {
            if let Some(msg) = client.end() {
                out.push(msg);
                self.ends_told += 1;
            }
        }
        let view = client.view().copied();
        // The conversation is over while this player's own window holds on another player's
        // line: that line ended it in the other game.
        if view.map_or(true, |v| v.initiator != me)
            && coop_engine::watch::holds_injected(e)
            && coop_engine::watch::close_own_window(e)
        {
            self.own_windows_closed += 1;
        }
        // Another player's pick in this player's own conversation: its line, said in that
        // player's game, is put to this player's window, and done here.
        if let Some(v) = view.filter(|v| v.initiator == me && !v.reported) {
            let step = (v.session, v.step);
            if let Some(choice) = v
                .playing
                .filter(|_| self.my_pick != Some(step) && self.injected != Some(step))
            {
                self.injected = Some(step);
                if talk::mark_said(e, choice) {
                    self.lines_marked += 1;
                }
                if coop_engine::watch::inject(e, choice) {
                    self.lines_injected += 1;
                }
                out.extend(client.line_done());
            }
        }
        let joined = client.joined();
        let mine = view.filter(|v| {
            if !joined {
                false
            } else if v.initiator == me {
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
        // Another player's line that ends the conversation: the window closes, as that player's
        // game closed its own (PC run 37).
        if coop_engine::watch::close_on_goodbye(e) {
            self.goodbye_closes += 1;
            return;
        }
        // The watching window, once the table has the step's list; this player's own window
        // holding on another player's line, once that step is over (S42 step 2c). Either only
        // once the line's changes to the story are in this game (PC run 37: a list built before
        // offered the options of the story as it was).
        let list = if v.initiator == me {
            coop_engine::watch::holds_injected(e)
        } else {
            client.menu().is_some()
        };
        if list && self.settled(me, v) && coop_engine::watch::show_topics(e) {
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
        // Another player's conversation, or this player's own it left.
        let joined = client.joined();
        let view = client
            .view()
            .copied()
            .filter(|v| v.initiator != me || !joined);
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
                if std::env::var_os("NV_COOP_TALK_LOG").is_some() {
                    let near = view.map(|v| talk::near(e, v.npc.0));
                    eprintln!(
                        "co-op talk: window on conversation {session} closed: now {:?}, near {near:?}, quiet {quiet}, last ended {:?}",
                        view.map(|v| v.session),
                        client.last_closed
                    );
                }
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
        // Joined mid-line (the line already shown here as text) or between two steps: the window
        // opens at once on the line being said, from its start, or on the last line, then lists
        // the options (the owner, 2026-11-22: talking to the NPC did nothing until its line
        // ended).
        if joined
            && self.watched.is_none()
            && (v.playing.is_none() || self.shown == Some((v.session, v.step)))
            && self.join_shown != Some((v.session, v.step))
            && talk::near(e, v.npc.0)
        {
            if let Some(last) = v.playing.or(client.last_choice()) {
                self.join_shown = Some((v.session, v.step));
                if coop_engine::watch::watch_line(e, v.npc.0, last)
                    == coop_engine::watch::Outcome::Window
                {
                    self.watched = Some((v.session, 0));
                    self.join_windows += 1;
                }
            }
        }
        let Some(choice) = v.playing.filter(|_| !v.reported) else {
            return;
        };
        // The step is this player's own pick (S42 step 2c: a click in the window): its game
        // said the line, and reports it done when it ends.
        if self.my_pick == Some((v.session, v.step)) {
            return;
        }
        if self.shown != Some((v.session, v.step)) {
            self.shown = Some((v.session, v.step));
            // Said in the story once, for every player: a line the game says once is not said
            // again to this player.
            if talk::mark_said(e, choice) {
                self.lines_marked += 1;
            }
            let near = talk::near(e, v.npc.0);
            let window = joined
                && near
                && coop_engine::watch::watch_line(e, v.npc.0, choice)
                    == coop_engine::watch::Outcome::Window;
            // Near a conversation this player is not in: a word that it may join, once.
            if !joined && near && self.hinted != Some(v.session) && !talk::busy(e) {
                self.hinted = Some(v.session);
                let who = client.players_in().first().copied().unwrap_or(v.initiator);
                let name = talk::name_of(e, v.npc.0).unwrap_or_else(|| "someone".into());
                talk::show(
                    e,
                    &format!(
                        "{} is talking to {name}. Talk to them to join.",
                        player_name(who)
                    ),
                );
                self.join_hints += 1;
            }
            if window {
                self.watched = Some((v.session, 0));
                self.lines_windowed += 1;
            } else if joined && near {
                if let Some((text, prompt)) = line_text(e, choice) {
                    let name = talk::name_of(e, v.npc.0);
                    self.queue_subtitles(messages(
                        v.picker.unwrap_or(v.initiator),
                        name.as_deref(),
                        &text,
                        &prompt,
                    ));
                    self.lines_shown += 1;
                }
            } else {
                // Far from the NPC (S42, the owner's choice of 2026-11-21): the line as text, or,
                // while this player is busy, held back and a word that the conversation goes on.
                self.lines_far += 1;
                // Named after the player in it (the starter may have left it).
                let who = client.players_in().first().copied().unwrap_or(v.initiator);
                let f = self.far.filter(|f| f.session == v.session).unwrap_or(Far {
                    session: v.session,
                    initiator: who,
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
                            &format!("{} is talking to {name}.", player_name(f.initiator)),
                        );
                        self.far_notices += 1;
                    }
                    Far {
                        told: true,
                        held: true,
                        ..f
                    }
                } else {
                    if let Some((text, prompt)) = line_text(e, choice) {
                        let name = talk::name_of(e, v.npc.0);
                        self.queue_subtitles(messages(
                            v.picker.unwrap_or(v.initiator),
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
        // A line this player said in the window showing another player's conversation (a click
        // on an option of its own, S42 step 2c) is the step's pick, as the initiator's lines are.
        if let Some(v) = client
            .view()
            .copied()
            .filter(|_| client.is_spectator())
            .filter(|v| self.queue.front().is_some_and(|q| q.npc == v.npc.0))
        {
            self.closing = false;
            self.say_lines(client, v, out);
            return;
        }
        if !(client.is_initiator() && client.joined()) {
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
        if !self.say_lines(client, v, out) {
            return;
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

    /// The lines this game said in the conversation `v`: each the step's pick, each one's end
    /// that step's lines done. False when a message went out or a step is awaited (nothing more
    /// this step).
    fn say_lines(
        &mut self,
        client: &mut DialogueClient,
        v: coop_core::dialogue::View,
        out: &mut Vec<DialogueMsg>,
    ) -> bool {
        if let Some(step) = self.awaiting {
            if v.step < step {
                return false;
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
                    return false;
                }
                // An end with no line playing, or of another line: nothing to finish.
                (true, _) => {
                    self.queue.pop_front();
                }
                // The next line while the last still plays: the last is done first.
                (false, Some(_)) => {
                    out.extend(client.line_done());
                    return false;
                }
                (false, None) => {
                    let Some(msg) = client.pick(next.info) else {
                        return false;
                    };
                    self.queue.pop_front();
                    self.awaiting = Some(v.step + 1);
                    self.my_pick = Some((v.session, v.step + 1));
                    self.picks += 1;
                    out.push(msg);
                    return false;
                }
            }
        }
        true
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
            ["Guest: Hello.", "Howdy.", "Welcome to Goodsprings."]
        );
        assert_eq!(
            messages(PlayerId(1), Some("Sunny Smiles"), "Howdy. | ", "Hello."),
            ["Guest: Hello.", "Sunny Smiles: Howdy."]
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
