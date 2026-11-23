//! The session's part in conversations, party saves and rests (docs/coop/SPECS.md S16 to S19):
//! one step for the host and one for a guest, called by the drivers between two of the game's
//! frames. The pieces are [`crate::talk`], [`crate::party`] and [`crate::rest`].
//!
//! On the host with no guest nothing is shared: its conversations and rests are the game's own,
//! as without co-op.

use coop_core::ids::{Epoch, PlayerId};
use coop_core::proto::{Msg, MAX_INTENT_CHANGES};
use coop_core::schema::Field;
use coop_core::sync::GuestPhase;
use coop_core::transport::Transport;
use coop_engine::{rest as game_rest, saves, talk as game_talk};
use engine_core::Engine;

use crate::party::{PartyGuest, PartyHost, PART_UP_TAG};
use crate::rest::Rester;
use crate::talk::{Enactor, Talker};
use crate::{send, GuestDriver, HostDriver};

/// The host's state.
#[derive(Debug, Default)]
pub struct HostSocial {
    /// The host player's own conversations and the ones it watches.
    pub talker: Talker,
    /// The guests' lines the host's game runs.
    pub enactor: Enactor,
    /// The guests' parts and the party's files.
    pub party: PartyHost,
    /// The rest vote.
    pub rester: Rester,
    /// Steps whose end result ran this step (session, step): told to the guests as settled the
    /// next step, once the game's changes were collected.
    settles: Vec<(u32, u32)>,
    /// Steps told to the guests as settled, for the log.
    pub settles_told: u32,
}

/// A guest's state.
#[derive(Debug, Default)]
pub struct GuestSocial {
    /// This player's own conversations and the ones it watches.
    pub talker: Talker,
    /// This player's part.
    pub party: PartyGuest,
    /// The rest vote.
    pub rester: Rester,
    /// Menus the host asked this game to open that it could not, for the log (taken by the
    /// caller).
    pub menus_not_opened: Vec<String>,
    /// What the host's run of this player's lines takes from its player, not taken yet (no
    /// player yet).
    pub pays: Vec<coop_engine::pay::Payment>,
    /// Payments taken from this game's player (S30).
    pub paid: u32,
    /// What was taken, for the log (taken by the caller).
    pub pay_notes: Vec<String>,
}

/// Feeds a message of the host player's own game to the host's live layer and sends what it
/// answers to the guests (the host's own copies wait in the local inbox).
fn local(h: &mut HostDriver, net: &mut dyn Transport, msg: Msg) {
    for (to, m) in h.sync.on_local(msg) {
        send(net, to, &m, &mut h.send_failures);
    }
}

