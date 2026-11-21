//! Guest side of the dialogue protocol: what the dialogue UI needs to know.

use super::{CloseReason, Deny, DialogueMsg, MenuItem, HEARTBEAT_MS};
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

/// Dialogue state for one machine.
#[derive(Clone, Debug)]
pub struct DialogueClient {
    me: PlayerId,
    next_req: u32,
    view: Option<View>,
    last_beat: u64,
    /// Newest denial of one of our opens.
    pub denied: Option<(u32, Deny)>,
    /// How the last session ended.
    pub last_closed: Option<(u32, CloseReason)>,
    /// Picks of ours the host refused, and the newest refused choice.
    pub refused: u32,
    /// The newest refused pick: (session, choice).
    pub last_refused: Option<(u32, u32)>,
    /// The conversation's merged options for the next step (S42 step 2).
    menu: Option<(u32, u32, Vec<MenuItem>)>,
    /// An option of ours another player picked, for this game to say.
    asked: Option<Asked>,
}

impl DialogueClient {
    /// A client for player `me`.
    pub fn new(me: PlayerId) -> Self {
        DialogueClient {
            me,
            next_req: 1,
            view: None,
            last_beat: 0,
            denied: None,
            last_closed: None,
            refused: 0,
            last_refused: None,
            menu: None,
            asked: None,
        }
    }

    /// The conversation's merged options for the next step, when the table sent them.
    pub fn menu(&self) -> Option<&[MenuItem]> {
        let v = self.view.as_ref()?;
        self.menu
            .as_ref()
            .filter(|(session, step, _)| {
                *session == v.session && *step == v.step + 1 && v.playing.is_none()
            })
            .map(|(_, _, items)| items.as_slice())
    }

    /// An option of ours another player picked, for this game to say (taken once).
    pub fn take_asked(&mut self) -> Option<Asked> {
        let v = self.view?;
        self.asked
            .take()
            .filter(|a| a.session == v.session && a.step == v.step + 1 && v.playing.is_none())
    }

    /// This player's options for the next step (S42 step 2): the infos its player may say.
    pub fn options(&self, choices: Vec<u32>) -> Option<DialogueMsg> {
        let v = self.view.as_ref().filter(|v| v.playing.is_none())?;
        Some(DialogueMsg::Options {
            session: v.session,
            step: v.step + 1,
            choices,
        })
    }

    /// Picks another player's option of the merged list: the table asks its owner to say it.
    pub fn want(&self, choice: u32) -> Option<DialogueMsg> {
        let v = self.view.as_ref().filter(|v| v.playing.is_none())?;
        Some(DialogueMsg::Want {
            session: v.session,
            step: v.step + 1,
            choice,
        })
    }

    /// Current session, if any.
    pub fn view(&self) -> Option<&View> {
        self.view.as_ref()
    }

    /// True when this machine picks.
    pub fn is_initiator(&self) -> bool {
        self.view.map_or(false, |v| v.initiator == self.me)
    }

    /// True when this machine only watches (banner, read-only list).
    pub fn is_spectator(&self) -> bool {
        self.view.map_or(false, |v| v.initiator != self.me)
    }

    /// True while the initiator's UI must wait for the host's end script.
    pub fn ui_blocked(&self) -> bool {
        self.view.map_or(false, |v| v.playing.is_some())
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
        let v = self.view.as_ref().filter(|v| v.playing.is_none())?;
        Some(DialogueMsg::Pick {
            session: v.session,
            step: v.step + 1,
            choice,
        })
    }

    /// Reports the current step's lines finished (voice and subtitle done).
    pub fn line_done(&mut self) -> Option<DialogueMsg> {
        let v = self
            .view
            .as_mut()
            .filter(|v| v.playing.is_some() && !v.reported)?;
        v.reported = true;
        Some(DialogueMsg::LineDone {
            session: v.session,
            step: v.step,
        })
    }

    /// Ends the conversation (initiator).
    pub fn close(&mut self) -> Option<DialogueMsg> {
        let v = self.view.as_ref().filter(|v| v.initiator == self.me)?;
        Some(DialogueMsg::Close { session: v.session })
    }

    /// Keep-alive when due (initiator).
    pub fn heartbeat(&mut self, now_ms: u64) -> Option<DialogueMsg> {
        let v = self.view.as_ref().filter(|v| v.initiator == self.me)?;
        if now_ms.saturating_sub(self.last_beat) < HEARTBEAT_MS {
            return None;
        }
        self.last_beat = now_ms;
        Some(DialogueMsg::Heartbeat { session: v.session })
    }

    /// Applies a host message.
    pub fn on_msg(&mut self, msg: DialogueMsg, now_ms: u64) {
        match msg {
            DialogueMsg::Started {
                session,
                npc,
                initiator,
                ..
            } => {
                self.view = Some(View {
                    session,
                    npc,
                    initiator,
                    step: 0,
                    playing: None,
                    reported: false,
                });
                self.last_beat = now_ms;
            }
            DialogueMsg::Denied { req, reason } => self.denied = Some((req, reason)),
            DialogueMsg::Refused {
                session, choice, ..
            } => {
                if self.view.is_some_and(|v| v.session == session) {
                    self.refused += 1;
                    self.last_refused = Some((session, choice));
                }
            }
            DialogueMsg::Menu {
                session,
                step,
                items,
            } => {
                if self.view.is_some_and(|v| v.session == session) {
                    self.menu = Some((session, step, items));
                }
            }
            DialogueMsg::Ask {
                session,
                step,
                choice,
                by,
            } => {
                if self.view.is_some_and(|v| v.session == session) {
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
            } => {
                if let Some(v) = self
                    .view
                    .as_mut()
                    .filter(|v| v.session == session && step > v.step)
                {
                    v.step = step;
                    v.playing = Some(choice);
                    v.reported = false;
                    self.menu = None;
                    self.asked = None;
                }
            }
            DialogueMsg::StepEnd { session, step } => {
                if let Some(v) = self
                    .view
                    .as_mut()
                    .filter(|v| v.session == session && v.step == step)
                {
                    v.playing = None;
                }
            }
            DialogueMsg::Closed { session, reason } => {
                if self.view.map_or(false, |v| v.session == session) {
                    self.view = None;
                    self.menu = None;
                    self.asked = None;
                }
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
            },
            1,
        );
        assert!(c.view().unwrap().playing.is_none());
        c.on_msg(
            DialogueMsg::Picked {
                session: 4,
                step: 1,
                choice: 5,
            },
            1,
        );
        c.on_msg(
            DialogueMsg::Picked {
                session: 4,
                step: 1,
                choice: 6,
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
        assert!(c.view().is_none());
        assert_eq!(c.last_closed, Some((4, CloseReason::InitiatorLeft)));
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
}
