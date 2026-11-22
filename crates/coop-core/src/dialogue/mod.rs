//! Dialogue session protocol. The initiator picks, the host runs each result
//! script once, everyone else spectates behind a per-step barrier.
//!
//! Host and client are pure state machines. Result scripts are not run here:
//! the host emits [`DialogueEvent`]s for the game to act on.

mod client;
mod host;
pub mod votes;

use serde::{Deserialize, Serialize};

use crate::ids::{FormId, PlayerId};

pub use client::{Asked, DialogueClient, View};
pub use host::{DOut, DialogueHost};

/// Initiator heartbeat period.
pub const HEARTBEAT_MS: u64 = 15_000;
/// Silence after which the initiator counts as gone.
pub const HEARTBEAT_MISS_MS: u64 = 45_000;
/// A session with no pick for this long is closed.
pub const LEASE_MS: u64 = 300_000;
/// Longest a step waits for spectators to finish their lines.
pub const LINE_TIMEOUT_MS: u64 = 30_000;
/// Longest the table waits for a player asked to say another player's pick of its option.
pub const ASK_TIMEOUT_MS: u64 = 10_000;
/// Most options one player's game may offer for a step.
pub const OPTIONS_MAX: usize = 64;
/// How long a vote stays open after its first vote (D13, the owner: 20 seconds): then the
/// starter's pick goes through, or the first vote when the starter did not vote.
pub const VOTE_MS: u64 = 20_000;

/// One option of a conversation's merged list (S42 step 2): an info a player may say next, and
/// the players whose game offered it, the one the table asks to say it first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MenuItem {
    /// The info (`TESTopicInfo`) the option says.
    pub choice: u32,
    /// The players who may say it.
    pub owners: Vec<PlayerId>,
}

/// Why an open was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Deny {
    /// Someone else is talking to this NPC.
    Busy,
    /// The requester already has a session.
    AlreadyTalking,
    /// The NPC is dead.
    NpcDead,
    /// The NPC is in combat.
    NpcInCombat,
}

/// Why a session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CloseReason {
    /// The initiator ended it.
    Finished,
    /// The initiator disconnected.
    InitiatorLeft,
    /// No heartbeat from the initiator.
    HeartbeatLost,
    /// No pick within the lease.
    LeaseExpired,
    /// The NPC died.
    NpcDied,
}

