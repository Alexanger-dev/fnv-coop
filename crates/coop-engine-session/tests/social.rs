//! Conversations, party saves, rejoining and the rest vote between games in one process, joined
//! over coop-core's loopback transport (docs/coop/SPECS.md S16 to S19). Each game is the story
//! kit's: a small story in engine memory with stand-ins for the game's functions.

use std::path::PathBuf;

use coop_core::compat::plugin_list_hash;
use coop_core::ids::PlayerId;
use coop_core::proto::{Hello, PROTOCOL_VERSION};
use coop_core::sync::HostConfig;
use coop_core::transport::{LoopbackEndpoint, LoopbackNet};
use coop_engine::testkit::{Story, StorySpec};
use coop_engine::{forms, gate, live, talk};
use coop_engine_session::ladder::{Cap, Ladder, DEFAULT_AVATAR_BASE};
use coop_engine_session::party::{self, Part};
use coop_engine_session::{gate_guest, GuestDriver, HostDriver};

engine_core::assemble!();

/// A quest of the shared story, and one each player runs for themself.
const MQ: u32 = 0x0010_4c0c;
const OWN: u32 = 0x0010_4c1c;
/// The game clock's hour.
const HOUR: u32 = 0x38;
const SUNNY: u32 = 0x0010_4e85;
const CHET: u32 = 0x0010_4f25;
const SALOON: u32 = 0x0010_5a00;
const STORE: u32 = 0x0010_5a01;
/// The inventory's menu class (one of the Pip-Boy's menus).
const INVENTORY: u32 = 1002;
/// The dialogue menu's class.
const DIALOGUE: u32 = 1009;
/// Sunny's line of the shared story: said, it sets MQ's stage 10; ended, stage 20.
const SHARED_LINE: u32 = 0x0010_9001;
/// A line of the player's own quest: said, it sets OWN's stage 5.
const OWN_LINE: u32 = 0x0010_9002;
/// A second line of the shared story, with no result.
const PLAIN_LINE: u32 = 0x0010_9003;

fn hello() -> Hello {
    let plugins = vec!["FalloutNV.esm".to_string()];
    Hello {
        proto: PROTOCOL_VERSION,
        build_hash: 1,
        rules_hash: 2,
        mods_hash: plugin_list_hash(&plugins),
        exe: "1.4.0.525".into(),
        volatile_hash: 3,
        plugins,
    }
}

/// A game: everyone in the saloon with Sunny; Chet in the store.
fn story() -> Story {
    let mut s = Story::new(&StorySpec {
        quests: vec![(MQ, vec![5, 10, 20, 30]), (OWN, vec![5, 9])],
        globals: vec![(HOUR, 8.0)],
        vars: vec![(OWN, vec![1])],
        cells: vec![SALOON, STORE],
        actors: vec![SUNNY, CHET],
        ..StorySpec::default()
    });
    s.add_info(
        SHARED_LINE,
        MQ,
        "Head on up to the saloon. | I will meet you there.",
        "Where is Sunny?",
    );
    s.set_info_result(SHARED_LINE, false, MQ, 10);
    s.set_info_result(SHARED_LINE, true, MQ, 20);
    s.add_info(OWN_LINE, OWN, "You have been out cold.", "");
    s.set_info_result(OWN_LINE, false, OWN, 5);
    s.add_info(PLAIN_LINE, MQ, "Howdy.", "");
    s.place(SUNNY, SALOON);
    s.place(CHET, STORE);
    s.move_player(SALOON, [0.0; 3], 0.0);
    s.set_hour_global(HOUR);
    s
}

fn guest_story() -> Story {
    let mut g = story();
    gate_guest(&mut g.e);
    gate::set_own_quests(&mut g.e, [OWN]);
    g
}

struct Guest {
    game: Story,
    driver: GuestDriver,
    net: LoopbackEndpoint,
}

struct World {
    net: LoopbackNet,
    host: Story,
    hd: HostDriver,
    hostnet: LoopbackEndpoint,
    guests: Vec<Guest>,
    t: u64,
}

impl World {
    fn new(guests: &[&str]) -> World {
        let net = LoopbackNet::new();
        let hostnet = net.endpoint(PlayerId::HOST);
        let mut host = story();
        let mut hd = HostDriver::new(HostConfig::new(hello(), 1), [MQ], []);
        hd.set_clock([HOUR]);
        hd.start(&mut host.e);
        let mut w = World {
            net,
            host,
            hd,
            hostnet,
            guests: Vec::new(),
            t: 0,
        };
        for name in guests {
            w.join(guest_story(), name);
        }
        w
    }

    /// A game joins as the next player, its player known by `name`.
    fn join(&mut self, game: Story, name: &str) -> usize {
        let player = PlayerId(self.guests.len() as u8 + 1);
        let mut driver = GuestDriver::new(player, hello());
        driver.social.party.key = party::key_of(name);
        driver.social.party.own_quests = vec![OWN];
        self.guests.push(Guest {
            game,
            driver,
            net: self.net.endpoint(player),
        });
        self.guests.len() - 1
    }

    fn run_every(&mut self, frames: u32, ms: u64) {
        for _ in 0..frames {
            self.t += ms;
            self.hd.step(&mut self.host.e, &mut self.hostnet, self.t);
            for g in &mut self.guests {
                g.driver.step(&mut g.game.e, &mut g.net, self.t);
            }
        }
    }

    fn run(&mut self, frames: u32) {
        self.run_every(frames, 50);
    }

    /// Every guest holds the host's story: the shared quest's stage, the replica's hash.
    fn assert_in_step(&mut self) {
        let stage = forms::quest_stage(&mut self.host.e, MQ);
        for (i, g) in self.guests.iter_mut().enumerate() {
            assert_eq!(forms::quest_stage(&mut g.game.e, MQ), stage, "guest {i}");
            assert_eq!(g.driver.sync.desyncs_detected, 0, "guest {i}");
            assert!(g.driver.sync.hash_checks > 0, "guest {i}");
            assert_eq!(
                g.driver.sync.store.rev(),
                self.hd.sync.store.rev(),
                "guest {i}"
            );
        }
    }
}

/// A folder of this test's own under the build's temporary folder, empty.
fn party_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

// ---- Conversations ----

/// S42 (the owner's choice of 2026-11-21, "option 3"): a player far from another player's
/// conversation sees its lines as text, but not while busy (a menu open, or in a fight): then one
/// word that the conversation goes on, and once free again and the conversation over, one word
/// that it ended; nothing of it afterwards.
#[test]
fn a_far_player_who_is_busy_is_told_of_the_conversation_not_shown_its_lines() {
    let mut w = World::new(&["Sunny"]);
    w.host.set_name(SUNNY, "Sunny Smiles");
    w.run(20);
    w.host.move_player(STORE, [0.0; 3], 0.0);
    // The host has its inventory open.
    w.host.open_menu(INVENTORY);
    assert!(w.guests[0].game.talk_to(SUNNY));
    w.guests[0].game.say(SHARED_LINE, false);
    w.run(10);
    assert_eq!(w.host.messages(), ["Guest is talking to Sunny Smiles."]);
    // Its next line: held back too, and nothing more said of it.
    w.guests[0].game.say(SHARED_LINE, true);
    w.run(5);
    w.guests[0].game.say(PLAIN_LINE, false);
    w.run(10);
    let t = &w.hd.social.talker;
    assert_eq!(
        (t.lines_far_held, t.lines_far_shown, t.far_notices),
        (2, 0, 1)
    );
    assert_eq!(w.host.messages().len(), 1);
    // The conversation ends while the host is still busy: its end waits for the host.
    w.guests[0].game.say(PLAIN_LINE, true);
    w.guests[0].game.leave();
    w.run(10);
    assert_eq!(w.host.messages().len(), 1);
    w.host.close_menu();
    w.run(2);
    assert_eq!(
        w.host.messages(),
        [
            "Guest is talking to Sunny Smiles.",
            "Guest finished talking to Sunny Smiles."
        ]
    );
    w.run(20);
    assert_eq!(w.host.messages().len(), 2);
    assert_eq!(w.hd.social.talker.far_notices, 2);
}

