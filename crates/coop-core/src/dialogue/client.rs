//! Guest side of the dialogue protocol: what the dialogue UI needs to know.

use super::{Busy, CloseReason, Deny, DialogueMsg, MenuItem, HEARTBEAT_MS};
use crate::ids::{FormId, PlayerId};

/// The session this machine currently sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct View {
    /// Session id.
    pub session: u32,
    /// The NPC.
    pub npc: FormId,
    /// Who picks.
    pub initiator: PlayerId,
    /// Last step picked.
    pub step: u32,
    /// Choice of the step being played (None between steps).
    pub playing: Option<u32>,
    /// The player who picked the last step.
    pub picker: Option<PlayerId>,
    /// True once this machine reported its lines done for the current step.
    pub reported: bool,
}

/// The table asks this player's game to say one of its options, picked by another player.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Asked {
    /// Session.
    pub session: u32,
    /// Step.
    pub step: u32,
    /// The option (an info).
    pub choice: u32,
    /// Who picked it.
    pub by: PlayerId,
}

/// What this machine knows of one open conversation.
#[derive(Clone, Debug)]
struct Known {
    view: View,
    /// The players in it (S42, D12), the initiator first while it is in.
    joined: Vec<PlayerId>,
    /// Its merged options: (step, options).
    menu: Option<(u32, Vec<MenuItem>)>,
    /// The info of its last step picked.
    last: Option<u32>,
}

/// Dialogue state for one machine.
///
/// It follows every open conversation; the one it shows ([`Self::view`]) is the one its player
/// is in, else the newest (S42, D12: a player takes part only in a conversation it started or
/// joined, and follows the others as text).
#[derive(Clone, Debug)]
pub struct DialogueClient {
    me: PlayerId,
    next_req: u32,
    sessions: std::collections::BTreeMap<u32, Known>,
    last_beat: u64,
    /// Newest denial of one of our opens.
    pub denied: Option<(u32, Deny)>,
    /// How the last session ended.
    pub last_closed: Option<(u32, CloseReason)>,
    /// Picks of ours the host refused, and the newest refused choice.
    pub refused: u32,
    /// The newest refused pick: (session, choice).
    pub last_refused: Option<(u32, u32)>,
    /// An option of ours another player picked, for this game to say (only for the shown
    /// conversation's next step: [`Self::take_asked`]).
    asked: Option<Asked>,
    /// What each other player is doing (D13), as the host last told.
    busy: std::collections::BTreeMap<PlayerId, Busy>,
    /// What this player is doing, as last told to the host.
    told_busy: Busy,
    /// Players waiting to talk to an NPC, newest last (the host's word), not yet taken.
    waiting: Vec<(PlayerId, FormId)>,
}

impl DialogueClient {
    /// A client for player `me`.
    pub fn new(me: PlayerId) -> Self {
        DialogueClient {
            me,
            next_req: 1,
            sessions: Default::default(),
            last_beat: 0,
            denied: None,
            last_closed: None,
            refused: 0,
            last_refused: None,
            asked: None,
            busy: Default::default(),
            told_busy: Busy::Free,
            waiting: Vec::new(),
        }
    }

    /// What this player is doing, for the host when it changed (D13).
    pub fn set_busy(&mut self, busy: Busy) -> Option<DialogueMsg> {
        (std::mem::replace(&mut self.told_busy, busy) != busy).then_some(DialogueMsg::Busy { busy })
    }

    /// What another player is doing, as the host last told (free when never told).
    pub fn busy_of(&self, p: PlayerId) -> Busy {
        self.busy.get(&p).copied().unwrap_or_default()
    }

    /// This player waits to talk to `npc` for a busy player near it: the host tells the others.
    pub fn waiting(&self, npc: FormId) -> DialogueMsg {
        DialogueMsg::Waiting { npc }
    }

    /// The players who started waiting to talk to an NPC since the last call (taken once).
    pub fn take_waiting(&mut self) -> Vec<(PlayerId, FormId)> {
        std::mem::take(&mut self.waiting)
    }

    /// The conversation shown: the one this player is in, else the newest.
    fn current(&self) -> Option<&Known> {
        self.sessions
            .values()
            .find(|k| k.joined.contains(&self.me))
            .or_else(|| self.sessions.values().next_back())
    }

    fn current_mut(&mut self) -> Option<&mut Known> {
        let me = self.me;
        let id = self
            .sessions
            .iter()
            .find(|(_, k)| k.joined.contains(&me))
            .or_else(|| self.sessions.iter().next_back())
            .map(|(id, _)| *id)?;
        self.sessions.get_mut(&id)
    }

