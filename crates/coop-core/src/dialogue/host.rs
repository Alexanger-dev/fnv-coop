//! Host side of the dialogue protocol.

use std::collections::{BTreeMap, BTreeSet};

use super::{
    CloseReason, Deny, DialogueEvent, DialogueMsg, MenuItem, ASK_TIMEOUT_MS, HEARTBEAT_MISS_MS,
    LEASE_MS, LINE_TIMEOUT_MS, OPTIONS_MAX,
};
use crate::ids::{FormId, PlayerId};
use crate::interest::Audience;

/// One addressed output message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DOut {
    /// Recipients.
    pub aud: Audience,
    /// Message.
    pub msg: DialogueMsg,
}

fn out(aud: Audience, msg: DialogueMsg) -> DOut {
    DOut { aud, msg }
}

#[derive(Clone, Debug)]
struct Playing {
    step: u32,
    choice: u32,
    started_ms: u64,
    waiting: BTreeSet<PlayerId>,
}

/// A pick of another player's option, waiting for its owner to say it.
#[derive(Clone, Debug)]
struct Asking {
    step: u32,
    choice: u32,
    by: PlayerId,
    owner: PlayerId,
    since_ms: u64,
}

#[derive(Clone, Debug)]
struct Session {
    npc: FormId,
    initiator: PlayerId,
    step: u32,
    playing: Option<Playing>,
    last_pick_ms: u64,
    last_beat_ms: u64,
    /// Each player's options for the next step (S42 step 2).
    offers: BTreeMap<PlayerId, Vec<u32>>,
    /// The merged options last sent.
    menu: Vec<MenuItem>,
    asking: Option<Asking>,
}

impl Session {
    /// Every player's options merged: the initiator's first, in its order, then each other
    /// player's (by id) not offered yet; an option's owners the initiator first, then by id.
    fn merged(&self) -> Vec<MenuItem> {
        let mut order: Vec<PlayerId> = vec![self.initiator];
        order.extend(self.offers.keys().copied().filter(|p| *p != self.initiator));
        let mut items: Vec<MenuItem> = Vec::new();
        for p in order {
            for &choice in self.offers.get(&p).into_iter().flatten() {
                match items.iter_mut().find(|i| i.choice == choice) {
                    Some(i) => {
                        if !i.owners.contains(&p) {
                            i.owners.push(p);
                        }
                    }
                    None => items.push(MenuItem {
                        choice,
                        owners: vec![p],
                    }),
                }
            }
        }
        items
    }
}

/// The authoritative dialogue lock and step sequencer.
#[derive(Clone, Debug, Default)]
pub struct DialogueHost {
    sessions: BTreeMap<u32, Session>,
    npc_lock: BTreeMap<FormId, u32>,
    talking: BTreeMap<PlayerId, u32>,
    dead: BTreeSet<FormId>,
    combat: BTreeSet<FormId>,
    next_id: u32,
    events: Vec<DialogueEvent>,
}

impl DialogueHost {
    /// Empty host side.
    pub fn new() -> Self {
        DialogueHost {
            next_id: 1,
            ..Default::default()
        }
    }

    /// Script actions the game must perform, in order.
    pub fn take_events(&mut self) -> Vec<DialogueEvent> {
        std::mem::take(&mut self.events)
    }

    /// Whether a conversation with `npc` is open.
    pub fn is_talking(&self, npc: FormId) -> bool {
        self.npc_lock.contains_key(&npc)
    }

    /// Open sessions.
    pub fn open_sessions(&self) -> usize {
        self.sessions.len()
    }

    /// Session the player is initiating, if any.
    pub fn session_of(&self, p: PlayerId) -> Option<u32> {
        self.talking.get(&p).copied()
    }

    /// The NPC of an open session.
    pub fn npc_of(&self, session: u32) -> Option<FormId> {
        self.sessions.get(&session).map(|s| s.npc)
    }

    /// The game refused a pick (the line's conditions fail for the picker).
    /// The step stays open, no script runs, and only the picker is told.
    /// A pick the table would ignore anyway gets no answer.
    pub fn refuse(&mut self, from: PlayerId, session: u32, step: u32, choice: u32) -> Vec<DOut> {
        let open = self.sessions.get(&session).is_some_and(|s| {
            (s.initiator == from || s.offers.contains_key(&from))
                && s.playing.is_none()
                && step == s.step + 1
        });
        if !open {
            return vec![];
        }
        vec![out(
            Audience::To(from),
            DialogueMsg::Refused {
                session,
                step,
                choice,
            },
        )]
    }