/// Two conversations one after the other while the far player is busy: each is told, and each
/// end once the player is free; an NPC with no name is "someone".
#[test]
fn a_busy_far_player_is_told_of_each_conversation() {
    let mut w = World::new(&["Sunny"]);
    w.host.set_name(SUNNY, "Sunny Smiles");
    w.host.set_name(CHET, "");
    w.host.place(CHET, SALOON);
    w.guests[0].game.place(CHET, SALOON);
    w.run(20);
    w.host.move_player(STORE, [0.0; 3], 0.0);
    w.host.open_menu(INVENTORY);
    assert!(w.guests[0].game.talk_to(SUNNY));
    w.guests[0].game.say(SHARED_LINE, false);
    w.run(10);
    w.guests[0].game.say(SHARED_LINE, true);
    w.run(5);
    // Straight on to Chet, a frame later.
    w.guests[0].game.leave();
    w.run(1);
    assert!(w.guests[0].game.talk_to(CHET));
    w.guests[0].game.say_as(CHET, PLAIN_LINE, false);
    w.run(10);
    w.guests[0].game.say_as(CHET, PLAIN_LINE, true);
    w.guests[0].game.leave();
    w.run(10);
    w.host.close_menu();
    w.run(2);
    assert_eq!(
        w.host.messages(),
        [
            "Guest is talking to Sunny Smiles.",
            "Guest is talking to someone.",
            "Guest finished talking to Sunny Smiles.",
            "Guest finished talking to someone."
        ]
    );
}

/// A third player starts a conversation while the busy far player is told of another's: the
/// first counts as over for the far player (it is shown one conversation at a time), and both
/// ends are told once it is free.
#[test]
fn a_busy_far_player_told_of_a_second_players_conversation_is_told_both_ends() {
    let mut w = World::new(&["Sunny", "Chet"]);
    w.host.set_name(SUNNY, "Sunny Smiles");
    w.host.set_name(CHET, "Chet");
    w.host.place(CHET, SALOON);
    for g in &mut w.guests {
        g.game.place(CHET, SALOON);
    }
    w.run(20);
    w.host.move_player(STORE, [0.0; 3], 0.0);
    w.host.open_menu(INVENTORY);
    assert!(w.guests[0].game.talk_to(SUNNY));
    w.guests[0].game.say(SHARED_LINE, false);
    w.run(10);
    // Guest 2, taken into it (D13), leaves it to talk to Chet.
    w.guests[1].game.click_leave();
    w.run(2);
    assert!(w.guests[1].game.talk_to(CHET));
    w.guests[1].game.say_as(CHET, PLAIN_LINE, false);
    w.run(10);
    w.host.close_menu();
    w.run(2);
    assert_eq!(
        w.host.messages(),
        [
            "Guest is talking to Sunny Smiles.",
            "Guest 2 is talking to Chet.",
            "Guest finished talking to Sunny Smiles."
        ]
    );
}

/// The owner, 2026-11-22: a line a script has an NPC say to the host's player (`SayTo`: Sunny's
/// "see those sarsaparilla bottles", which gives the rifle) is heard by every player near the NPC,
/// in their own game; its result runs once, where the host's game said it. Said away from the
/// host's player, the host's game does not say it and the near guest's game runs its result.
#[test]
fn a_line_a_script_has_an_npc_say_is_heard_by_every_player_near_it_and_its_result_runs_once() {
    let mut w = World::new(&["Sunny", "Chet"]);
    w.run(20);
    // Guest 2 is in the store, away from Sunny.
    w.guests[1].game.move_player(STORE, [0.0; 3], 0.0);
    w.run(10);
    // The host's script has Sunny say the line to its player; the host's game says it.
    w.host.script_says(SUNNY, SHARED_LINE);
    w.host.say_as(SUNNY, SHARED_LINE, false);
    w.host.say_as(SUNNY, SHARED_LINE, true);
    w.run(10);
    // The guest near her: she says it there, to that player.
    assert_eq!(w.guests[0].game.said_to_player(SUNNY), (1, SHARED_LINE));
    assert_eq!(w.guests[0].driver.says_said, 1);
    // Guest 2, away: not.
    assert_eq!(w.guests[1].game.said_to_player(SUNNY).0, 0);
    // The guest's game says it: its result is the host's, not run again (both its parts).
    w.guests[0].game.say_as(SUNNY, SHARED_LINE, false);
    w.guests[0].game.say_as(SUNNY, SHARED_LINE, true);
    assert_eq!(
        coop_engine::commands::echo_results_skipped(&mut w.guests[0].game.e),
        2
    );
    // A line no one says: nothing to skip.
    w.guests[0].game.say_unspoken(PLAIN_LINE);
    // Her next line there is that game's own again.
    w.guests[0].game.say_as(SUNNY, PLAIN_LINE, false);
    assert_eq!(
        coop_engine::commands::echo_results_skipped(&mut w.guests[0].game.e),
        2
    );
    // Another, said there but never played (no end): her lines in the dialogue window are the
    // window's still.
    w.host.script_says(SUNNY, SHARED_LINE);
    w.run(10);
    assert_eq!(w.guests[0].game.said_to_player(SUNNY).0, 2);
    assert!(w.guests[0].game.talk_to(SUNNY));
    w.guests[0].game.say(PLAIN_LINE, false);
    assert_eq!(
        coop_engine::commands::echo_results_skipped(&mut w.guests[0].game.e),
        2,
        "a line of the window"
    );
    talk::close_menu(&mut w.guests[0].game.e);
    w.run(10);
    // Said away from the host's player: the host's game does not say it; the guest near her
    // says it and runs its result.
    w.host.move_player(STORE, [0.0; 3], 0.0);
    w.run(10);
    w.host.script_says(SUNNY, SHARED_LINE);
    w.run(10);
    assert_eq!(w.guests[0].game.said_to_player(SUNNY).0, 3);
    w.guests[0].game.say_as(SUNNY, SHARED_LINE, false);
    w.guests[0].game.say_as(SUNNY, SHARED_LINE, true);
    assert_eq!(
        coop_engine::commands::echo_results_skipped(&mut w.guests[0].game.e),
        2,
        "run here"
    );
}