    /// The conversation shown, when this player is in it.
    fn mine(&self) -> Option<&Known> {
        self.current().filter(|k| k.joined.contains(&self.me))
    }

    /// The conversation's merged options for the next step, when the table sent them.
    pub fn menu(&self) -> Option<&[MenuItem]> {
        let k = self.current()?;
        k.menu
            .as_ref()
            .filter(|(step, _)| *step == k.view.step + 1 && k.view.playing.is_none())
            .map(|(_, items)| items.as_slice())
    }

    /// An option of ours another player picked, for this game to say (taken once).
    pub fn take_asked(&mut self) -> Option<Asked> {
        let v = self.current()?.view;
        self.asked
            .take()
            .filter(|a| a.session == v.session && a.step == v.step + 1 && v.playing.is_none())
    }

    /// This player's options for the next step (S42 step 2): the infos its player may say.
    pub fn options(&self, choices: Vec<u32>) -> Option<DialogueMsg> {
        let v = self.view().filter(|v| v.playing.is_none())?;
        Some(DialogueMsg::Options {
            session: v.session,
            step: v.step + 1,
            choices,
        })
    }

    /// Picks another player's option of the merged list: the table asks its owner to say it.
    pub fn want(&self, choice: u32) -> Option<DialogueMsg> {
        let v = self.view().filter(|v| v.playing.is_none())?;
        Some(DialogueMsg::Want {
            session: v.session,
            step: v.step + 1,
            choice,
        })
    }

    /// The conversation shown, if any.
    pub fn view(&self) -> Option<&View> {
        self.current().map(|k| &k.view)
    }

    /// Whether this player is in the conversation shown (it started or joined it, and has not
    /// left).
    pub fn joined(&self) -> bool {
        self.mine().is_some()
    }

    /// The players in the conversation shown.
    pub fn players_in(&self) -> &[PlayerId] {
        self.current().map_or(&[], |k| k.joined.as_slice())
    }

    /// The info of the shown conversation's last step picked (a player who joins between two
    /// steps is shown it first).
    pub fn last_choice(&self) -> Option<u32> {
        self.current()?.last
    }

    /// The NPCs of the conversations this player is not in: busy to it, and talking to one joins
    /// its conversation.
    pub fn busy_npcs(&self) -> Vec<FormId> {
        self.sessions
            .values()
            .filter(|k| !k.joined.contains(&self.me))
            .map(|k| k.view.npc)
            .collect()
    }

    /// Joins the conversation with `npc` this player is not in (it talked to the NPC), when it
    /// is in none.
    pub fn join(&self, npc: FormId) -> Option<DialogueMsg> {
        if self.joined() {
            return None;
        }
        let k = self.sessions.values().find(|k| k.view.npc == npc)?;
        Some(DialogueMsg::Join {
            session: k.view.session,
        })
    }

    /// Leaves the conversation this player is in: out of it here at once (no more window), and
    /// the table told.
    pub fn leave(&mut self) -> Option<DialogueMsg> {
        let me = self.me;
        let k = self
            .sessions
            .values_mut()
            .find(|k| k.joined.contains(&me))?;
        k.joined.retain(|p| *p != me);
        Some(DialogueMsg::Leave {
            session: k.view.session,
        })
    }

    /// True when this machine started the conversation shown.
    pub fn is_initiator(&self) -> bool {
        self.view().is_some_and(|v| v.initiator == self.me)
    }

    /// True when another player started the conversation shown.
    pub fn is_spectator(&self) -> bool {
        self.view().is_some_and(|v| v.initiator != self.me)
    }

    /// True while the initiator's UI must wait for the host's end script.
    pub fn ui_blocked(&self) -> bool {
        self.view().is_some_and(|v| v.playing.is_some())
    }

    /// Asks to talk to `npc`.
    pub fn open(&mut self, npc: FormId) -> (u32, DialogueMsg) {
        let req = self.next_req;
        self.next_req += 1;
        (req, DialogueMsg::Open { req, npc })
    }

    /// Picks the next choice (the info this game said), between steps: the initiator, or since
    /// S42 step 2 any player in the conversation who offered options (the table decides).
    pub fn pick(&mut self, choice: u32) -> Option<DialogueMsg> {
        let v = self.view().filter(|v| v.playing.is_none())?;
        Some(DialogueMsg::Pick {
            session: v.session,
            step: v.step + 1,
            choice,
        })
    }