/// Dialogue wire messages (all reliable).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DialogueMsg {
    /// Initiator asks to talk to an NPC.
    Open {
        /// Request id.
        req: u32,
        /// The NPC.
        npc: FormId,
    },
    /// Host refuses an open.
    Denied {
        /// Request id.
        req: u32,
        /// Reason.
        reason: Deny,
    },
    /// Host: a session began (initiator and spectators).
    Started {
        /// Session id.
        session: u32,
        /// The NPC.
        npc: FormId,
        /// Who picks.
        initiator: PlayerId,
        /// Request id this answers (meaningful to the initiator).
        req: u32,
    },
    /// Initiator picks a choice for the next step.
    Pick {
        /// Session.
        session: u32,
        /// Step number, strictly increasing from 1.
        step: u32,
        /// Choice id (topic info).
        choice: u32,
    },
    /// Host: step accepted, everyone plays the lines.
    Picked {
        /// Session.
        session: u32,
        /// Step.
        step: u32,
        /// Choice.
        choice: u32,
        /// The player who picked it (its subtitle names that player).
        by: PlayerId,
    },
    /// The game that said a step's line, then the host to the others: the NPC began response
    /// `n` (from 0; the line's start is response 0) of it, so the others show that response now
    /// (the owner, 2026-11-22: a watching player's subtitles were a response behind the voice).
    Response {
        /// Session.
        session: u32,
        /// Step.
        step: u32,
        /// The response begun (1 for the second).
        n: u32,
    },
    /// A peer finished playing a step's lines.
    LineDone {
        /// Session.
        session: u32,
        /// Step.
        step: u32,
    },
    /// Host: barrier passed and the end script ran.
    StepEnd {
        /// Session.
        session: u32,
        /// Step.
        step: u32,
    },
    /// Initiator keep-alive.
    Heartbeat {
        /// Session.
        session: u32,
    },
    /// Initiator ends the conversation.
    Close {
        /// Session.
        session: u32,
    },
    /// Host: the session is over.
    Closed {
        /// Session.
        session: u32,
        /// Why.
        reason: CloseReason,
    },
    /// A player in the conversation: the infos its player may say for step
    /// `step` (S42 step 2), in its game's order.
    Options {
        /// Session.
        session: u32,
        /// The step these are for (the next pick's).
        step: u32,
        /// The infos.
        choices: Vec<u32>,
    },
    /// Host: the conversation's options for step `step`, every player's
    /// merged (the initiator's first, in its order).
    Menu {
        /// Session.
        session: u32,
        /// Step.
        step: u32,
        /// The options.
        items: Vec<MenuItem>,
    },
    /// A player picks another player's option: the table asks an owner to say it.
    Want {
        /// Session.
        session: u32,
        /// Step.
        step: u32,
        /// The option (an info of the merged list).
        choice: u32,
    },
    /// Host to an option's owner: player `by` picked it; say it (the owner's
    /// game resolves it with its own player, then picks as usual).
    Ask {
        /// Session.
        session: u32,
        /// Step.
        step: u32,
        /// The option.
        choice: u32,
        /// Who picked it.
        by: PlayerId,
    },
    /// Host to the picker: the game refused the pick (the line's conditions
    /// fail for the picker). The step stays open.
    Refused {
        /// Session.
        session: u32,
        /// The step the pick asked for.
        step: u32,
        /// The refused choice.
        choice: u32,
    },
    /// A player joins a conversation another player started (S42, D12: it talked to the NPC).
    Join {
        /// Session.
        session: u32,
    },
    /// A player leaves a conversation (its "[Leave conversation]", or it walked away); the
    /// conversation goes on for the others, and ends when no one is left in it.
    Leave {
        /// Session.
        session: u32,
    },
    /// Host: the players now in a conversation (after a join or a leave), in the order they came.
    Joined {
        /// Session.
        session: u32,
        /// The players in it.
        players: Vec<PlayerId>,
    },
    /// A player's game: its player is now free, in a menu or fighting (sent when it changes;
    /// D13: a conversation near a busy player waits for it).
    Busy {
        /// What the player is doing.
        busy: Busy,
    },
    /// Host: a player is now free, in a menu or fighting.
    PlayerBusy {
        /// The player.
        player: PlayerId,
        /// What it is doing.
        busy: Busy,
    },
    /// A player's game: its player talked to NPC `npc` and waits for a busy player near it (D13).
    Waiting {
        /// The NPC.
        npc: FormId,
    },
    /// Host: player `player` waits to talk to NPC `npc` (the busy player near it is told).
    WaitingFor {
        /// The player who waits.
        player: PlayerId,
        /// The NPC.
        npc: FormId,
    },
    /// A player in the conversation votes for option `choice` of step `step`, a line that matters
    /// (S42 step 3b): its click is held until the vote is decided. A later vote of the same
    /// player replaces its earlier one.
    Vote {
        /// Session.
        session: u32,
        /// Step.
        step: u32,
        /// The option voted for (an info of the merged list).
        choice: u32,
    },
    /// Host: the votes so far for step `step`, in the order the players first voted, and how
    /// many players vote; the vote closes `left_ms` from now.
    Votes {
        /// Session.
        session: u32,
        /// Step.
        step: u32,
        /// Each voter's option.
        votes: Vec<(PlayerId, u32)>,
        /// How many players vote (the players in the game, but those who left the
        /// conversation).
        of: u8,
        /// Milliseconds until the vote closes.
        left_ms: u64,
    },
    /// Host: option `choice` was chosen for step `step` (its owner is asked to say it).
    Chosen {
        /// Session.
        session: u32,
        /// Step.
        step: u32,
        /// The option.
        choice: u32,
    },
    /// Host, to the player who picked a line: the line's result opens a menu with `npc` (a
    /// trade, repairs, ...) in that player's game, not in the host's, which ran the rest of the
    /// result (the owner, 2026-11-22: the guest's trade window opened on the host's screen).
    OpenMenu {
        /// The NPC (the reference the result's command ran on).
        npc: FormId,
        /// Which menu (`coop_engine::menus::Menu` as a byte).
        menu: u8,
        /// The command's value (a trade's discount, a recipe category's form id, ...).
        value: u32,
    },
}

/// What a player is doing, for a conversation that would take it in (D13).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Busy {
    /// Free: a conversation near it takes it in.
    #[default]
    Free,
    /// In a menu (the Pip-Boy, a container, a trade): a conversation near it waits.
    Menu,
    /// In a fight: a conversation near it waits.
    Combat,
    /// In a conversation: it is not waited for.
    Talking,
}

/// What the game must do on the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DialogueEvent {
    /// Run the begin script of `choice` (once).
    RunBegin {
        /// Session.
        session: u32,
        /// NPC.
        npc: FormId,
        /// Step.
        step: u32,
        /// Choice.
        choice: u32,
        /// Who picked.
        initiator: PlayerId,
    },
    /// Run the end script of the step's choice (once).
    RunEnd {
        /// Session.
        session: u32,
        /// NPC.
        npc: FormId,
        /// Step.
        step: u32,
        /// Choice.
        choice: u32,
    },
    /// Release the NPC and the control locks.
    Closed {
        /// Session.
        session: u32,
        /// NPC.
        npc: FormId,
        /// Why.
        reason: CloseReason,
    },
}