/// S42 step 3b (D13, the owner): a click on a line that matters is a vote; the votes show after
/// its row ("(1/3 Votes, 20s)"); a player away from the NPC is told and waited for; when the 20 s
/// are up the starter's pick goes through, its owner asked to say it, and the players in it are
/// told what was chosen.
#[test]
fn a_vote_is_shown_waited_for_twenty_seconds_and_chosen() {
    let mut w = World::new(&["Sunny", "Chet"]);
    for e in std::iter::once(&mut w.host.e).chain(w.guests.iter_mut().map(|g| &mut g.game.e)) {
        coop_engine::choices::set_vote_lines(
            e,
            std::collections::HashSet::from([SHARED_LINE, PLAIN_LINE]),
        );
    }
    w.guests[1].game.set_name(SUNNY, "Sunny Smiles");
    w.run(20);
    // Guest 2 is in the store, away from Sunny.
    w.guests[1].game.move_player(STORE, [0.0; 3], 0.0);
    w.run(10);
    assert!(w.guests[0].game.talk_to(SUNNY));
    w.guests[0].game.say(PLAIN_LINE, false);
    w.guests[0].game.say(PLAIN_LINE, true);
    w.run(10);
    assert_eq!(w.hd.social.talker.pulled_in, 1);
    // The guest's window lists two lines that matter; the host votes for one.
    w.guests[0].game.list_options(&[SHARED_LINE, PLAIN_LINE]);
    w.run(4);
    w.host.click_vote(SHARED_LINE);
    w.run(4);
    assert_eq!(w.hd.social.talker.votes_cast, 1);
    assert_eq!(
        forms::quest_stage(&mut w.host.e, MQ),
        Some(0),
        "nothing said yet"
    );
    // The guest, in it, sees the votes after the row; guest 2, away, is told and may come.
    // (The seconds left rounded up: under 20 s gone by, still 20.)
    assert_eq!(
        coop_engine::choices::marks(&mut w.guests[0].game.e),
        [(SHARED_LINE, "(1/3 Votes, 20s)".to_string())]
    );
    let told = "Host voted for \"Where is Sunny?\". Talk to Sunny Smiles to vote (20s).";
    // The guest (the starter) votes for it too: 2 of 3, one row.
    w.guests[0].game.click_vote(SHARED_LINE);
    w.run(4);
    let marks = coop_engine::choices::marks(&mut w.host.e);
    assert_eq!(marks.len(), 1, "{marks:?}");
    assert!(marks[0].1.starts_with("(2/3 Votes, "), "{marks:?}");
    // The host changes its mind: 1 of 3 each; guest 2 still waited for.
    w.host.click_vote(PLAIN_LINE);
    w.run(4);
    let marks = coop_engine::choices::marks(&mut w.host.e);
    assert_eq!(marks.len(), 2, "{marks:?}");
    assert!(
        marks.iter().all(|(_, m)| m.starts_with("(1/3 Votes, ")),
        "{marks:?}"
    );
    assert_eq!(w.hd.social.talker.votes_chosen, 0);
    // The 20 s are up: the starter's pick (not the first vote's), the guest (its owner) asked to
    // say it; the players in it told.
    w.run_every(25, 1_000);
    assert_eq!(w.hd.social.talker.votes_chosen, 1);
    assert_eq!(w.guests[0].driver.social.talker.votes_chosen, 1);
    assert!(w
        .host
        .messages()
        .contains(&"Chosen: Where is Sunny?".to_string()));
    assert!(coop_engine::choices::marks(&mut w.host.e).is_empty());
    assert_eq!(
        w.guests[0].driver.social.talker.asks_said + w.guests[0].driver.social.talker.asks_failed,
        1,
        "the owner was asked to say it"
    );
    // Guest 2, away, was told of the vote once, and nothing of what was chosen.
    let far = w.guests[1].game.messages();
    assert_eq!(far.iter().filter(|m| *m == told).count(), 1, "{far:?}");
    assert!(!far.iter().any(|m| m.starts_with("Chosen")), "{far:?}");
}

/// The owner, 2026-11-22: an option whose text is its topic's name ("I wanted to ask you some
/// questions.", no prompt of its own) had no subtitle; it has, as the option list shows it.
#[test]
fn an_option_named_by_its_topic_is_subtitled_with_that_name() {
    let mut w = World::new(&["Sunny"]);
    w.host
        .set_topic_name(PLAIN_LINE, "I wanted to ask you some questions.");
    w.run(20);
    w.host.move_player(STORE, [0.0; 3], 0.0);
    assert!(w.guests[0].game.talk_to(SUNNY));
    w.guests[0].game.say(PLAIN_LINE, false);
    w.run(10);
    assert_eq!(
        w.host.messages(),
        ["Guest: I wanted to ask you some questions.", "Howdy."]
    );
}

/// The owner, 2026-11-21: a far player's lines came as Pip-Boy notifications, cut short. They
/// are the game's own subtitles now, one at a time (each its full time), the NPC's named.
#[test]
fn a_far_player_gets_the_lines_as_subtitles_one_at_a_time() {
    let mut w = World::new(&["Sunny"]);
    w.host.set_name(SUNNY, "Sunny Smiles");
    w.host.subtitles_on();
    w.run(20);
    w.host.move_player(STORE, [0.0; 3], 0.0);
    assert!(w.guests[0].game.talk_to(SUNNY));
    w.guests[0].game.say(SHARED_LINE, false);
    w.run(10);
    // The first shown; the next waits for it to end.
    assert_eq!(w.host.subtitles(), ["Guest: Where is Sunny?"]);
    w.host.subtitle_ends();
    w.run(2);
    assert_eq!(w.host.subtitles().len(), 2);
    assert_eq!(
        w.host.subtitles()[1],
        "Sunny Smiles: Head on up to the saloon."
    );
    w.host.subtitle_ends();
    w.run(2);
    assert_eq!(
        w.host.subtitles()[2],
        "Sunny Smiles: I will meet you there."
    );
    w.host.subtitle_ends();
    w.run(20);
    assert_eq!(w.host.subtitles().len(), 3);
    assert!(w.host.messages().is_empty(), "{:?}", w.host.messages());
    assert_eq!(w.hd.social.talker.subtitles_shown, 3);
}

/// Waiting subtitles are not shown while the player is busy, and one waits for the HUD's own
/// (an NPC's nearby) only so long.
#[test]
fn waiting_subtitles_hold_while_busy_and_do_not_wait_forever() {
    let mut w = World::new(&["Sunny"]);
    w.host.subtitles_on();
    w.run(20);
    w.host.move_player(STORE, [0.0; 3], 0.0);
    assert!(w.guests[0].game.talk_to(SUNNY));
    w.guests[0].game.say(SHARED_LINE, false);
    w.run(10);
    assert_eq!(w.host.subtitles().len(), 1);
    // The inventory opened: the first ends, nothing more while it is open.
    w.host.open_menu(INVENTORY);
    w.host.subtitle_ends();
    w.run(30);
    assert_eq!(w.host.subtitles().len(), 1);
    w.host.close_menu();
    w.run(2);
    assert_eq!(w.host.subtitles().len(), 2);
    // The HUD's subtitle never ends (an NPC chattering nearby): the next is shown after a while.
    w.run(coop_engine_session::talk::SUBTITLE_WAIT_STEPS - 10);
    assert_eq!(w.host.subtitles().len(), 2);
    w.run(20);
    assert_eq!(w.host.subtitles().len(), 3);
}

/// A subtitle longer than the game's holds is cut to what it holds (the game copies it into
/// 0x104 bytes).
#[test]
fn a_subtitle_is_never_longer_than_the_game_holds() {
    let mut w = World::new(&["Sunny"]);
    w.host.subtitles_on();
    assert!(coop_engine::talk::subtitle(&mut w.host.e, &"x".repeat(400)));
    assert_eq!(w.host.subtitles()[0].len(), coop_engine::addr::SUBTITLE_MAX);
}

/// With the game's subtitles turned off, the lines come as HUD messages, one a step.
#[test]
fn with_subtitles_off_the_lines_come_as_messages() {
    let mut w = World::new(&["Sunny"]);
    w.host.subtitles_on();
    w.host.set_subtitles_off(true);
    w.run(20);
    w.host.move_player(STORE, [0.0; 3], 0.0);
    assert!(w.guests[0].game.talk_to(SUNNY));
    w.guests[0].game.say(SHARED_LINE, false);
    w.run(10);
    assert!(w.host.subtitles().is_empty());
    assert_eq!(
        w.host.messages(),
        [
            "Guest: Where is Sunny?",
            "Head on up to the saloon.",
            "I will meet you there."
        ]
    );
    assert_eq!(w.hd.social.talker.subtitles_as_messages, 3);
}