    /// Updates the NPC combat flag used to deny new sessions.
    pub fn set_in_combat(&mut self, npc: FormId, v: bool) {
        if v {
            self.combat.insert(npc);
        } else {
            self.combat.remove(&npc);
        }
    }

    /// The NPC died: any session with it ends.
    pub fn npc_died(&mut self, npc: FormId) -> Vec<DOut> {
        self.dead.insert(npc);
        match self.npc_lock.get(&npc).copied() {
            Some(s) => self.close(s, CloseReason::NpcDied),
            None => vec![],
        }
    }

    /// Handles one message. `members` is everyone currently in the session
    /// (host included); they become the step barrier.
    pub fn on_msg(
        &mut self,
        from: PlayerId,
        msg: DialogueMsg,
        now_ms: u64,
        members: &[PlayerId],
    ) -> Vec<DOut> {
        match msg {
            DialogueMsg::Open { req, npc } => self.open(from, req, npc, now_ms),
            DialogueMsg::Pick {
                session,
                step,
                choice,
            } => self.pick(from, session, step, choice, now_ms, members),
            DialogueMsg::LineDone { session, step } => {
                if let Some(Session {
                    playing: Some(p), ..
                }) = self.sessions.get_mut(&session)
                {
                    if p.step == step {
                        p.waiting.remove(&from);
                    }
                }
                self.maybe_finish_step(session)
            }
            DialogueMsg::Options {
                session,
                step,
                choices,
            } => self.options(from, session, step, choices, members),
            DialogueMsg::Want {
                session,
                step,
                choice,
            } => self.want(from, session, step, choice, now_ms, members),
            DialogueMsg::Heartbeat { session } => {
                if let Some(s) = self
                    .sessions
                    .get_mut(&session)
                    .filter(|s| s.initiator == from)
                {
                    s.last_beat_ms = now_ms;
                }
                vec![]
            }
            DialogueMsg::Close { session } => {
                if self
                    .sessions
                    .get(&session)
                    .map_or(false, |s| s.initiator == from)
                {
                    self.close(session, CloseReason::Finished)
                } else {
                    vec![]
                }
            }
            _ => vec![],
        }
    }

    fn open(&mut self, from: PlayerId, req: u32, npc: FormId, now_ms: u64) -> Vec<DOut> {
        let deny = if self.talking.contains_key(&from) {
            Some(Deny::AlreadyTalking)
        } else if self.dead.contains(&npc) {
            Some(Deny::NpcDead)
        } else if self.npc_lock.contains_key(&npc) {
            Some(Deny::Busy)
        } else if self.combat.contains(&npc) {
            Some(Deny::NpcInCombat)
        } else {
            None
        };
        if let Some(reason) = deny {
            return vec![out(Audience::To(from), DialogueMsg::Denied { req, reason })];
        }
        let session = self.next_id;
        self.next_id += 1;
        self.sessions.insert(
            session,
            Session {
                npc,
                initiator: from,
                step: 0,
                playing: None,
                last_pick_ms: now_ms,
                last_beat_ms: now_ms,
                offers: BTreeMap::new(),
                menu: Vec::new(),
                asking: None,
            },
        );
        self.npc_lock.insert(npc, session);
        self.talking.insert(from, session);
        vec![out(
            Audience::All,
            DialogueMsg::Started {
                session,
                npc,
                initiator: from,
                req,
            },
        )]
    }

    fn pick(
        &mut self,
        from: PlayerId,
        session: u32,
        step: u32,
        choice: u32,
        now_ms: u64,
        members: &[PlayerId],
    ) -> Vec<DOut> {
        // The initiator picks; so does any player in the conversation who offered options for
        // this step (S42 step 2: its game said one of its own, or one it was asked to say).
        let Some(s) = self.sessions.get_mut(&session).filter(|s| {
            s.initiator == from || (members.contains(&from) && s.offers.contains_key(&from))
        }) else {
            return vec![];
        };
        // duplicates (step already taken), skips and picks mid-step are ignored
        if s.playing.is_some() || step != s.step + 1 {
            return vec![];
        }
        s.step = step;
        s.offers.clear();
        s.menu.clear();
        s.asking = None;
        s.last_pick_ms = now_ms;
        s.last_beat_ms = now_ms;
        s.playing = Some(Playing {
            step,
            choice,
            started_ms: now_ms,
            waiting: members.iter().copied().collect(),
        });
        self.events.push(DialogueEvent::RunBegin {
            session,
            npc: s.npc,
            step,
            choice,
            initiator: from,
        });
        vec![out(
            Audience::All,
            DialogueMsg::Picked {
                session,
                step,
                choice,
            },
        )]
    }

