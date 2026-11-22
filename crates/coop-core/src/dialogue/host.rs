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
    /// Who picked the last step: its line may end the conversation in that player's game, which
    /// then ends it for every player (S42 step 2c).
    last_picker: Option<PlayerId>,
    /// Each player's options for the next step (S42 step 2).
    offers: BTreeMap<PlayerId, Vec<u32>>,
    /// The merged options last sent.
    menu: Vec<MenuItem>,
    asking: Option<Asking>,
    /// The players in the conversation (S42, D12: the initiator, then whoever joined by talking
    /// to the NPC, in that order), until they leave. Only they pick, offer options and vote, and
    /// only they hold a step's end.
    joined: Vec<PlayerId>,
}

impl Session {
    /// The player who leads the conversation (S42, D12): its initiator while it is in it, else
    /// the player in it the longest. It picks without offering options first, may end the
    /// conversation, and decides a tie (step 3b).
    fn lead(&self) -> Option<PlayerId> {
        if self.joined.contains(&self.initiator) {
            Some(self.initiator)
        } else {
            self.joined.first().copied()
        }
    }

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
    /// The conversation each player is in (one at a time).
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

    /// The conversation the player is in, if any.
    pub fn session_of(&self, p: PlayerId) -> Option<u32> {
        self.talking.get(&p).copied()
    }

    /// The NPC of an open session.
    pub fn npc_of(&self, session: u32) -> Option<FormId> {
        self.sessions.get(&session).map(|s| s.npc)
    }

    /// The players in an open session's conversation, the initiator first if it is still in.
    pub fn players_in(&self, session: u32) -> &[PlayerId] {
        self.sessions
            .get(&session)
            .map_or(&[], |s| s.joined.as_slice())
    }