/// A far player in a fight is busy too: told of the conversation, not shown its lines.
#[test]
fn a_far_player_in_a_fight_is_told_of_the_conversation_not_shown_its_lines() {
    let mut w = World::new(&["Sunny"]);
    w.run(20);
    w.host.move_player(STORE, [0.0; 3], 0.0);
    w.host.set_combat(coop_engine::actors::PLAYER_REF_ID, CHET);
    assert!(w.guests[0].game.talk_to(SUNNY));
    w.guests[0].game.say(SHARED_LINE, false);
    w.run(10);
    assert_eq!(w.host.messages(), ["Guest is talking to someone."]);
    assert_eq!(w.hd.social.talker.lines_far_held, 1);
    // The fight over, the next line comes as text again.
    w.host.set_combat(coop_engine::actors::PLAYER_REF_ID, 0);
    w.guests[0].game.say(SHARED_LINE, true);
    w.run(5);
    w.guests[0].game.say(PLAIN_LINE, false);
    w.run(10);
    assert_eq!(
        w.host.messages(),
        ["Guest is talking to someone.", "Howdy."]
    );
}

/// A far player free for the whole conversation sees every line and is not told it ended.
#[test]
fn a_far_player_who_is_free_sees_the_lines_and_no_notice() {
    let mut w = World::new(&["Sunny"]);
    w.run(20);
    w.host.move_player(STORE, [0.0; 3], 0.0);
    assert!(w.guests[0].game.talk_to(SUNNY));
    w.guests[0].game.say(SHARED_LINE, false);
    w.run(10);
    w.guests[0].game.say(SHARED_LINE, true);
    w.guests[0].game.leave();
    w.run(20);
    assert_eq!(w.host.messages().len(), 3, "{:?}", w.host.messages());
    assert!(!w.host.messages().iter().any(|m| m.contains("talking to")));
    assert_eq!(w.hd.social.talker.far_notices, 0);
}

#[test]
fn a_guests_line_runs_once_on_the_host_and_the_others_see_it() {
    let mut w = World::new(&["Sunny", "Chet"]);
    w.run(20);
    // Guest 1 talks to Sunny; guest 2 stands by; the host is in the store.
    w.host.move_player(STORE, [0.0; 3], 0.0);
    assert!(w.guests[0].game.talk_to(SUNNY));
    w.guests[0].game.say(SHARED_LINE, false);
    w.run(10);
    // The result ran in the host's game, not in the talker's, and came back as the story.
    assert_eq!(forms::quest_stage(&mut w.host.e, MQ), Some(10));
    assert_eq!(w.hd.social.enactor.enacted, 1);
    assert_eq!(talk::counts(&mut w.guests[0].game.e), (1, 0, 0));
    assert_eq!(w.guests[0].game.said(SHARED_LINE), 1);
    assert_eq!(w.guests[0].driver.social.talker.opened, 1);
    w.assert_in_step();
    // The guest standing by, taken into the conversation (D13; with no window in this test
    // game, as text), saw what was chosen and each response; so did the host, elsewhere and free
    // (S42, the owner's choice of 2026-11-21: far off, the lines come as text), not taken in.
    let lines = [
        "Guest: Where is Sunny?",
        "Head on up to the saloon.",
        "I will meet you there.",
    ];
    assert_eq!(w.guests[1].game.messages(), lines);
    assert_eq!(w.guests[1].driver.social.talker.pulled_in, 1);
    assert_eq!(w.guests[1].driver.social.talker.lines_shown, 1);
    assert_eq!(w.host.messages(), lines);
    assert_eq!(w.hd.social.talker.pulled_in, 0);
    assert_eq!(w.hd.social.talker.lines_far, 1);
    assert_eq!(w.hd.social.talker.lines_far_shown, 1);
    // The line counts as said in every game, near or far (the owner, 2026-11-16: a line the
    // game says once was said again to the player whose game had not said it).
    assert!(talk::said(&mut w.guests[1].game.e, SHARED_LINE));
    assert!(talk::said(&mut w.host.e, SHARED_LINE));
    assert!(!talk::said(&mut w.guests[1].game.e, SHARED_LINE + 1));
    assert!(w.guests[0].game.messages().is_empty());
    // The line ends, then the conversation: the end result runs once, the NPC is free again.
    w.guests[0].game.say(SHARED_LINE, true);
    w.run(10);
    assert_eq!(forms::quest_stage(&mut w.host.e, MQ), Some(20));
    assert_eq!(w.hd.social.enactor.enacted, 2);
    assert_eq!(w.hd.sync.live.dialogue.open_sessions(), 1);
    w.guests[0].game.leave();
    w.run(10);
    assert_eq!(w.hd.sync.live.dialogue.open_sessions(), 0);
    assert_eq!(w.hd.social.enactor.left, 0);
    w.assert_in_step();
    // Nothing ran twice: the stage's own result script ran on the host only.
    assert_eq!(w.guests[0].game.scripts_run(), 0);
    assert_eq!(w.guests[1].game.scripts_run(), 0);
}

/// PC run 30: Sunny hands over her rifle at the bottles by speaking up, outside the guest's
/// conversation: the line ran in the guest's game, whose player alone got the rifle, and the host
/// never met that line again. A story line's reward a guest's game gave its player goes to the
/// host's player and every other guest once; the guest is not given it twice.
#[test]
fn a_reward_a_guests_npc_gave_outside_the_conversation_goes_to_every_player() {
    const RIFLE: u32 = 0x0007_ea24;
    let mut w = World::new(&["Sunny", "Chet"]);
    w.host.add_item_form(RIFLE);
    for g in &mut w.guests {
        g.game.add_item_form(RIFLE);
        g.game.set_info_reward_xp(SHARED_LINE, true, 10);
    }
    w.run(20);
    // A line's result giving the rifle and 10 experience, said by Sunny in guest 1's game.
    w.guests[0].game.set_info_item(SHARED_LINE, true, RIFLE, 1);
    w.guests[0].game.say_as(SUNNY, SHARED_LINE, true);
    w.run(10);
    assert_eq!(w.host.items(RIFLE), 1);
    assert_eq!(w.host.xp(), 10);
    for (i, g) in w.guests.iter_mut().enumerate() {
        assert_eq!(g.game.items(RIFLE), 1, "guest {i}");
        assert_eq!(g.game.xp(), 10, "guest {i}");
    }
    w.run(40);
    assert_eq!(w.host.items(RIFLE), 1);
    assert_eq!(w.guests[0].game.items(RIFLE), 1);
    assert_eq!(w.guests[1].game.items(RIFLE), 1);
}

#[test]
fn a_line_whose_end_never_comes_is_finished_by_the_next_line_and_by_the_goodbye() {
    let mut w = World::new(&["Sunny"]);
    w.run(20);
    assert!(w.guests[0].game.talk_to(SUNNY));
    w.guests[0].game.say(PLAIN_LINE, false);
    w.guests[0].game.say(SHARED_LINE, false);
    w.guests[0].game.leave();
    w.run(30);
    // Both lines were picked in order; the second's end result ran when the conversation ended.
    assert_eq!(w.guests[0].driver.social.talker.picks, 2);
    assert_eq!(forms::quest_stage(&mut w.host.e, MQ), Some(20));
    assert_eq!(w.hd.sync.live.dialogue.open_sessions(), 0);
    assert_eq!(
        w.host.messages(),
        [
            "Howdy.",
            "Guest: Where is Sunny?",
            "Head on up to the saloon.",
            "I will meet you there."
        ]
    );
    w.assert_in_step();
}

#[test]
fn the_hosts_line_is_seen_by_a_guest_and_runs_in_the_hosts_game_only() {
    let mut w = World::new(&["Sunny"]);
    w.run(20);
    assert!(w.host.talk_to(SUNNY));
    w.host.say(SHARED_LINE, false);
    w.host.say(SHARED_LINE, true);
    w.host.leave();
    w.run(20);
    assert_eq!(forms::quest_stage(&mut w.host.e, MQ), Some(20));
    assert_eq!(
        w.hd.social.enactor.enacted, 0,
        "the host's game ran it itself"
    );
    assert_eq!(w.hd.social.talker.opened, 1);
    assert_eq!(
        w.guests[0].game.messages(),
        [
            "Host: Where is Sunny?",
            "Head on up to the saloon.",
            "I will meet you there."
        ]
    );
    assert_eq!(w.hd.sync.live.dialogue.open_sessions(), 0);
    w.assert_in_step();
    assert_eq!(w.guests[0].game.scripts_run(), 0);
}