    /// A player's options for the next step: merged with the others', and the merged list sent
    /// to everyone when it changed. Only a player in the conversation, between steps.
    fn options(
        &mut self,
        from: PlayerId,
        session: u32,
        step: u32,
        mut choices: Vec<u32>,
        members: &[PlayerId],
    ) -> Vec<DOut> {
        let Some(s) = self.sessions.get_mut(&session) else {
            return vec![];
        };
        if !(from == s.initiator || members.contains(&from))
            || s.playing.is_some()
            || step != s.step + 1
        {
            return vec![];
        }
        choices.truncate(OPTIONS_MAX);
        if choices.is_empty() {
            s.offers.remove(&from);
        } else {
            s.offers.insert(from, choices);
        }
        let menu = s.merged();
        if menu == s.menu {
            return vec![];
        }
        s.menu = menu.clone();
        vec![out(
            Audience::All,
            DialogueMsg::Menu {
                session,
                step,
                items: menu,
            },
        )]
    }

    /// A player picks an option of the merged list that is not its own: an owner is asked to say
    /// it (the initiator when it is one). One at a time; a pick of an option no one offers is
    /// refused.
    fn want(
        &mut self,
        from: PlayerId,
        session: u32,
        step: u32,
        choice: u32,
        now_ms: u64,
        members: &[PlayerId],
    ) -> Vec<DOut> {
        let Some(s) = self.sessions.get_mut(&session) else {
            return vec![];
        };
        if !(from == s.initiator || members.contains(&from))
            || s.playing.is_some()
            || step != s.step + 1
            || s.asking.is_some()
        {
            return vec![];
        }
        let owner = s
            .menu
            .iter()
            .find(|i| i.choice == choice)
            .and_then(|i| i.owners.first().copied())
            .filter(|o| *o != from && members.iter().chain([&s.initiator]).any(|m| m == o));
        let Some(owner) = owner else {
            return vec![out(
                Audience::To(from),
                DialogueMsg::Refused {
                    session,
                    step,
                    choice,
                },
            )];
        };
        s.asking = Some(Asking {
            step,
            choice,
            by: from,
            owner,
            since_ms: now_ms,
        });
        vec![out(
            Audience::To(owner),
            DialogueMsg::Ask {
                session,
                step,
                choice,
                by: from,
            },
        )]
    }

    /// A pick the owner did not say in time (it left, or its game could not): refused to the
    /// player who made it.
    fn ask_lapsed(&mut self, session: u32) -> Vec<DOut> {
        let Some(a) = self
            .sessions
            .get_mut(&session)
            .and_then(|s| s.asking.take())
        else {
            return vec![];
        };
        vec![out(
            Audience::To(a.by),
            DialogueMsg::Refused {
                session,
                step: a.step,
                choice: a.choice,
            },
        )]
    }

    fn maybe_finish_step(&mut self, session: u32) -> Vec<DOut> {
        let Some(s) = self.sessions.get_mut(&session) else {
            return vec![];
        };
        let done = s.playing.as_ref().map_or(false, |p| p.waiting.is_empty());
        if !done {
            return vec![];
        }
        let p = s.playing.take().expect("checked");
        self.events.push(DialogueEvent::RunEnd {
            session,
            npc: s.npc,
            step: p.step,
            choice: p.choice,
        });
        vec![out(
            Audience::All,
            DialogueMsg::StepEnd {
                session,
                step: p.step,
            },
        )]
    }

    fn close(&mut self, session: u32, reason: CloseReason) -> Vec<DOut> {
        let Some(s) = self.sessions.remove(&session) else {
            return vec![];
        };
        // a step whose begin script ran always gets its end script
        if let Some(p) = s.playing {
            self.events.push(DialogueEvent::RunEnd {
                session,
                npc: s.npc,
                step: p.step,
                choice: p.choice,
            });
        }
        self.npc_lock.remove(&s.npc);
        self.talking.remove(&s.initiator);
        self.events.push(DialogueEvent::Closed {
            session,
            npc: s.npc,
            reason,
        });
        vec![out(Audience::All, DialogueMsg::Closed { session, reason })]
    }