    /// Reports the current step's lines finished (voice and subtitle done).
    pub fn line_done(&mut self) -> Option<DialogueMsg> {
        let v = &mut self
            .current_mut()
            .filter(|k| k.view.playing.is_some() && !k.view.reported)?
            .view;
        v.reported = true;
        Some(DialogueMsg::LineDone {
            session: v.session,
            step: v.step,
        })
    }

    /// Ends the conversation (initiator, while in it).
    pub fn close(&mut self) -> Option<DialogueMsg> {
        let v = self.mine()?.view;
        (v.initiator == self.me).then_some(DialogueMsg::Close { session: v.session })
    }

    /// Ends the conversation: this player's line, the last step's, ended it in its game (S42 step
    /// 2c; the table takes it from the initiator or the last step's picker).
    pub fn end(&mut self) -> Option<DialogueMsg> {
        let v = self.mine()?.view;
        Some(DialogueMsg::Close { session: v.session })
    }

    /// Keep-alive when due (any player in the conversation: it goes on when its initiator left).
    pub fn heartbeat(&mut self, now_ms: u64) -> Option<DialogueMsg> {
        let session = self.mine()?.view.session;
        if now_ms.saturating_sub(self.last_beat) < HEARTBEAT_MS {
            return None;
        }
        self.last_beat = now_ms;
        Some(DialogueMsg::Heartbeat { session })
    }