#[test]
fn a_line_of_the_players_own_quest_stays_in_that_players_game() {
    let mut w = World::new(&["Sunny"]);
    w.run(20);
    assert!(w.guests[0].game.talk_to(SUNNY));
    w.guests[0].game.say(OWN_LINE, false);
    w.guests[0].game.say(OWN_LINE, true);
    w.guests[0].game.leave();
    w.run(20);
    assert_eq!(forms::quest_stage(&mut w.guests[0].game.e, OWN), Some(5));
    assert_eq!(forms::quest_stage(&mut w.host.e, OWN), Some(0));
    assert_eq!(w.guests[0].driver.social.talker.opened, 0);
    assert_eq!(w.hd.social.enactor.enacted + w.hd.social.enactor.left, 0);
    assert!(w.host.messages().is_empty());
}

/// The owner, 2026-11-14: both players sat for Doc Mitchell's questions; he asked the host's and
/// stared at the guest until the host was done. While a player's own opening runs (OWN's stage 9
/// not done in its game), its conversations are its own: the host's conversation with the NPC does
/// not keep it from the guest, and the guest's lines go to nobody; once the opening is done, an NPC
/// is in one conversation at a time again.
#[test]
fn in_their_own_openings_both_players_talk_to_the_same_npc_at_once() {
    let mut w = World::new(&["Sunny"]);
    w.hd.social.talker.opening = Some((OWN, 9));
    w.guests[0].driver.social.talker.opening = Some((OWN, 9));
    w.run(20);
    assert!(w.host.talk_to(SUNNY));
    w.host.say(PLAIN_LINE, false);
    w.run(10);
    assert!(w.guests[0].game.talk_to(SUNNY));
    w.guests[0].game.say(PLAIN_LINE, false);
    w.run(10);
    assert_eq!(w.guests[0].driver.social.talker.blocked, 0);
    assert_eq!(w.hd.sync.live.dialogue.open_sessions(), 0);
    assert!(w.guests[0].driver.social.talker.opening_lines > 0);
    assert!(w.hd.social.talker.opening_lines > 0);
    w.guests[0].game.leave();
    w.host.leave();
    // Both openings done: one conversation at a time.
    forms::set_quest_stage(&mut w.host.e, OWN, 9);
    forms::set_quest_stage(&mut w.guests[0].game.e, OWN, 9);
    w.run(10);
    assert!(w.host.talk_to(SUNNY));
    w.host.say(PLAIN_LINE, false);
    w.run(10);
    // Near and free, the guest is taken into the host's conversation (D13), with no word.
    assert_eq!(w.guests[0].driver.social.talker.pulled_in, 1);
    assert!(w.guests[0].driver.sync.live.dialogue.joined());
    assert_eq!(w.guests[0].driver.social.talker.blocked, 0);
}

/// D13 (the owner, 2026-11-22: every player near a conversation hears it together): the players
/// near a conversation are taken into it; one may leave it with `[Leave conversation]` and is not
/// taken in again, only told it may join; talking to the NPC joins it again; an NPC's own greeting
/// joins nothing; an NPC is in one conversation at a time.
#[test]
fn the_players_near_a_conversation_are_taken_in_and_may_leave_and_join_again() {
    let mut w = World::new(&["Sunny", "Chet"]);
    w.run(20);
    assert!(w.guests[0].game.talk_to(SUNNY));
    w.guests[0].game.say(PLAIN_LINE, false);
    w.run(10);
    let session = w.hd.sync.live.dialogue.session_of(PlayerId(1)).unwrap();
    // Guest 2 and the host, in the saloon too, are taken in, told nothing.
    let mut players = w.hd.sync.live.dialogue.players_in(session).to_vec();
    players.sort();
    assert_eq!(players, [PlayerId::HOST, PlayerId(1), PlayerId(2)]);
    assert_eq!(w.guests[1].driver.social.talker.pulled_in, 1);
    assert_eq!(w.hd.social.talker.pulled_in, 1);
    assert_eq!(w.guests[1].driver.social.talker.join_hints, 0);
    assert!(w.guests[1].driver.sync.live.dialogue.joined());
    // Each has the leave row.
    for g in &mut w.guests {
        assert!(coop_engine::choices::leave_offered(&mut g.game.e));
    }
    assert!(coop_engine::choices::leave_offered(&mut w.host.e));
    // Guest 2 leaves: out, not taken in again, told at its next line that it may join.
    w.guests[1].game.messages();
    w.guests[1].game.click_leave();
    w.run(4);
    assert_eq!(w.guests[1].driver.social.talker.leaves, 1);
    assert!(!w
        .hd
        .sync
        .live
        .dialogue
        .players_in(session)
        .contains(&PlayerId(2)));
    assert!(!coop_engine::choices::leave_offered(
        &mut w.guests[1].game.e
    ));
    w.guests[0].game.say(PLAIN_LINE, true);
    w.guests[0].game.say(PLAIN_LINE, false);
    w.run(10);
    assert_eq!(w.guests[1].driver.social.talker.pulled_in, 1);
    assert!(w.guests[1]
        .game
        .messages()
        .contains(&"Guest is talking to someone. Talk to them to join.".to_string()));
    // Chet activating Sunny in its game is not guest 2 talking to her.
    let (sunny, chet) = (
        forms::form(&mut w.guests[1].game.e, SUNNY).unwrap(),
        forms::form(&mut w.guests[1].game.e, CHET).unwrap(),
    );
    w.guests[1]
        .game
        .e
        .site(coop_engine::addr::REFR_ACTIVATE_SITE, &[sunny, chet]);
    w.run(2);
    assert_eq!(w.guests[1].driver.social.talker.joins, 0);
    // Sunny coming to greet guest 2 (her menu asked of the game with no activation): refused,
    // and told why; joins nothing.
    assert!(!w.guests[1].game.talk_to(SUNNY));
    w.run(2);
    assert_eq!(w.guests[1].driver.social.talker.joins, 0);
    assert_eq!(w.guests[1].driver.social.talker.blocked, 1);
    assert_eq!(
        w.guests[1].game.messages().last().map(String::as_str),
        Some("Another player is talking to them.")
    );
    // Guest 2 talks to Sunny: it joins again, with no word that she is taken.
    assert!(!w.guests[1].game.activate(SUNNY));
    w.run(4);
    assert_eq!(w.guests[1].driver.social.talker.joins, 1);
    assert_eq!(w.guests[1].driver.social.talker.blocked, 1);
    assert!(w
        .hd
        .sync
        .live
        .dialogue
        .players_in(session)
        .contains(&PlayerId(2)));
    // Once the conversation is over, Sunny is free again: the host talks to her itself.
    w.guests[0].game.say(PLAIN_LINE, true);
    w.guests[0].game.leave();
    w.run(10);
    assert_eq!(w.hd.sync.live.dialogue.open_sessions(), 0);
    assert!(w.host.talk_to(SUNNY));
}