    /// A peer left. The initiator's departure aborts its session; anyone
    /// else just stops counting for barriers.
    pub fn on_disconnect(&mut self, p: PlayerId) -> Vec<DOut> {
        let mut res = Vec::new();
        if let Some(s) = self.talking.get(&p).copied() {
            res.extend(self.close(s, CloseReason::InitiatorLeft));
        }
        let ids: Vec<u32> = self.sessions.keys().copied().collect();
        for id in ids {
            let mut lapsed = false;
            let mut menu = None;
            if let Some(s) = self.sessions.get_mut(&id) {
                if let Some(pl) = s.playing.as_mut() {
                    pl.waiting.remove(&p);
                }
                lapsed = s.asking.as_ref().is_some_and(|a| a.owner == p);
                if s.asking.as_ref().is_some_and(|a| a.by == p) {
                    s.asking = None;
                }
                // Its options leave with it.
                if s.offers.remove(&p).is_some() {
                    let merged = s.merged();
                    if merged != s.menu {
                        s.menu = merged.clone();
                        menu = Some((s.step + 1, merged));
                    }
                }
            }
            if lapsed {
                res.extend(self.ask_lapsed(id));
            }
            if let Some((step, items)) = menu {
                res.push(out(
                    Audience::All,
                    DialogueMsg::Menu {
                        session: id,
                        step,
                        items,
                    },
                ));
            }
            res.extend(self.maybe_finish_step(id));
        }
        res
    }