    /// Applies a host message.
    pub fn on_msg(&mut self, msg: DialogueMsg, now_ms: u64) {
        let shown = self.view().map(|v| v.session);
        match msg {
            DialogueMsg::Started {
                session,
                npc,
                initiator,
                ..
            } => {
                self.sessions.insert(
                    session,
                    Known {
                        view: View {
                            session,
                            npc,
                            initiator,
                            step: 0,
                            playing: None,
                            picker: None,
                            reported: false,
                        },
                        joined: vec![initiator],
                        menu: None,
                        last: None,
                    },
                );
                if initiator == self.me {
                    self.last_beat = now_ms;
                }
            }
            DialogueMsg::Denied { req, reason } => self.denied = Some((req, reason)),
            DialogueMsg::Refused {
                session, choice, ..
            } => {
                if shown == Some(session) {
                    self.refused += 1;
                    self.last_refused = Some((session, choice));
                }
            }
            DialogueMsg::Menu {
                session,
                step,
                items,
            } => {
                if let Some(k) = self.sessions.get_mut(&session) {
                    k.menu = Some((step, items));
                }
            }
            DialogueMsg::Ask {
                session,
                step,
                choice,
                by,
            } => {
                if shown == Some(session) {
                    self.asked = Some(Asked {
                        session,
                        step,
                        choice,
                        by,
                    });
                }
            }
            DialogueMsg::Picked {
                session,
                step,
                choice,
                by,
            } => {
                if let Some(k) = self
                    .sessions
                    .get_mut(&session)
                    .filter(|k| step > k.view.step)
                {
                    k.view.step = step;
                    k.view.playing = Some(choice);
                    k.view.picker = Some(by);
                    k.view.reported = false;
                    k.menu = None;
                    k.last = Some(choice);
                }
            }
            DialogueMsg::StepEnd { session, step } => {
                if let Some(k) = self
                    .sessions
                    .get_mut(&session)
                    .filter(|k| k.view.step == step)
                {
                    k.view.playing = None;
                }
            }
            DialogueMsg::Joined { session, players } => {
                if let Some(k) = self.sessions.get_mut(&session) {
                    k.joined = players;
                }
            }
            DialogueMsg::PlayerBusy { player, busy } => {
                if player != self.me {
                    self.busy.insert(player, busy);
                }
            }
            DialogueMsg::WaitingFor { player, npc } => {
                if player != self.me {
                    self.waiting.push((player, npc));
                }
            }
            DialogueMsg::Closed { session, reason } => {
                self.sessions.remove(&session);
                self.last_closed = Some((session, reason));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NPC: FormId = FormId(1);

    fn start(c: &mut DialogueClient, initiator: u8) {
        c.on_msg(
            DialogueMsg::Started {
                session: 4,
                npc: NPC,
                initiator: PlayerId(initiator),
                req: 1,
            },
            0,
        );
    }

    #[test]
    fn initiator_flow_blocks_between_pick_and_end() {
        let mut c = DialogueClient::new(PlayerId(1));
        start(&mut c, 1);
        assert!(c.is_initiator());
        let Some(DialogueMsg::Pick { step: 1, .. }) = c.pick(7) else {
            panic!()
        };
        c.on_msg(
            DialogueMsg::Picked {
                session: 4,
                step: 1,
                choice: 7,
                by: PlayerId(1),
            },
            1,
        );
        assert!(c.ui_blocked());
        assert!(c.pick(8).is_none(), "blocked until the end script");
        assert!(c.line_done().is_some());
        assert!(c.line_done().is_none(), "reported once");
        c.on_msg(
            DialogueMsg::StepEnd {
                session: 4,
                step: 1,
            },
            2,
        );
        assert!(!c.ui_blocked());
        let Some(DialogueMsg::Pick { step: 2, .. }) = c.pick(8) else {
            panic!()
        };
    }

    #[test]
    fn spectator_cannot_close_but_may_pick_and_reports_lines() {
        let mut c = DialogueClient::new(PlayerId(2));
        start(&mut c, 1);
        assert!(c.is_spectator());
        assert!(c.close().is_none() && c.heartbeat(99_999).is_none());
        // S42 step 2: a spectator's pick goes to the table, which takes it from a player who
        // offered options for the step.
        assert!(matches!(
            c.pick(1),
            Some(DialogueMsg::Pick {
                session: 4,
                step: 1,
                choice: 1
            })
        ));
        c.on_msg(
            DialogueMsg::Picked {
                session: 4,
                step: 1,
                choice: 5,
                by: PlayerId(1),
            },
            1,
        );
        assert!(c.line_done().is_some());
    }

    #[test]
    fn stale_messages_are_ignored() {
        let mut c = DialogueClient::new(PlayerId(2));
        start(&mut c, 1);
        c.on_msg(
            DialogueMsg::Picked {
                session: 9,
                step: 1,
                choice: 5,
                by: PlayerId(1),
            },
            1,
        );
        assert!(c.view().unwrap().playing.is_none());
        c.on_msg(
            DialogueMsg::Picked {
                session: 4,
                step: 1,
                choice: 5,
                by: PlayerId(1),
            },
            1,
        );
        c.on_msg(
            DialogueMsg::Picked {
                session: 4,
                step: 1,
                choice: 6,
                by: PlayerId(1),
            },
            1,
        );
        assert_eq!(c.view().unwrap().playing, Some(5));
        c.on_msg(
            DialogueMsg::StepEnd {
                session: 4,
                step: 7,
            },
            2,
        );
        assert!(c.ui_blocked());
    }

    #[test]
    fn heartbeat_cadence_and_close() {
        let mut c = DialogueClient::new(PlayerId(1));
        start(&mut c, 1);
        // Another player's conversation starting does not put this one's heartbeat off.
        c.on_msg(
            DialogueMsg::Started {
                session: 2,
                npc: FormId(9),
                initiator: PlayerId(2),
                req: 1,
            },
            HEARTBEAT_MS - 1,
        );
        assert!(c.heartbeat(HEARTBEAT_MS - 1).is_none());
        assert!(c.heartbeat(HEARTBEAT_MS).is_some());
        assert!(c.heartbeat(HEARTBEAT_MS + 1).is_none());
        c.on_msg(
            DialogueMsg::Closed {
                session: 4,
                reason: CloseReason::InitiatorLeft,
            },
            5,
        );
        // The other conversation is shown now; once it ends too, none.
        assert_eq!(c.view().map(|v| v.session), Some(2));
        assert_eq!(c.last_closed, Some((4, CloseReason::InitiatorLeft)));
        c.on_msg(
            DialogueMsg::Closed {
                session: 2,
                reason: CloseReason::Finished,
            },
            6,
        );
        assert!(c.view().is_none());
    }

    #[test]
    fn denial_is_recorded() {
        let mut c = DialogueClient::new(PlayerId(1));
        let (req, _) = c.open(NPC);
        c.on_msg(
            DialogueMsg::Denied {
                req,
                reason: Deny::Busy,
            },
            0,
        );
        assert_eq!(c.denied, Some((req, Deny::Busy)));
    }

    #[test]
    fn the_menu_and_an_ask_are_for_the_next_step_of_this_session_only() {
        let mut c = DialogueClient::new(PlayerId(2));
        start(&mut c, 1);
        let item = MenuItem {
            choice: 7,
            owners: vec![PlayerId(1)],
        };
        assert!(matches!(
            c.options(vec![7, 8]),
            Some(DialogueMsg::Options { session: 4, step: 1, ref choices }) if *choices == [7, 8]
        ));
        assert!(matches!(
            c.want(7),
            Some(DialogueMsg::Want {
                session: 4,
                step: 1,
                choice: 7
            })
        ));
        // Another session's menu and ask: not ours.
        c.on_msg(
            DialogueMsg::Menu {
                session: 5,
                step: 1,
                items: vec![item.clone()],
            },
            1,
        );
        c.on_msg(
            DialogueMsg::Ask {
                session: 5,
                step: 1,
                choice: 7,
                by: PlayerId(1),
            },
            1,
        );
        assert!(c.menu().is_none());
        assert!(c.take_asked().is_none());
        c.on_msg(
            DialogueMsg::Menu {
                session: 4,
                step: 1,
                items: vec![item.clone()],
            },
            1,
        );
        c.on_msg(
            DialogueMsg::Ask {
                session: 4,
                step: 1,
                choice: 7,
                by: PlayerId(1),
            },
            1,
        );
        assert_eq!(c.menu(), Some(&[item.clone()][..]));
        assert_eq!(
            c.take_asked(),
            Some(Asked {
                session: 4,
                step: 1,
                choice: 7,
                by: PlayerId(1)
            })
        );
        assert!(c.take_asked().is_none(), "taken once");
        // A menu for a later step is not this one's.
        c.on_msg(
            DialogueMsg::Menu {
                session: 4,
                step: 2,
                items: vec![item.clone()],
            },
            1,
        );
        assert!(c.menu().is_none());
        // Picked: the step's menu and ask are gone; none while it plays.
        c.on_msg(
            DialogueMsg::Menu {
                session: 4,
                step: 1,
                items: vec![item.clone()],
            },
            1,
        );
        c.on_msg(
            DialogueMsg::Ask {
                session: 4,
                step: 1,
                choice: 7,
                by: PlayerId(1),
            },
            1,
        );
        c.on_msg(
            DialogueMsg::Picked {
                session: 4,
                step: 1,
                choice: 9,
                by: PlayerId(1),
            },
            2,
        );
        assert!(c.menu().is_none() && c.take_asked().is_none());
        assert!(c.options(vec![1]).is_none() && c.want(1).is_none() && c.pick(1).is_none());
        // Closed: gone too.
        c.on_msg(
            DialogueMsg::StepEnd {
                session: 4,
                step: 1,
            },
            3,
        );
        c.on_msg(
            DialogueMsg::Menu {
                session: 4,
                step: 2,
                items: vec![item.clone()],
            },
            4,
        );
        assert!(c.menu().is_some());
        c.on_msg(
            DialogueMsg::Closed {
                session: 4,
                reason: CloseReason::Finished,
            },
            5,
        );
        assert!(c.menu().is_none());
    }

    fn started(c: &mut DialogueClient, session: u32, npc: u32, initiator: u8) {
        c.on_msg(
            DialogueMsg::Started {
                session,
                npc: FormId(npc),
                initiator: PlayerId(initiator),
                req: 1,
            },
            0,
        );
    }

    #[test]
    fn a_player_shows_its_own_conversation_else_the_newest_and_joins_by_the_npc() {
        let mut c = DialogueClient::new(PlayerId(2));
        assert!(c.join(NPC).is_none() && c.busy_npcs().is_empty());
        // Player 1 talks to NPC 1: shown, not joined, busy to this player.
        started(&mut c, 4, 1, 1);
        assert_eq!(c.view().unwrap().session, 4);
        assert!(!c.joined());
        assert_eq!(c.players_in(), [PlayerId(1)]);
        assert_eq!(c.busy_npcs(), [FormId(1)]);
        assert!(c.close().is_none() && c.end().is_none() && c.heartbeat(99_999).is_none());
        assert!(c.leave().is_none());
        // Player 3 talks to NPC 5: the newest is shown.
        started(&mut c, 6, 5, 3);
        assert_eq!(c.view().unwrap().session, 6);
        assert_eq!(c.busy_npcs(), [FormId(1), FormId(5)]);
        // Talking to NPC 1 joins session 4 (not one of an NPC without a conversation).
        assert!(c.join(FormId(9)).is_none());
        assert_eq!(c.join(FormId(1)), Some(DialogueMsg::Join { session: 4 }));
        c.on_msg(
            DialogueMsg::Joined {
                session: 4,
                players: vec![PlayerId(1), PlayerId(2)],
            },
            1,
        );
        // In it: shown, in charge of its end and its heartbeat, and NPC 1 no longer busy.
        assert!(c.joined());
        assert_eq!(c.view().unwrap().session, 4);
        assert_eq!(c.players_in(), [PlayerId(1), PlayerId(2)]);
        assert_eq!(c.busy_npcs(), [FormId(5)]);
        assert!(c.join(FormId(5)).is_none(), "one conversation at a time");
        assert!(c.close().is_none(), "not its initiator");
        assert_eq!(c.end(), Some(DialogueMsg::Close { session: 4 }));
        assert_eq!(
            c.heartbeat(HEARTBEAT_MS),
            Some(DialogueMsg::Heartbeat { session: 4 })
        );
        // Its last line, for a player joining between steps.
        assert_eq!(c.last_choice(), None);
        c.on_msg(
            DialogueMsg::Picked {
                session: 4,
                step: 1,
                choice: 7,
                by: PlayerId(1),
            },
            2,
        );
        c.on_msg(
            DialogueMsg::Picked {
                session: 6,
                step: 1,
                choice: 8,
                by: PlayerId(1),
            },
            2,
        );
        assert_eq!(c.last_choice(), Some(7));
        assert_eq!(c.view().unwrap().playing, Some(7));
        assert_eq!(c.view().unwrap().picker, Some(PlayerId(1)));
        // Leaving: out at once, the newest shown again, the table told.
        assert_eq!(c.leave(), Some(DialogueMsg::Leave { session: 4 }));
        assert!(!c.joined());
        assert_eq!(c.view().unwrap().session, 6);
        assert_eq!(c.last_choice(), Some(8));
        assert!(c.leave().is_none());
        // Its lines are reported for the shown one.
        assert_eq!(
            c.line_done(),
            Some(DialogueMsg::LineDone {
                session: 6,
                step: 1
            })
        );
        // Session 6 ends: session 4 is shown, as it stands.
        c.on_msg(
            DialogueMsg::Closed {
                session: 6,
                reason: CloseReason::Finished,
            },
            3,
        );
        let v = c.view().unwrap();
        assert_eq!((v.session, v.step, v.playing), (4, 1, Some(7)));
    }

    #[test]
    fn the_initiator_who_left_neither_closes_nor_beats_and_an_ask_is_for_the_shown_one() {
        let mut c = DialogueClient::new(PlayerId(1));
        started(&mut c, 4, 1, 1);
        assert!(c.joined() && c.close().is_some());
        c.leave();
        assert!(c.is_initiator() && !c.joined());
        assert!(c.close().is_none() && c.end().is_none() && c.heartbeat(99_999).is_none());
        // Another session's ask and refusal are not this one's.
        started(&mut c, 3, 2, 0);
        c.on_msg(
            DialogueMsg::Ask {
                session: 3,
                step: 1,
                choice: 7,
                by: PlayerId(0),
            },
            1,
        );
        c.on_msg(
            DialogueMsg::Refused {
                session: 3,
                step: 1,
                choice: 7,
            },
            1,
        );
        assert!(c.take_asked().is_none());
        assert_eq!(c.refused, 0);
    }

    #[test]
    fn what_the_players_are_doing_and_who_waits_is_kept() {
        let mut c = DialogueClient::new(PlayerId(1));
        // This player's own: sent when it changes.
        assert_eq!(c.set_busy(Busy::Free), None);
        assert_eq!(
            c.set_busy(Busy::Combat),
            Some(DialogueMsg::Busy { busy: Busy::Combat })
        );
        assert_eq!(c.set_busy(Busy::Combat), None);
        assert_eq!(c.waiting(NPC), DialogueMsg::Waiting { npc: NPC });
        // The others': as the host tells; this player's own word back is not kept.
        assert_eq!(c.busy_of(PlayerId(0)), Busy::Free);
        for player in [PlayerId(0), PlayerId(1)] {
            c.on_msg(
                DialogueMsg::PlayerBusy {
                    player,
                    busy: Busy::Menu,
                },
                1,
            );
            c.on_msg(DialogueMsg::WaitingFor { player, npc: NPC }, 1);
        }
        assert_eq!(c.busy_of(PlayerId(0)), Busy::Menu);
        assert_eq!(c.busy_of(PlayerId(1)), Busy::Free);
        assert_eq!(c.take_waiting(), [(PlayerId(0), NPC)]);
        assert!(c.take_waiting().is_empty());
    }
}