/// D13 (the owner, 2026-11-22): a player who talks to an NPC near another player in a menu waits
/// for it ("... is currently in a menu. Please wait.", the other told who waits); the
/// conversation starts once that player is free, with no timer; walking away gives it up; an
/// NPC's own greeting does not wait.
#[test]
fn a_conversation_near_a_busy_player_waits_for_it_and_starts_once_it_is_free() {
    let mut w = World::new(&["Sunny", "Chet"]);
    // The players' avatars: where each other player stands in this game.
    w.hd.ladder = Some(Ladder::new(false, Cap::Avatar, DEFAULT_AVATAR_BASE).quiet());
    for g in &mut w.guests {
        g.driver.ladder = Some(Ladder::new(true, Cap::Avatar, DEFAULT_AVATAR_BASE).quiet());
        g.game.set_name(SUNNY, "Sunny Smiles");
    }
    w.host.set_name(SUNNY, "Sunny Smiles");
    w.run(20);
    assert_eq!(w.hd.social.talker.avatars.len(), 2, "the guests' avatars");
    // Guest 2 goes to the store: not near Sunny, not waited for.
    w.guests[1].game.move_player(STORE, [0.0; 3], 0.0);
    w.run(10);
    w.guests[0].game.open_menu(INVENTORY);
    w.run(4);
    // The host talks to Sunny: held, told why; the guest told who waits.
    assert!(!w.host.activate(SUNNY));
    w.run(4);
    assert_eq!(w.hd.social.talker.waits, 1);
    assert_eq!(talk::menu_npc(&mut w.host.e), None);
    assert_eq!(
        w.host.messages().last().map(String::as_str),
        Some("Guest is currently in a menu. Please wait.")
    );
    assert_eq!(
        w.guests[0].game.messages().last().map(String::as_str),
        Some("Host wants to talk to Sunny Smiles.")
    );
    // Guest 2, free and elsewhere, is not told.
    assert!(w.guests[1]
        .game
        .messages()
        .iter()
        .all(|m| !m.contains("wants to talk")));
    // Still in the menu a while later: still waiting.
    w.run(40);
    assert_eq!(w.hd.social.talker.waits_started, 0);
    assert_eq!(talk::menu_npc(&mut w.host.e), None);
    // The guest closes its menu while the host is in a menu itself: it starts once the host is
    // out of it too.
    w.host.open_menu(INVENTORY);
    w.guests[0].game.close_menu();
    w.run(6);
    assert_eq!(w.hd.social.talker.waits_started, 0);
    w.host.close_menu();
    w.run(6);
    assert_eq!(w.hd.social.talker.waits_started, 1);
    assert!(talk::menu_npc(&mut w.host.e).is_some());
    w.host.say(PLAIN_LINE, false);
    w.run(10);
    assert_eq!(w.guests[0].driver.social.talker.pulled_in, 1);
    w.host.say(PLAIN_LINE, true);
    w.host.leave();
    w.run(10);
    // The guest in a fight: "in battle"; the conversation starts once the fight is over.
    w.guests[0]
        .game
        .set_combat(coop_engine::actors::PLAYER_REF_ID, CHET);
    w.run(4);
    assert!(!w.host.activate(SUNNY));
    w.run(2);
    assert_eq!(
        w.host.messages().last().map(String::as_str),
        Some("Guest is currently in battle. Please wait.")
    );
    w.guests[0]
        .game
        .set_combat(coop_engine::actors::PLAYER_REF_ID, 0);
    w.run(6);
    assert_eq!(w.hd.social.talker.waits_started, 2);
    talk::close_menu(&mut w.host.e);
    w.host.leave();
    w.run(4);
    let waits = w.hd.social.talker.waits;
    // Something there that is not an actor (a door, say) activated: no wait.
    w.guests[0].game.open_menu(INVENTORY);
    w.host.place(CHET, SALOON);
    w.run(4);
    let chet = forms::form(&mut w.host.e, CHET).unwrap();
    let kind = w.host.e.mem.u8(chet + coop_engine::addr::FORM_TYPE_OFFSET);
    w.host.e.mem.set_u8(
        chet + coop_engine::addr::FORM_TYPE_OFFSET,
        coop_engine::addr::FORM_TYPE_REFR,
    );
    talk::activate(&mut w.host.e, CHET);
    w.run(2);
    assert_eq!(w.hd.social.talker.waits, waits);
    w.host
        .e
        .mem
        .set_u8(chet + coop_engine::addr::FORM_TYPE_OFFSET, kind);
    talk::close_menu(&mut w.host.e);
    w.host.leave();
    // Sunny greeting the host herself while the guest is busy: no wait.
    assert!(w.host.talk_to(SUNNY));
    assert_eq!(w.hd.social.talker.waits, waits);
    talk::close_menu(&mut w.host.e);
    w.host.leave();
    w.guests[0].game.close_menu();
    w.run(4);
    // The guest in a conversation of its own (with Chet): the host is not held for it.
    w.guests[0].game.place(CHET, SALOON);
    assert!(w.guests[0].game.talk_to(CHET));
    // (Its dialogue window is a menu to the game, as in the game.)
    w.guests[0].game.open_menu(DIALOGUE);
    w.guests[0].game.say_as(CHET, PLAIN_LINE, false);
    w.run(6);
    assert!(w.host.activate(SUNNY));
    w.run(2);
    assert_eq!(w.hd.social.talker.waits, waits);
    talk::close_menu(&mut w.host.e);
    w.host.leave();
    w.guests[0].game.close_menu();
    w.guests[0].game.leave();
    w.run(10);
    // Held again, then the host walks away: given up, nothing starts when the guest is free.
    w.guests[0].game.open_menu(INVENTORY);
    w.run(4);
    assert!(!w.host.activate(SUNNY));
    w.run(2);
    assert_eq!(w.hd.social.talker.waits, waits + 1);
    w.host.move_player(STORE, [0.0; 3], 0.0);
    w.run(2);
    w.guests[0].game.close_menu();
    w.run(6);
    assert_eq!(w.hd.social.talker.waits_started, 2);
    assert_eq!(talk::menu_npc(&mut w.host.e), None);
}

/// S42, D12: the starter leaves its own conversation and the player who joined goes on with it
/// (not taken in again):
/// the starter's window closes without ending it, its later lines run as any guest's, the
/// starter follows them as text, and the conversation ends once no one is left in it.
#[test]
fn the_starter_leaves_and_the_player_who_joined_goes_on() {
    let mut w = World::new(&["Sunny"]);
    w.run(20);
    assert!(w.host.talk_to(SUNNY));
    w.host.say(PLAIN_LINE, false);
    w.host.say(PLAIN_LINE, true);
    w.run(10);
    let session = w.hd.sync.live.dialogue.session_of(PlayerId::HOST).unwrap();
    // The guest, near, is taken in (D13).
    w.run(4);
    assert_eq!(w.guests[0].driver.social.talker.pulled_in, 1);
    assert_eq!(
        w.hd.sync.live.dialogue.players_in(session),
        [PlayerId::HOST, PlayerId(1)]
    );
    // The host leaves (a line it said at that moment is not its to send any more): its window
    // closes, the conversation goes on with the guest.
    w.host.say(PLAIN_LINE, false);
    w.host.click_leave();
    w.run(4);
    assert_eq!(
        w.hd.social.talker.denied, 0,
        "no conversation asked for again"
    );
    assert_eq!(w.hd.social.talker.leaves, 1);
    assert_eq!(
        talk::menu_npc(&mut w.host.e),
        None,
        "the host's window closed"
    );
    assert_eq!(w.hd.sync.live.dialogue.players_in(session), [PlayerId(1)]);
    assert_eq!(w.hd.sync.live.dialogue.open_sessions(), 1);
    // Alone in it now, the guest has no leave row.
    assert!(!coop_engine::choices::leave_offered(
        &mut w.guests[0].game.e
    ));
    // The guest's line (its window on Sunny: she is not busy to a player in her conversation):
    // picked, run in the host's game once, followed there as text.
    w.host.messages();
    assert!(w.guests[0].game.talk_to(SUNNY));
    w.guests[0].game.say(SHARED_LINE, false);
    w.run(10);
    assert_eq!(forms::quest_stage(&mut w.host.e, MQ), Some(10));
    assert_eq!(w.hd.social.enactor.enacted, 1);
    // The chosen line's subtitle names the player who chose it: the guest, not the starter.
    let m = w.host.messages();
    assert!(m.contains(&"Guest: Where is Sunny?".to_string()), "{m:?}");
    assert!(
        m.contains(&"Head on up to the saloon.".to_string()),
        "{m:?}"
    );
    // The word that it may join names the player in it.
    assert!(
        m.contains(&"Guest is talking to someone. Talk to them to join.".to_string()),
        "{m:?}"
    );
    assert_eq!(w.hd.social.talker.lines_far_shown, 1);
    w.guests[0].game.say(SHARED_LINE, true);
    w.run(10);
    assert_eq!(forms::quest_stage(&mut w.host.e, MQ), Some(20));
    // The host busy in its inventory: told who is talking, the guest (not itself, the starter).
    w.host.messages();
    w.host.open_menu(INVENTORY);
    w.guests[0].game.say(PLAIN_LINE, false);
    w.guests[0].game.say(PLAIN_LINE, true);
    w.run(10);
    let m = w.host.messages();
    assert!(
        m.contains(&"Guest is talking to someone.".to_string()),
        "{m:?}"
    );
    w.host.close_menu();
    w.run(10);
    // The host, out of it, talks to Chet: a conversation of its own.
    w.host.place(CHET, SALOON);
    assert!(w.host.talk_to(CHET));
    w.host.say(PLAIN_LINE, false);
    w.run(10);
    assert_eq!(w.hd.social.talker.opened, 2);
    assert_eq!(w.hd.sync.live.dialogue.open_sessions(), 2);
    w.host.say(PLAIN_LINE, true);
    w.host.leave();
    w.run(10);
    // The guest leaves too: no one is left, the conversation ends, Sunny is free.
    w.guests[0].game.click_leave();
    w.run(10);
    assert_eq!(w.hd.sync.live.dialogue.open_sessions(), 0);
    assert!(!w
        .hd
        .sync
        .live
        .dialogue
        .is_talking(coop_core::ids::FormId(SUNNY)));
    w.assert_in_step();
}