    /// Timeouts: slow spectators, lost heartbeats, expired leases.
    pub fn tick(&mut self, now_ms: u64) -> Vec<DOut> {
        let mut res = Vec::new();
        let ids: Vec<u32> = self.sessions.keys().copied().collect();
        for id in ids {
            let Some(s) = self.sessions.get_mut(&id) else {
                continue;
            };
            if now_ms.saturating_sub(s.last_beat_ms) >= HEARTBEAT_MISS_MS {
                res.extend(self.close(id, CloseReason::HeartbeatLost));
            } else if now_ms.saturating_sub(s.last_pick_ms) >= LEASE_MS {
                res.extend(self.close(id, CloseReason::LeaseExpired));
            } else {
                if let Some(p) = s.playing.as_mut() {
                    if now_ms.saturating_sub(p.started_ms) >= LINE_TIMEOUT_MS {
                        p.waiting.clear();
                    }
                }
                let lapsed = s
                    .asking
                    .as_ref()
                    .is_some_and(|a| now_ms.saturating_sub(a.since_ms) >= ASK_TIMEOUT_MS);
                if lapsed {
                    res.extend(self.ask_lapsed(id));
                }
                res.extend(self.maybe_finish_step(id));
            }
        }
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NPC: FormId = FormId(0x77);
    const H: PlayerId = PlayerId(0);
    const A: PlayerId = PlayerId(1);
    const B: PlayerId = PlayerId(2);
    const ALL: [PlayerId; 3] = [H, A, B];

    fn started(h: &mut DialogueHost, who: PlayerId) -> u32 {
        let o = h.on_msg(who, DialogueMsg::Open { req: 1, npc: NPC }, 0, &ALL);
        match o[0].msg {
            DialogueMsg::Started { session, .. } => session,
            ref m => panic!("{m:?}"),
        }
    }

    fn done_all(h: &mut DialogueHost, s: u32, step: u32) -> Vec<DOut> {
        let mut last = vec![];
        for p in ALL {
            last = h.on_msg(p, DialogueMsg::LineDone { session: s, step }, 10, &ALL);
        }
        last
    }

    #[test]
    fn initiator_picks_script_runs_once_barrier_ends_step() {
        let mut h = DialogueHost::new();
        let s = started(&mut h, A);
        let o = h.on_msg(
            A,
            DialogueMsg::Pick {
                session: s,
                step: 1,
                choice: 9,
            },
            5,
            &ALL,
        );
        assert!(matches!(
            o[0].msg,
            DialogueMsg::Picked {
                step: 1,
                choice: 9,
                ..
            }
        ));
        // duplicate pick: nothing, no second begin script
        assert!(h
            .on_msg(
                A,
                DialogueMsg::Pick {
                    session: s,
                    step: 1,
                    choice: 9
                },
                6,
                &ALL
            )
            .is_empty());
        for p in [H, A] {
            assert!(h
                .on_msg(
                    p,
                    DialogueMsg::LineDone {
                        session: s,
                        step: 1
                    },
                    10,
                    &ALL
                )
                .is_empty());
        }
        let o = h.on_msg(
            B,
            DialogueMsg::LineDone {
                session: s,
                step: 1,
            },
            11,
            &ALL,
        );
        assert!(matches!(o[0].msg, DialogueMsg::StepEnd { step: 1, .. }));
        // duplicate LineDone after the step ended changes nothing
        assert!(h
            .on_msg(
                B,
                DialogueMsg::LineDone {
                    session: s,
                    step: 1
                },
                12,
                &ALL
            )
            .is_empty());
        let ev = h.take_events();
        assert_eq!(
            ev.iter()
                .filter(|e| matches!(e, DialogueEvent::RunBegin { .. }))
                .count(),
            1
        );
        assert_eq!(
            ev.iter()
                .filter(|e| matches!(e, DialogueEvent::RunEnd { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn only_the_initiator_picks_and_steps_must_increase() {
        let mut h = DialogueHost::new();
        let s = started(&mut h, A);
        assert!(h
            .on_msg(
                B,
                DialogueMsg::Pick {
                    session: s,
                    step: 1,
                    choice: 1
                },
                0,
                &ALL
            )
            .is_empty());
        assert!(
            h.on_msg(
                A,
                DialogueMsg::Pick {
                    session: s,
                    step: 2,
                    choice: 1
                },
                0,
                &ALL
            )
            .is_empty(),
            "skip"
        );
        h.on_msg(
            A,
            DialogueMsg::Pick {
                session: s,
                step: 1,
                choice: 1,
            },
            0,
            &ALL,
        );
        assert!(
            h.on_msg(
                A,
                DialogueMsg::Pick {
                    session: s,
                    step: 2,
                    choice: 1
                },
                0,
                &ALL
            )
            .is_empty(),
            "mid-step"
        );
        done_all(&mut h, s, 1);
        assert!(!h
            .on_msg(
                A,
                DialogueMsg::Pick {
                    session: s,
                    step: 2,
                    choice: 4
                },
                20,
                &ALL
            )
            .is_empty());
        assert!(
            h.on_msg(
                A,
                DialogueMsg::Pick {
                    session: s,
                    step: 1,
                    choice: 4
                },
                30,
                &ALL
            )
            .is_empty(),
            "replay"
        );
    }

    #[test]
    fn denials() {
        let mut h = DialogueHost::new();
        let s = started(&mut h, A);
        let deny = |h: &mut DialogueHost, who, npc| match h.on_msg(
            who,
            DialogueMsg::Open { req: 2, npc },
            1,
            &ALL,
        )[0]
        .msg
        {
            DialogueMsg::Denied { reason, .. } => reason,
            ref m => panic!("{m:?}"),
        };
        assert_eq!(deny(&mut h, B, NPC), Deny::Busy);
        assert_eq!(deny(&mut h, A, FormId(5)), Deny::AlreadyTalking);
        h.set_in_combat(FormId(5), true);
        assert_eq!(deny(&mut h, B, FormId(5)), Deny::NpcInCombat);
        h.npc_died(FormId(6));
        assert_eq!(deny(&mut h, B, FormId(6)), Deny::NpcDead);
        h.on_msg(A, DialogueMsg::Close { session: s }, 2, &ALL);
        assert!(matches!(
            h.on_msg(B, DialogueMsg::Open { req: 3, npc: NPC }, 3, &ALL)[0].msg,
            DialogueMsg::Started { .. }
        ));
    }

    #[test]
    fn initiator_disconnect_flushes_end_script_and_closes() {
        let mut h = DialogueHost::new();
        let s = started(&mut h, A);
        h.on_msg(
            A,
            DialogueMsg::Pick {
                session: s,
                step: 1,
                choice: 3,
            },
            1,
            &ALL,
        );
        let o = h.on_disconnect(A);
        assert!(matches!(
            o[0].msg,
            DialogueMsg::Closed {
                reason: CloseReason::InitiatorLeft,
                ..
            }
        ));
        let ev = h.take_events();
        let kinds: Vec<_> = ev.iter().map(|e| std::mem::discriminant(e)).collect();
        assert_eq!(kinds.len(), 3, "{ev:?}");
        assert!(matches!(ev[1], DialogueEvent::RunEnd { .. }));
        assert_eq!(h.open_sessions(), 0);
        assert!(h.session_of(A).is_none());
    }

    #[test]
    fn spectator_disconnect_releases_barrier() {
        let mut h = DialogueHost::new();
        let s = started(&mut h, A);
        h.on_msg(
            A,
            DialogueMsg::Pick {
                session: s,
                step: 1,
                choice: 3,
            },
            1,
            &ALL,
        );
        h.on_msg(
            H,
            DialogueMsg::LineDone {
                session: s,
                step: 1,
            },
            2,
            &ALL,
        );
        h.on_msg(
            A,
            DialogueMsg::LineDone {
                session: s,
                step: 1,
            },
            2,
            &ALL,
        );
        let o = h.on_disconnect(B);
        assert!(matches!(o[0].msg, DialogueMsg::StepEnd { .. }));
    }

    #[test]
    fn slow_spectator_times_out() {
        let mut h = DialogueHost::new();
        let s = started(&mut h, A);
        h.on_msg(
            A,
            DialogueMsg::Pick {
                session: s,
                step: 1,
                choice: 3,
            },
            0,
            &ALL,
        );
        h.on_msg(
            A,
            DialogueMsg::LineDone {
                session: s,
                step: 1,
            },
            1,
            &ALL,
        );
        h.on_msg(
            A,
            DialogueMsg::Heartbeat { session: s },
            LINE_TIMEOUT_MS - 1,
            &ALL,
        );
        assert!(h.tick(LINE_TIMEOUT_MS - 1).is_empty());
        let o = h.tick(LINE_TIMEOUT_MS);
        assert!(matches!(o[0].msg, DialogueMsg::StepEnd { .. }));
    }

    #[test]
    fn heartbeat_loss_and_lease() {
        let mut h = DialogueHost::new();
        let s = started(&mut h, A);
        h.on_msg(A, DialogueMsg::Heartbeat { session: s }, 20_000, &ALL);
        assert!(h.tick(60_000).is_empty());
        assert!(matches!(
            h.tick(65_000)[0].msg,
            DialogueMsg::Closed {
                reason: CloseReason::HeartbeatLost,
                ..
            }
        ));
        let s = started(&mut h, B);
        for t in (70_000..LEASE_MS).step_by(15_000) {
            h.on_msg(B, DialogueMsg::Heartbeat { session: s }, t, &ALL);
            assert!(h.tick(t).is_empty());
        }
        let t = LEASE_MS;
        h.on_msg(B, DialogueMsg::Heartbeat { session: s }, t, &ALL);
        assert!(matches!(
            h.tick(t)[0].msg,
            DialogueMsg::Closed {
                reason: CloseReason::LeaseExpired,
                ..
            }
        ));
    }

    #[test]
    fn npc_death_closes_session() {
        let mut h = DialogueHost::new();
        started(&mut h, A);
        let o = h.npc_died(NPC);
        assert!(matches!(
            o[0].msg,
            DialogueMsg::Closed {
                reason: CloseReason::NpcDied,
                ..
            }
        ));
        assert!(h.npc_died(NPC).is_empty());
    }

    #[test]
    fn a_refused_pick_tells_only_the_picker_and_keeps_the_step_open() {
        let mut h = DialogueHost::new();
        let s = started(&mut h, A);
        assert_eq!(h.npc_of(s), Some(NPC));
        let o = h.refuse(A, s, 1, 9);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].aud, Audience::To(A));
        assert!(matches!(
            o[0].msg,
            DialogueMsg::Refused {
                step: 1,
                choice: 9,
                ..
            }
        ));
        // nothing ran, and the same step can still be picked
        assert!(h.take_events().is_empty());
        let pick = DialogueMsg::Pick {
            session: s,
            step: 1,
            choice: 8,
        };
        assert!(matches!(
            h.on_msg(A, pick, 5, &ALL)[0].msg,
            DialogueMsg::Picked { choice: 8, .. }
        ));
        // a spectator, a wrong step or a step in play gets no answer
        assert!(h.refuse(B, s, 2, 9).is_empty());
        assert!(h.refuse(A, s, 2, 9).is_empty());
        let mut c = super::super::DialogueClient::new(A);
        c.on_msg(
            DialogueMsg::Started {
                session: s,
                npc: NPC,
                initiator: A,
                req: 1,
            },
            0,
        );
        c.on_msg(o[0].msg.clone(), 1);
        assert_eq!((c.refused, c.last_refused), (1, Some((s, 9))));
    }

    fn offer(h: &mut DialogueHost, who: PlayerId, s: u32, step: u32, c: &[u32]) -> Vec<DOut> {
        h.on_msg(
            who,
            DialogueMsg::Options {
                session: s,
                step,
                choices: c.to_vec(),
            },
            1,
            &ALL,
        )
    }

    fn item(choice: u32, owners: &[PlayerId]) -> MenuItem {
        MenuItem {
            choice,
            owners: owners.to_vec(),
        }
    }

    #[test]
    fn every_players_options_are_merged_the_initiators_first() {
        let mut h = DialogueHost::new();
        let s = started(&mut h, A);
        // B's first: its own list, each option once.
        let o = offer(&mut h, B, s, 1, &[3, 2, 3]);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].aud, Audience::All);
        assert_eq!(
            o[0].msg,
            DialogueMsg::Menu {
                session: s,
                step: 1,
                items: vec![item(3, &[B]), item(2, &[B])]
            }
        );
        // The initiator's: first, in its order; a shared option has both, the initiator first.
        let o = offer(&mut h, A, s, 1, &[1, 2]);
        assert_eq!(
            o[0].msg,
            DialogueMsg::Menu {
                session: s,
                step: 1,
                items: vec![item(1, &[A]), item(2, &[A, B]), item(3, &[B])]
            }
        );
        // The same again: nothing sent. For another step, or from no one in it: ignored.
        assert!(offer(&mut h, A, s, 1, &[1, 2]).is_empty());
        assert!(offer(&mut h, B, s, 2, &[9]).is_empty());
        assert!(h
            .on_msg(
                PlayerId(7),
                DialogueMsg::Options {
                    session: s,
                    step: 1,
                    choices: vec![9]
                },
                1,
                &ALL
            )
            .is_empty());
        // B has none now: its options leave the list.
        let o = offer(&mut h, B, s, 1, &[]);
        assert_eq!(
            o[0].msg,
            DialogueMsg::Menu {
                session: s,
                step: 1,
                items: vec![item(1, &[A]), item(2, &[A])]
            }
        );
        // At most OPTIONS_MAX of one player's.
        let many: Vec<u32> = (100..100 + OPTIONS_MAX as u32 + 5).collect();
        let o = offer(&mut h, H, s, 1, &many);
        let DialogueMsg::Menu { items, .. } = &o[0].msg else {
            panic!()
        };
        assert_eq!(items.len(), 2 + OPTIONS_MAX);
    }

    #[test]
    fn a_player_who_offered_options_picks_and_the_step_starts_anew() {
        let mut h = DialogueHost::new();
        let s = started(&mut h, A);
        // H offered nothing: its pick is not taken.
        let pick = |h: &mut DialogueHost, who, step, choice| {
            h.on_msg(
                who,
                DialogueMsg::Pick {
                    session: s,
                    step,
                    choice,
                },
                5,
                &ALL,
            )
        };
        assert!(pick(&mut h, H, 1, 3).is_empty());
        offer(&mut h, B, s, 1, &[3]);
        // Nor one of a player not in the conversation, though it offered.
        assert!(h
            .on_msg(
                B,
                DialogueMsg::Pick {
                    session: s,
                    step: 1,
                    choice: 3
                },
                5,
                &[H, A]
            )
            .is_empty());
        let o = pick(&mut h, B, 1, 3);
        assert_eq!(
            o[0].msg,
            DialogueMsg::Picked {
                session: s,
                step: 1,
                choice: 3
            }
        );
        assert!(matches!(
            h.take_events()[0],
            DialogueEvent::RunBegin {
                choice: 3,
                initiator: B,
                ..
            }
        ));
        done_all(&mut h, s, 1);
        // The next step: B's options of the last step are gone, so B picks no more until it
        // offers again.
        assert!(pick(&mut h, B, 2, 3).is_empty());
        assert!(
            !pick(&mut h, A, 2, 4).is_empty(),
            "the initiator always picks"
        );
    }

    #[test]
    fn a_pick_of_another_players_option_asks_its_owner_once() {
        let mut h = DialogueHost::new();
        let s = started(&mut h, A);
        offer(&mut h, A, s, 1, &[1, 2]);
        offer(&mut h, B, s, 1, &[2, 3]);
        let want = |h: &mut DialogueHost, who, choice, now| {
            h.on_msg(
                who,
                DialogueMsg::Want {
                    session: s,
                    step: 1,
                    choice,
                },
                now,
                &ALL,
            )
        };
        // An option no one offers, or one of the picker's own: refused to the picker.
        for (who, choice) in [(H, 9), (B, 3)] {
            let o = want(&mut h, who, choice, 1);
            assert_eq!(o[0].aud, Audience::To(who));
            assert_eq!(
                o[0].msg,
                DialogueMsg::Refused {
                    session: s,
                    step: 1,
                    choice
                }
            );
        }
        // H picks 2 (A's and B's): the initiator is asked.
        let o = want(&mut h, H, 2, 1);
        assert_eq!(o[0].aud, Audience::To(A));
        assert_eq!(
            o[0].msg,
            DialogueMsg::Ask {
                session: s,
                step: 1,
                choice: 2,
                by: H
            }
        );
        // One at a time.
        assert!(want(&mut h, B, 1, 2).is_empty());
        // Not said in time: refused to H, and another may be asked.
        assert!(h.tick(1 + ASK_TIMEOUT_MS - 1).is_empty());
        let o = h.tick(1 + ASK_TIMEOUT_MS);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].aud, Audience::To(H));
        assert!(matches!(o[0].msg, DialogueMsg::Refused { choice: 2, .. }));
        // A picks 3 (B's): B asked; B leaves before saying it: refused to A, and B's options
        // leave the list.
        let o = want(&mut h, A, 3, 20_000);
        assert_eq!(o[0].aud, Audience::To(B));
        let o = h.on_disconnect(B);
        assert!(o
            .iter()
            .any(|d| d.aud == Audience::To(A)
                && matches!(d.msg, DialogueMsg::Refused { choice: 3, .. })));
        assert!(o.iter().any(|d| d.msg
            == DialogueMsg::Menu {
                session: s,
                step: 1,
                items: vec![item(1, &[A]), item(2, &[A])]
            }));
    }