    /// The game refused a pick (the line's conditions fail for the picker).
    /// The step stays open, no script runs, and only the picker is told.
    /// A pick the table would ignore anyway gets no answer.
    pub fn refuse(&mut self, from: PlayerId, session: u32, step: u32, choice: u32) -> Vec<DOut> {
        let open = self.sessions.get(&session).is_some_and(|s| {
            s.joined.contains(&from)
                && (s.lead() == Some(from) || s.offers.contains_key(&from))
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
                // From any player in the conversation: it goes on when the initiator left it.
                if let Some(s) = self
                    .sessions
                    .get_mut(&session)
                    .filter(|s| s.joined.contains(&from))
                {
                    s.last_beat_ms = now_ms;
                }
                vec![]
            }
            DialogueMsg::Close { session } => {
                // The lead, or the player whose line was the last step's (it ended the
                // conversation in that player's game), while in the conversation.
                if self.sessions.get(&session).is_some_and(|s| {
                    s.joined.contains(&from)
                        && (s.lead() == Some(from) || s.last_picker == Some(from))
                }) {
                    self.close(session, CloseReason::Finished)
                } else {
                    vec![]
                }
            }
            DialogueMsg::Join { session } => self.join(from, session, members),
            DialogueMsg::Leave { session } => {
                if self
                    .sessions
                    .get(&session)
                    .is_some_and(|s| s.joined.contains(&from))
                {
                    self.drop_player(session, from, CloseReason::Finished)
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
                last_picker: None,
                offers: BTreeMap::new(),
                menu: Vec::new(),
                asking: None,
                joined: vec![from],
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
        // The lead (the initiator, or with it gone the player in the conversation longest) picks;
        // so does any player in the conversation who offered options for this step (S42 step 2:
        // its game said one of its own, or one it was asked to say). Only while in the
        // conversation (D12).
        let Some(s) = self.sessions.get_mut(&session).filter(|s| {
            s.joined.contains(&from)
                && members.contains(&from)
                && (s.lead() == Some(from) || s.offers.contains_key(&from))
        }) else {
            return vec![];
        };
        // duplicates (step already taken), skips and picks mid-step are ignored
        if s.playing.is_some() || step != s.step + 1 {
            return vec![];
        }
        s.step = step;
        s.last_picker = Some(from);
        s.offers.clear();
        s.menu.clear();
        s.asking = None;
        s.last_pick_ms = now_ms;
        s.last_beat_ms = now_ms;
        // Only the players in the conversation hold the step's end (a player gone is out of it);
        // the others follow it as text and wait for nothing.
        s.playing = Some(Playing {
            step,
            choice,
            started_ms: now_ms,
            waiting: s.joined.iter().copied().collect(),
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
                by: from,
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
        if !(s.joined.contains(&from) && members.contains(&from))
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
        if !(s.joined.contains(&from) && members.contains(&from))
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
            .filter(|o| *o != from && members.contains(o));
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
        for p in &s.joined {
            self.talking.remove(p);
        }
        self.events.push(DialogueEvent::Closed {
            session,
            npc: s.npc,
            reason,
        });
        vec![out(Audience::All, DialogueMsg::Closed { session, reason })]
    }

    /// A peer left. It leaves the conversation it was in (which ends when no one is left in
    /// it); anyone else just stops counting for barriers.
    pub fn on_disconnect(&mut self, p: PlayerId) -> Vec<DOut> {
        let mut res = Vec::new();
        if let Some(s) = self.talking.get(&p).copied() {
            res.extend(self.drop_player(s, p, CloseReason::InitiatorLeft));
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

    /// A player joins a conversation (D12: it talked to the NPC): not one it is in, nor while in
    /// another. Everyone is told who is in it.
    fn join(&mut self, from: PlayerId, session: u32, members: &[PlayerId]) -> Vec<DOut> {
        if !members.contains(&from) || self.talking.contains_key(&from) {
            return vec![];
        }
        let Some(s) = self.sessions.get_mut(&session) else {
            return vec![];
        };
        s.joined.push(from);
        let players = s.joined.clone();
        self.talking.insert(from, session);
        vec![out(Audience::All, DialogueMsg::Joined { session, players })]
    }

    /// A player is out of a conversation (it left, or it is gone): its options, its ask and its
    /// part in the step go with it; the conversation ends with `reason` when no one is left in
    /// it, else everyone is told who is in it.
    fn drop_player(&mut self, session: u32, p: PlayerId, reason: CloseReason) -> Vec<DOut> {
        let Some(s) = self.sessions.get_mut(&session) else {
            return vec![];
        };
        s.joined.retain(|j| *j != p);
        self.talking.remove(&p);
        if s.joined.is_empty() {
            return self.close(session, reason);
        }
        let players = s.joined.clone();
        let mut res = vec![out(Audience::All, DialogueMsg::Joined { session, players })];
        if let Some(pl) = s.playing.as_mut() {
            pl.waiting.remove(&p);
        }
        if s.asking.as_ref().is_some_and(|a| a.by == p) {
            s.asking = None;
        }
        let lapsed = s.asking.as_ref().is_some_and(|a| a.owner == p);
        if s.offers.remove(&p).is_some() {
            let merged = s.merged();
            if merged != s.menu {
                s.menu = merged.clone();
                res.push(out(
                    Audience::All,
                    DialogueMsg::Menu {
                        session,
                        step: s.step + 1,
                        items: merged,
                    },
                ));
            }
        }
        if lapsed {
            res.extend(self.ask_lapsed(session));
        }
        res.extend(self.maybe_finish_step(session));
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

    /// A conversation `who` started that every other player joined.
    fn started_all(h: &mut DialogueHost, who: PlayerId) -> u32 {
        let s = started(h, who);
        for p in ALL.into_iter().filter(|p| *p != who) {
            let o = h.on_msg(p, DialogueMsg::Join { session: s }, 0, &ALL);
            assert!(matches!(o[0].msg, DialogueMsg::Joined { .. }), "{o:?}");
        }
        s
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
        let s = started_all(&mut h, A);
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
        let s = started_all(&mut h, A);
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
        // B was in the conversation: it is out, and the step ends.
        let o = h.on_disconnect(B);
        assert!(matches!(
            o[0].msg,
            DialogueMsg::Joined { ref players, .. } if *players == [A, H]
        ));
        assert!(matches!(o.last().unwrap().msg, DialogueMsg::StepEnd { .. }));
    }

    #[test]
    fn slow_spectator_times_out() {
        let mut h = DialogueHost::new();
        let s = started_all(&mut h, A);
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
        let s = started_all(&mut h, A);
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
        let s = started_all(&mut h, A);
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
                choice: 3,
                by: B,
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
        let s = started_all(&mut h, A);
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
        let s = started_all(&mut h, A);
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
        let s = started_all(&mut h, A);
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

    #[test]
    fn the_player_whose_line_was_the_last_step_may_end_the_conversation() {
        let mut h = DialogueHost::new();
        let s = started_all(&mut h, A);
        let close =
            |h: &mut DialogueHost, who| h.on_msg(who, DialogueMsg::Close { session: s }, 9, &ALL);
        // B picked nothing: its close is not taken.
        assert!(close(&mut h, B).is_empty());
        offer(&mut h, B, s, 1, &[3]);
        h.on_msg(
            B,
            DialogueMsg::Pick {
                session: s,
                step: 1,
                choice: 3,
            },
            5,
            &ALL,
        );
        // H did not pick the last step: not taken either.
        assert!(close(&mut h, H).is_empty());
        let o = close(&mut h, B);
        assert!(matches!(
            o[0].msg,
            DialogueMsg::Closed {
                reason: CloseReason::Finished,
                ..
            }
        ));
        assert!(!h.is_talking(NPC));
        // Every player who was in it is free to start another.
        for p in ALL {
            assert_eq!(h.session_of(p), None);
            let npc = FormId(0x80 + u32::from(p.0));
            let o = h.on_msg(p, DialogueMsg::Open { req: 9, npc }, 10, &ALL);
            assert!(matches!(o[0].msg, DialogueMsg::Started { .. }), "{o:?}");
        }
    }

    fn msg(h: &mut DialogueHost, from: PlayerId, m: DialogueMsg) -> Vec<DOut> {
        h.on_msg(from, m, 5, &ALL)
    }

    #[test]
    fn only_players_who_joined_take_part_and_hold_a_step() {
        let mut h = DialogueHost::new();
        let s = started(&mut h, A);
        assert_eq!(h.players_in(s), [A]);
        // B is near but did not join: its options, picks and wants are not taken.
        assert!(msg(
            &mut h,
            B,
            DialogueMsg::Options {
                session: s,
                step: 1,
                choices: vec![3]
            }
        )
        .is_empty());
        assert!(msg(
            &mut h,
            B,
            DialogueMsg::Want {
                session: s,
                step: 1,
                choice: 3
            }
        )
        .is_empty());
        assert!(msg(
            &mut h,
            B,
            DialogueMsg::Pick {
                session: s,
                step: 1,
                choice: 3
            }
        )
        .is_empty());
        assert!(h.refuse(B, s, 1, 3).is_empty());
        // The step's end waits for A alone.
        msg(
            &mut h,
            A,
            DialogueMsg::Pick {
                session: s,
                step: 1,
                choice: 9,
            },
        );
        let o = msg(
            &mut h,
            A,
            DialogueMsg::LineDone {
                session: s,
                step: 1,
            },
        );
        assert!(
            matches!(o[0].msg, DialogueMsg::StepEnd { step: 1, .. }),
            "{o:?}"
        );
        // B joins (it talked to the NPC): everyone is told, then B takes part.
        let o = msg(&mut h, B, DialogueMsg::Join { session: s });
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].aud, Audience::All);
        assert_eq!(
            o[0].msg,
            DialogueMsg::Joined {
                session: s,
                players: vec![A, B]
            }
        );
        assert_eq!(h.session_of(B), Some(s));
        assert!(
            msg(&mut h, B, DialogueMsg::Join { session: s }).is_empty(),
            "once"
        );
        assert!(!msg(
            &mut h,
            B,
            DialogueMsg::Options {
                session: s,
                step: 2,
                choices: vec![3]
            }
        )
        .is_empty());
        msg(
            &mut h,
            B,
            DialogueMsg::Pick {
                session: s,
                step: 2,
                choice: 3,
            },
        );
        assert!(msg(
            &mut h,
            A,
            DialogueMsg::LineDone {
                session: s,
                step: 2
            }
        )
        .is_empty());
        let o = msg(
            &mut h,
            B,
            DialogueMsg::LineDone {
                session: s,
                step: 2,
            },
        );
        assert!(matches!(o[0].msg, DialogueMsg::StepEnd { step: 2, .. }));
        // A player gone is not joined, nor one in another conversation, nor an unknown one.
        assert!(h
            .on_msg(H, DialogueMsg::Join { session: s }, 5, &[A, B])
            .is_empty());
        assert!(msg(&mut h, H, DialogueMsg::Join { session: s + 9 }).is_empty());
        let mut h2 = DialogueHost::new();
        let s1 = started(&mut h2, A);
        let s2 = h2.on_msg(
            B,
            DialogueMsg::Open {
                req: 1,
                npc: FormId(0x78),
            },
            0,
            &ALL,
        );
        let DialogueMsg::Started { session: s2, .. } = s2[0].msg else {
            panic!()
        };
        assert!(msg(&mut h2, B, DialogueMsg::Join { session: s1 }).is_empty());
        assert_eq!(h2.players_in(s2), [B]);
    }

    #[test]
    fn a_player_who_leaves_is_out_and_the_conversation_goes_on_until_no_one_is_left() {
        let mut h = DialogueHost::new();
        let s = started_all(&mut h, A);
        offer(&mut h, B, s, 1, &[3]);
        offer(&mut h, A, s, 1, &[1]);
        // A non-member's leave: nothing.
        let mut h2 = DialogueHost::new();
        let s2 = started(&mut h2, A);
        assert!(msg(&mut h2, B, DialogueMsg::Leave { session: s2 }).is_empty());
        // B leaves: its options go, everyone is told who is in.
        let o = msg(&mut h, B, DialogueMsg::Leave { session: s });
        assert_eq!(
            o[0].msg,
            DialogueMsg::Joined {
                session: s,
                players: vec![A, H]
            }
        );
        assert!(o.iter().any(|m| matches!(
            &m.msg,
            DialogueMsg::Menu { items, .. } if items.iter().all(|i| i.choice != 3)
        )));
        assert_eq!(h.session_of(B), None);
        // The initiator leaves: the conversation goes on with H, who leads it now: it picks
        // without offering options, may end it and keeps it alive.
        msg(&mut h, A, DialogueMsg::Leave { session: s });
        assert_eq!(h.players_in(s), [H]);
        assert!(h.is_talking(NPC));
        let o = msg(
            &mut h,
            H,
            DialogueMsg::Pick {
                session: s,
                step: 1,
                choice: 5,
            },
        );
        assert!(
            matches!(
                o[0].msg,
                DialogueMsg::Picked {
                    step: 1,
                    choice: 5,
                    ..
                }
            ),
            "{o:?}"
        );
        msg(
            &mut h,
            H,
            DialogueMsg::LineDone {
                session: s,
                step: 1,
            },
        );
        // A, out of it, picks, closes and beats for nothing; it may start another conversation.
        assert!(msg(
            &mut h,
            A,
            DialogueMsg::Pick {
                session: s,
                step: 2,
                choice: 1
            }
        )
        .is_empty());
        assert!(msg(&mut h, A, DialogueMsg::Close { session: s }).is_empty());
        assert!(matches!(
            msg(
                &mut h,
                A,
                DialogueMsg::Open {
                    req: 2,
                    npc: FormId(0x78)
                }
            )[0]
            .msg,
            DialogueMsg::Started { .. }
        ));
        // A's heartbeat, out of it, keeps nothing alive; H's does.
        h.on_msg(
            A,
            DialogueMsg::Heartbeat { session: s },
            HEARTBEAT_MISS_MS - 1,
            &ALL,
        );
        assert!(h
            .clone()
            .tick(HEARTBEAT_MISS_MS + 10)
            .iter()
            .any(|m| matches!(m.msg, DialogueMsg::Closed { session, .. } if session == s)));
        h.on_msg(
            H,
            DialogueMsg::Heartbeat { session: s },
            HEARTBEAT_MISS_MS - 1,
            &ALL,
        );
        assert!(h
            .tick(HEARTBEAT_MISS_MS + 10)
            .iter()
            .all(|m| !matches!(m.msg, DialogueMsg::Closed { session, .. } if session == s)));
        // H leaves too: no one is left, it ends.
        let o = msg(&mut h, H, DialogueMsg::Leave { session: s });
        assert_eq!(
            o.last().unwrap().msg,
            DialogueMsg::Closed {
                session: s,
                reason: CloseReason::Finished
            }
        );
        assert!(!h.is_talking(NPC));
        assert_eq!(h.session_of(H), None);
    }

    #[test]
    fn a_joined_player_leaving_mid_step_frees_the_step_and_its_ask() {
        let mut h = DialogueHost::new();
        let s = started_all(&mut h, A);
        msg(
            &mut h,
            A,
            DialogueMsg::Pick {
                session: s,
                step: 1,
                choice: 9,
            },
        );
        msg(
            &mut h,
            A,
            DialogueMsg::LineDone {
                session: s,
                step: 1,
            },
        );
        msg(
            &mut h,
            H,
            DialogueMsg::LineDone {
                session: s,
                step: 1,
            },
        );
        let o = msg(&mut h, B, DialogueMsg::Leave { session: s });
        assert!(matches!(
            o.last().unwrap().msg,
            DialogueMsg::StepEnd { step: 1, .. }
        ));
        // An ask of B's option, B gone: refused to the player who picked it.
        let mut h = DialogueHost::new();
        let s = started_all(&mut h, A);
        offer(&mut h, B, s, 1, &[3]);
        msg(
            &mut h,
            A,
            DialogueMsg::Want {
                session: s,
                step: 1,
                choice: 3,
            },
        );
        let o = msg(&mut h, B, DialogueMsg::Leave { session: s });
        assert!(o
            .iter()
            .any(|m| m.aud == Audience::To(A)
                && matches!(m.msg, DialogueMsg::Refused { choice: 3, .. })));
        // B's own ask, B gone: dropped, so A may be asked by H.
        let mut h = DialogueHost::new();
        let s = started_all(&mut h, A);
        offer(&mut h, A, s, 1, &[1]);
        msg(
            &mut h,
            B,
            DialogueMsg::Want {
                session: s,
                step: 1,
                choice: 1,
            },
        );
        msg(&mut h, B, DialogueMsg::Leave { session: s });
        let o = msg(
            &mut h,
            H,
            DialogueMsg::Want {
                session: s,
                step: 1,
                choice: 1,
            },
        );
        assert!(matches!(o[0].msg, DialogueMsg::Ask { by: H, .. }), "{o:?}");
    }
}