#[test]
fn two_players_who_start_at_once_the_host_lets_one_talk_and_closes_the_others_menu() {
    let mut w = World::new(&["Sunny", "Chet"]);
    w.run(20);
    // The same frame: neither game knows of the other's conversation yet.
    assert!(w.guests[0].game.talk_to(SUNNY));
    assert!(w.guests[1].game.talk_to(SUNNY));
    w.guests[0].game.say(SHARED_LINE, false);
    w.guests[1].game.say(SHARED_LINE, false);
    w.run(10);
    assert_eq!(w.guests[0].driver.social.talker.opened, 1);
    assert_eq!(w.guests[1].driver.social.talker.denied, 1);
    assert_eq!(talk::menu_npc(&mut w.guests[1].game.e), None);
    assert_eq!(talk::menu_npc(&mut w.guests[0].game.e), Some(SUNNY));
    // One result on the host, for the one conversation.
    assert_eq!(w.hd.social.enactor.enacted, 1);
    assert_eq!(forms::quest_stage(&mut w.host.e, MQ), Some(10));
    w.assert_in_step();
}

#[test]
fn a_pick_the_host_cannot_run_is_left_alone_and_named() {
    let mut w = World::new(&["Sunny"]);
    w.run(20);
    // Sunny is not in the host's game now (her cell is not loaded there).
    w.host.set_present(SUNNY, false);
    assert!(w.guests[0].game.talk_to(SUNNY));
    w.guests[0].game.say(SHARED_LINE, false);
    w.run(10);
    assert_eq!(w.hd.social.enactor.enacted, 0);
    assert_eq!(w.hd.social.enactor.left, 1);
    assert!(w.hd.social.enactor.notes[0].contains("could not be run here"));
    assert_eq!(forms::quest_stage(&mut w.host.e, MQ), Some(0));
    w.assert_in_step();
}

// ---- Party saves, rejoining, joining late ----