    #[test]
    fn an_asked_owners_pick_is_taken_and_ends_the_ask() {
        let mut h = DialogueHost::new();
        let s = started(&mut h, A);
        offer(&mut h, B, s, 1, &[3]);
        h.on_msg(
            A,
            DialogueMsg::Want {
                session: s,
                step: 1,
                choice: 3,
            },
            1,
            &ALL,
        );
        // B's game said 3's failure line (a speech check): that is the step's pick.
        let o = h.on_msg(
            B,
            DialogueMsg::Pick {
                session: s,
                step: 1,
                choice: 33,
            },
            2,
            &ALL,
        );
        assert!(matches!(o[0].msg, DialogueMsg::Picked { choice: 33, .. }));
        // No refusal later: the ask is over.
        assert!(h
            .tick(2 + ASK_TIMEOUT_MS)
            .iter()
            .all(|d| !matches!(d.msg, DialogueMsg::Refused { .. })));
    }

    #[test]
    fn options_and_wants_mid_step_are_ignored() {
        let mut h = DialogueHost::new();
        let s = started(&mut h, A);
        h.on_msg(
            A,
            DialogueMsg::Pick {
                session: s,
                step: 1,
                choice: 9,
            },
            5,
            &ALL,
        );
        assert!(offer(&mut h, B, s, 2, &[3]).is_empty());
        assert!(h
            .on_msg(
                B,
                DialogueMsg::Want {
                    session: s,
                    step: 2,
                    choice: 9
                },
                6,
                &ALL
            )
            .is_empty());
    }