/// The host's step: after the guests' messages were taken, before their claims are enacted and
/// the game's changes collected.
pub(crate) fn host_step(h: &mut HostDriver, e: &mut Engine, net: &mut dyn Transport, now_ms: u64) {
    h.sync.claim_tags.insert(PART_UP_TAG);
    // What the live layer addressed to the host player.
    for msg in h.sync.live.take_local() {
        h.view.on_msg(msg, now_ms);
    }
    let players = h.sync.players();
    let shared = h.social_on && !players.is_empty();
    game_talk::set_sharing(e, shared);
    game_rest::set_sharing(e, shared);
    if !shared {
        // Nobody else's conversation to watch (S42).
        coop_engine::watch::end_watch(e);
    }
    if !h.social_on {
        return;
    }

    // The guests' parts: taken out of the claims before the driver judges the rest.
    let claims = std::mem::take(&mut h.sync.claims);
    for (from, form, field) in claims {
        if matches!(&field, Field::Custom { tag, .. } if *tag == PART_UP_TAG) {
            h.social.party.on_claim(from, &field);
        } else {
            h.sync.claims.push((from, form, field));
        }
    }
    let mut writes = h.social.party.on_players(&players);
    for ev in saves::take_events(e) {
        match ev {
            saves::Event::Saved(name) => h.social.party.on_saved(&name),
            saves::Event::Loaded(name) => writes.extend(h.social.party.on_loaded(&name)),
        }
    }
    for (form, field) in writes {
        h.sync.write(form, field);
    }

    // Conversations: the host player's own, the ones it watches, and the guests' results.
    let quests = &h.quests;
    let share = |e: &mut Engine, info: u32, _held: bool| {
        game_talk::info_quest(e, info).is_some_and(|q| quests.contains(&q))
    };
    h.social.talker.now_ms = now_ms;
    // The other players' avatars: a conversation near a busy one waits for it (D13).
    h.social.talker.avatars = h
        .ladder
        .as_ref()
        .map(crate::ladder::Ladder::avatar_ids)
        .unwrap_or_default();
    let msgs = h
        .social
        .talker
        .step(e, PlayerId::HOST, &mut h.view.dialogue, &share);
    for m in msgs {
        local(h, net, Msg::Dialogue(m));
    }
    // An actor another game runs came to greet a player: that player's game runs it now, unless
    // it is talking with someone (the owner, 2026-11-16: Sunny waited for the player whose game
    // ran her while the other stood in front of her).
    let mut asks: Vec<(u32, PlayerId)> = std::mem::take(&mut h.social.talker.greet_asks)
        .into_iter()
        .map(|npc| (npc, PlayerId::HOST))
        .collect();
    asks.extend(
        std::mem::take(&mut h.sync.live.actors.greets)
            .into_iter()
            .map(|(a, p)| (a.0, p)),
    );
    for (npc, p) in asks {
        let talking = h.sync.live.dialogue.is_talking(coop_core::ids::FormId(npc));
        if let Some(f) = h.fight.as_mut() {
            f.greet_asked(npc, p, talking, now_ms);
        }
    }
    // The steps whose end result ran last step: their changes are in the story now (collected
    // after that step's social part, the quest variables read again then).
    let rev = h.sync.store.rev().0;
    for (session, step) in std::mem::take(&mut h.social.settles) {
        let msg = Msg::Dialogue(coop_core::dialogue::DialogueMsg::Settled { session, step, rev });
        for p in h.sync.players() {
            send(net, p, &msg, &mut h.send_failures);
        }
        h.social.settles_told += 1;
    }
    for ev in h.sync.live.dialogue.take_events() {
        if let coop_core::dialogue::DialogueEvent::RunEnd { session, step, .. } = &ev {
            h.social.settles.push((*session, *step));
            h.sweep_vars_now = true;
        }
        if let coop_core::dialogue::DialogueEvent::RunBegin { npc, initiator, .. } = &ev {
            if let Some(f) = h.fight.as_mut() {
                f.engage(npc.0, *initiator);
            }
            // companions: who last spoke with it (a hire outside a line is that player's).
            h.companions.on_talk(npc.0, *initiator);
        }
        h.social.enactor.on_event(e, ev, &h.quests);
    }
    for (to, p) in std::mem::take(&mut h.social.enactor.pays) {
        let msg = Msg::Dialogue(coop_core::dialogue::DialogueMsg::Pay {
            item: coop_core::ids::FormId(p.item),
            count: p.count,
            silent: p.silent,
        });
        send(net, to, &msg, &mut h.send_failures);
    }
    for (to, m) in std::mem::take(&mut h.social.enactor.menus) {
        let msg = Msg::Dialogue(coop_core::dialogue::DialogueMsg::OpenMenu {
            npc: coop_core::ids::FormId(m.npc),
            menu: m.menu.byte(),
            value: m.value,
        });
        send(net, to, &msg, &mut h.send_failures);
    }

    // The rest vote.
    let (msgs, passed) = h.social.rester.step(e, &mut h.view.rules);
    for m in msgs {
        local(h, net, Msg::Rules(m));
    }
    if passed {
        // The new time goes to the guests now, not at the clock's next turn.
        h.clock_sent_ms = None;
    }
}