#[test]
fn a_party_save_holds_each_guests_part_and_a_load_gives_it_back() {
    let dir = party_dir("party-save");
    let mut w = World::new(&["Sunny", "Chet"]);
    w.hd.social.party.dir = Some(dir.clone());
    // Guest 1 has played its own opening and stands at a place of its own.
    w.guests[0].game.set_stage(OWN, 5);
    w.guests[0].game.set_quest_var(OWN, 1, 2.5);
    w.guests[0]
        .game
        .move_player(SALOON, [10.0, 20.0, 30.0], 1.0);
    w.run(20);
    let key = party::key_of("Sunny");
    let held =
        w.hd.social
            .party
            .held(key)
            .expect("the host holds the part");
    assert_eq!(
        held.place.map(|p| (p.space, p.pos)),
        Some((SALOON, [10.0, 20.0, 30.0]))
    );
    assert_eq!(held.quests.len(), 1);
    assert_eq!(held.quests[0].vars, [(1, 2.5f64.to_bits())]);
    // No save yet: no file. The host saves: a file per guest, named after the save.
    assert!(!dir.exists());
    assert_eq!(w.host.save("Save 3 - Goodsprings.fos"), None);
    w.run(2);
    let file = dir.join(format!("Save 3 - Goodsprings.{key:016x}.nvcoop"));
    assert_eq!(
        Part::decode(&std::fs::read(&file).unwrap()),
        Some(held.clone())
    );
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
    assert_eq!(
        (w.hd.social.party.saved, w.hd.social.party.files_written),
        (1, 2)
    );
    // The guest plays on: the host's copy follows, the saved file does not.
    w.guests[0].game.set_stage(OWN, 9);
    w.guests[0].game.move_player(STORE, [1.0, 2.0, 3.0], 0.0);
    w.run(120);
    assert_eq!(
        w.hd.social.party.held(key).unwrap().place.map(|p| p.space),
        Some(STORE)
    );
    assert_eq!(
        Part::decode(&std::fs::read(&file).unwrap()),
        Some(held.clone())
    );
    assert_eq!(w.guests[0].driver.social.party.restored, 0);
    // The host loads that save: each guest gets its part back, once.
    assert_eq!(w.host.load("Save 3 - Goodsprings.fos"), None);
    w.run(20);
    assert_eq!(w.hd.social.party.given_back, 2);
    for g in &w.guests {
        assert_eq!(g.driver.social.party.restored, 1);
    }
    let l = live::local(&mut w.guests[0].game.e).unwrap();
    assert_eq!(
        (l.cell, l.pos, l.heading),
        (SALOON, [10.0, 20.0, 30.0], 1.0)
    );
    assert_eq!(w.guests[0].game.warps(), 1);
    assert_eq!(w.guests[0].driver.social.party.last.quests, 1);
    w.run(200);
    for g in &w.guests {
        assert_eq!(g.driver.social.party.restored, 1, "given back once");
    }
    w.assert_in_step();
    // A save with no party files gives nothing back.
    assert_eq!(w.host.load("Save 1"), None);
    w.run(20);
    assert_eq!(w.hd.social.party.given_back, 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_player_who_joins_late_gets_the_snapshot_and_its_own_saved_part() {
    let dir = party_dir("late-join");
    // An earlier session: Sunny played and the host saved.
    {
        let mut w = World::new(&["Sunny"]);
        w.hd.social.party.dir = Some(dir.clone());
        w.guests[0].game.set_stage(OWN, 5);
        w.guests[0].game.move_player(STORE, [7.0, 8.0, 9.0], 0.5);
        w.run(20);
        w.host.save("autosave");
        w.run(2);
    }
    // A new session: the host loads the save and plays on, a conversation among it.
    let mut w = World::new(&[]);
    w.hd.social.party.dir = Some(dir.clone());
    w.host.load("autosave");
    assert!(w.host.talk_to(SUNNY));
    w.host.say(SHARED_LINE, false);
    w.run(20);
    // Sunny joins the running game from a game of its own that just started, and a stranger too.
    let sunny = w.join(guest_story(), "Sunny");
    let other = w.join(guest_story(), "Stranger");
    w.run(60);
    w.assert_in_step();
    assert_eq!(
        forms::quest_stage(&mut w.guests[sunny].game.e, MQ),
        Some(10)
    );
    assert_eq!(w.guests[sunny].driver.social.party.restored, 1);
    assert_eq!(
        forms::quest_stage(&mut w.guests[sunny].game.e, OWN),
        Some(5)
    );
    let l = live::local(&mut w.guests[sunny].game.e).unwrap();
    assert_eq!((l.cell, l.pos), (STORE, [7.0, 8.0, 9.0]));
    assert_eq!(w.guests[other].driver.social.party.restored, 0);
    assert_eq!(
        forms::quest_stage(&mut w.guests[other].game.e, OWN),
        Some(0)
    );
    // The story's scripts never ran on either.
    assert_eq!(w.guests[sunny].game.scripts_run(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_guest_that_drops_keeps_its_game_and_one_that_starts_anew_gets_its_part_back() {
    let mut w = World::new(&["Sunny"]);
    w.guests[0].game.set_stage(OWN, 5);
    w.guests[0].game.move_player(STORE, [7.0, 8.0, 9.0], 0.5);
    w.run(20);
    // The connection drops; the host plays on; the guest walks on in its own game.
    w.hd.sync.disconnect(PlayerId(1));
    w.net.flush(PlayerId(1));
    w.net.flush(PlayerId::HOST);
    assert!(w.host.set_stage(MQ, 10));
    for _ in 0..10 {
        w.t += 50;
        w.hd.step(&mut w.host.e, &mut w.hostnet, w.t);
    }
    assert_eq!(w.hd.social.party.left_behind, 1);
    w.net.flush(PlayerId(1));
    w.guests[0].game.move_player(SALOON, [1.0, 1.0, 1.0], 0.0);
    // It comes back with its game still running: the story catches up, its own state stays.
    {
        let g = &mut w.guests[0];
        g.driver.rejoin(PlayerId(1), &mut g.net);
    }
    w.run(60);
    w.assert_in_step();
    assert_eq!(w.guests[0].driver.social.party.restored, 0);
    assert_eq!(live::local(&mut w.guests[0].game.e).unwrap().cell, SALOON);
    // It drops again, and this time its game is started anew: the part the host kept comes back.
    w.run(120);
    w.hd.sync.disconnect(PlayerId(1));
    w.net.flush(PlayerId(1));
    w.net.flush(PlayerId::HOST);
    w.t += 50;
    w.hd.step(&mut w.host.e, &mut w.hostnet, w.t);
    w.net.flush(PlayerId(1));
    w.guests.clear();
    w.join(guest_story(), "Sunny");
    w.run(60);
    w.assert_in_step();
    assert_eq!(w.guests[0].driver.social.party.restored, 1);
    assert_eq!(forms::quest_stage(&mut w.guests[0].game.e, OWN), Some(5));
    let l = live::local(&mut w.guests[0].game.e).unwrap();
    assert_eq!((l.cell, l.pos), (SALOON, [1.0, 1.0, 1.0]));
}

#[test]
fn a_guest_still_at_its_menu_gets_its_part_once_it_is_in_the_world() {
    let dir = party_dir("menu-join");
    {
        let mut w = World::new(&["Sunny"]);
        w.hd.social.party.dir = Some(dir.clone());
        w.guests[0].game.set_stage(OWN, 5);
        w.run(20);
        w.host.save("quicksave");
        w.run(2);
    }
    let mut w = World::new(&[]);
    w.hd.social.party.dir = Some(dir.clone());
    w.host.load("quicksave");
    // A game at its main menu: a player in no cell.
    let mut game = Story::new(&StorySpec {
        quests: vec![(MQ, vec![5, 10, 20, 30]), (OWN, vec![5, 9])],
        globals: vec![(HOUR, 8.0)],
        vars: vec![(OWN, vec![1])],
        cells: vec![SALOON, STORE],
        ..StorySpec::default()
    });
    gate_guest(&mut game.e);
    w.join(game, "Sunny");
    w.run(40);
    assert_eq!(w.guests[0].driver.social.party.restored, 0);
    assert_eq!(w.guests[0].driver.social.party.uploads, 0);
    w.guests[0].game.move_player(STORE, [0.0; 3], 0.0);
    w.run(5);
    assert_eq!(w.guests[0].driver.social.party.restored, 1);
    assert_eq!(forms::quest_stage(&mut w.guests[0].game.e, OWN), Some(5));
    assert_eq!(live::local(&mut w.guests[0].game.e).unwrap().cell, SALOON);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- Sleep and wait ----

fn hour(s: &mut Story) -> f32 {
    forms::global_value(&mut s.e, HOUR).unwrap()
}

#[test]
fn a_rest_everyone_agrees_to_passes_for_everyone() {
    let mut w = World::new(&["Sunny", "Chet"]);
    w.run(30);
    // Guest 1 asks to wait three hours: its own rest does not start; the others are told.
    w.guests[0].game.click_rest(3, false);
    w.run(5);
    assert_eq!(w.guests[0].game.rest_clicks(), (0, 1));
    assert_eq!(w.guests[0].driver.social.rester.proposed, 1);
    for s in [&mut w.host, &mut w.guests[1].game] {
        assert_eq!(
            s.messages().last().map(String::as_str),
            Some("Guest wants to wait 3 hour(s). Rest yourself to agree.")
        );
    }
    assert_eq!(hour(&mut w.host), 8.0);
    // The host agrees by resting itself (whatever hours its own menu shows); still one to go.
    w.host.click_rest(1, false);
    w.run(5);
    assert_eq!(w.hd.social.rester.agreed, 1);
    assert_eq!(w.host.hours_passed().0, 0);
    // The last player agrees: three hours pass in every game, through the game's hour step.
    w.guests[1].game.click_rest(8, true);
    w.run(40);
    assert_eq!(w.host.hours_passed().0, 3);
    assert_eq!(hour(&mut w.host), 11.0);
    assert_eq!(w.hd.social.rester.passed, 1);
    for g in &mut w.guests {
        assert_eq!(g.game.hours_passed().0, 3);
        assert_eq!(hour(&mut g.game), 11.0, "the host's clock");
        assert_eq!(g.driver.social.rester.passed, 1);
        assert_eq!(g.game.rest_clicks().0, 0, "no game rested by itself");
    }
    w.assert_in_step();
}

#[test]
fn a_rest_one_player_does_not_agree_to_passes_for_no_one() {
    let mut w = World::new(&["Sunny"]);
    w.run(30);
    w.guests[0].game.click_rest(5, true);
    w.run(5);
    assert_eq!(
        w.host.messages().last().map(String::as_str),
        Some("Guest wants to sleep 5 hour(s). Rest yourself to agree.")
    );
    // The host never answers: the vote runs out.
    w.run_every(40, 1000);
    assert_eq!(w.host.hours_passed().0, 0);
    assert_eq!(w.guests[0].game.hours_passed().0, 0);
    assert_eq!(hour(&mut w.host), 8.0);
    assert_eq!(w.guests[0].driver.social.rester.refused, 1);
    assert_eq!(
        w.guests[0].game.messages().last().map(String::as_str),
        Some("The party did not agree to rest.")
    );
    w.assert_in_step();
}

#[test]
fn a_host_alone_rests_as_the_game_does() {
    let mut w = World::new(&[]);
    w.run(5);
    w.host.click_rest(2, false);
    w.run(5);
    assert_eq!(w.host.rest_clicks(), (1, 0), "the game's own rest started");
    assert_eq!(w.hd.social.rester.proposed, 0);
    // And its conversations are its own too.
    assert!(w.host.talk_to(SUNNY));
    w.host.say(SHARED_LINE, false);
    w.run(5);
    assert_eq!(w.hd.social.talker.opened, 0);
    assert_eq!(forms::quest_stage(&mut w.host.e, MQ), Some(10));
}