    #[test]
    fn an_owner_not_in_the_conversation_is_not_asked_and_a_leaving_asker_frees_the_ask() {
        let mut h = DialogueHost::new();
        let s = started(&mut h, A);
        offer(&mut h, B, s, 1, &[3, 3, 4]);
        // B walked off (still connected): its option is not asked of it.
        let o = h.on_msg(
            A,
            DialogueMsg::Want {
                session: s,
                step: 1,
                choice: 3,
            },
            1,
            &[H, A],
        );
        assert!(matches!(o[0].msg, DialogueMsg::Refused { choice: 3, .. }));
        // H asks B for 4, then leaves: the ask is over, A may ask at once.
        let o = h.on_msg(
            H,
            DialogueMsg::Want {
                session: s,
                step: 1,
                choice: 4,
            },
            1,
            &ALL,
        );
        assert_eq!(o[0].aud, Audience::To(B));
        h.on_disconnect(H);
        let o = h.on_msg(
            A,
            DialogueMsg::Want {
                session: s,
                step: 1,
                choice: 3,
            },
            2,
            &[A, B],
        );
        assert_eq!(o[0].aud, Audience::To(B));
        assert!(matches!(
            o[0].msg,
            DialogueMsg::Ask {
                choice: 3,
                by: A,
                ..
            }
        ));
    }
}