/// A guest's step: after the host's messages were taken.
pub(crate) fn guest_step(
    g: &mut GuestDriver,
    e: &mut Engine,
    net: &mut dyn Transport,
    now_ms: u64,
) {
    let active = g.social_on && g.sync.phase() == GuestPhase::Active;
    game_talk::set_sharing(e, active);
    game_rest::set_sharing(e, active);
    if !active {
        // Nobody else's conversation to watch (S42).
        coop_engine::watch::end_watch(e);
        return;
    }
    let me = g.sync.player;
    let share = |_: &mut Engine, _info: u32, held: bool| held;
    g.social.talker.story_base = g.sync.store.base().0;
    g.social.talker.now_ms = now_ms;
    g.social.talker.avatars = g
        .ladder
        .as_ref()
        .map(crate::ladder::Ladder::avatar_ids)
        .unwrap_or_default();
    let msgs = g
        .social
        .talker
        .step(e, me, &mut g.sync.live.dialogue, &share);
    for m in msgs {
        send(net, PlayerId::HOST, &Msg::Dialogue(m), &mut g.send_failures);
    }
    // The menus the host's run of this player's lines opens (a trade, repairs, ...).
    for (npc, menu, value) in g.sync.live.dialogue.take_menus() {
        let opened = coop_engine::menus::Menu::from_byte(menu).map(|menu| {
            coop_engine::menus::open(
                e,
                coop_engine::menus::MenuOpen {
                    menu,
                    npc: npc.0,
                    value,
                },
            )
        });
        if !matches!(opened, Some(Ok(()))) {
            g.social.menus_not_opened.push(format!(
                "the host's menu {menu} with {:08x} was not opened here: {opened:?}",
                npc.0
            ));
        }
    }
    // What the host's run of this player's lines takes from it (a payment, a hand-in): taken
    // from this game's own player (S30), once there is one.
    g.social
        .pays
        .extend(
            g.sync
                .live
                .dialogue
                .take_pays()
                .into_iter()
                .map(|(item, count, silent)| coop_engine::pay::Payment {
                    item: item.0,
                    count,
                    silent,
                }),
        );
    let pays = std::mem::take(&mut g.social.pays);
    for (i, p) in pays.iter().enumerate() {
        let Some(taken) = coop_engine::pay::take(e, *p) else {
            g.social.pays.extend_from_slice(&pays[i..]);
            break;
        };
        g.social.paid += 1;
        // items: what the story takes is not a put into a holder (see `story_took`).
        if let Some(items) = g.items.as_mut() {
            items.story_took(p.item, taken);
        }
        g.social.pay_notes.push(format!(
            "the host's run of this player's line took {taken} of {:08x} (asked {})",
            p.item, p.count
        ));
    }
    for npc in std::mem::take(&mut g.social.talker.greet_asks) {
        let m = Msg::Actor(coop_core::actors::ActorMsg::Greet {
            actor: coop_core::ids::FormId(npc),
        });
        send(net, PlayerId::HOST, &m, &mut g.send_failures);
    }
    let (msgs, _) = g.social.rester.step(e, &mut g.sync.live.rules);
    for m in msgs {
        send(net, PlayerId::HOST, &Msg::Rules(m), &mut g.send_failures);
    }

    // This player's part: a saved one is put back first; then the game's own goes to the host.
    g.social.party.restore(
        e,
        g.sync
            .store
            .iter()
            .map(|(k, entry)| (k.form.0, &entry.field)),
    );
    let claims = g.social.party.upload(e, now_ms);
    for batch in claims.chunks(MAX_INTENT_CHANGES) {
        for msg in g.sync.send_intent(Epoch(0), batch.to_vec()) {
            send(net, PlayerId::HOST, &msg, &mut g.send_failures);
        }
    }
}
