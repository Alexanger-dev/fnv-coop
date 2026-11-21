//! Which dialogue lines are voted on (S42 step 3, agreed with the owner 2026-11-18 and
//! 2026-11-21): only the choices that matter.
//!
//! A line (a topic info: what the NPC answers to the option the player picks) is voted on when
//! picking it does something that cannot be taken back or changes the story for both players:
//!
//! - it ends the conversation (the goodbye mark);
//! - it moves a quest (`SetStage`, `CompleteQuest`, `StartQuest`, `StopQuest`, an objective
//!   completed or shown, every objective completed or failed);
//! - it gives or takes something (items or caps, XP, perks, karma, reputation, a skill), but an
//!   item given to the player (`player.AddItem`) is not voted on by itself;
//! - it starts a fight or turns someone hostile or friendly (`StartCombat`, `SetEnemy`, `Kill`,
//!   a faction joined or left);
//! - it sends someone off (`AddScriptPackage`, `MoveTo`: an NPC walks away, the player is taken
//!   elsewhere);
//! - it is a speech check (the speech challenge mark: success and failure differ) or a skill
//!   check (an option shown with a skill's requirement).
//!
//! Every other line (a question that comes back to the same list) is not voted on: either player
//! picks it, for both. The two grey kinds were decided by the council (D10, 2026-11-21): a line
//! that only sets a script's variable is not voted on (3-0: it is a question's bookkeeping, what
//! the variable unlocks is voted on when it happens), and neither is a line that opens a menu
//! (barter, repair, crafting, a companion's things; 2-1): the menu opens only for the player who
//! picked it, whose game runs the line (the other games skip its result, R1); the other can pick
//! it after. Both stay noted on the verdict ([`Verdict::NoVote`]). A line whose result script this
//! reader cannot read (only its compiled form) is voted on, to be safe.
//!
//! The owner's corrections after reading the list (2026-11-21):
//!
//! - A free item (`player.AddItem`) is not voted on unless the line also moves a quest (or does
//!   anything else voted on): Sunny's "I ran out of ammo." Every player in the conversation gets
//!   the item (step 4).
//! - Doc Mitchell's tutorial (`VCG01`, the character's creation) is each player's own: none of
//!   its lines are voted on, the test's answers and his goodbye included ([`OWN_QUESTS`]).
//! - A few lines are not voted on by name ([`NOT_VOTED`]): Sunny's "I wanted to ask you some
//!   questions." while her quest runs only ends the conversation ("let's just finish this
//!   first"); her "I ran out of ammo." ends it with free ammo.
//!
//! The final verdict on a line is [`settle`] of [`with_follow_ups`] of [`classify`].
//!
//! The rule reads facts about a line ([`LineFacts`]) that the game's data gives, so the same rule
//! serves the offline list (`coop-votelist`, from the plugin files) and the game.

/// What a line's data says about it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineFacts {
    /// The goodbye mark: the line ends the conversation.
    pub goodbye: bool,
    /// The speech challenge mark: a speech check, success and failure lines.
    pub speech_challenge: bool,
    /// The option is shown with a skill's requirement (a condition on one of the player's
    /// skills).
    pub skill_check: bool,
    /// The source text of the line's result scripts (begin and end), when the data has it.
    pub scripts: Vec<String>,
    /// The line has a result script whose source text the data does not have.
    pub unreadable_script: bool,
}

/// Why a line is voted on, or grey.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reason {
    /// The goodbye mark.
    EndsConversation,
    /// A quest moved, by this command.
    ChangesQuest(String),
    /// Something given or taken, by this command.
    GivesOrTakes(String),
    /// A fight or a standing changed, by this command.
    Fight(String),
    /// Someone sent off, by this command.
    SendsOff(String),
    /// A speech check.
    SpeechCheck,
    /// A skill check.
    SkillCheck,
    /// A result script this reader cannot read (voted on, to be safe).
    UnreadableScript,
    /// Noted, not voted on: a menu for the player who picks it, by this command.
    OpensMenu(String),
    /// Noted, not voted on: a script variable set (`set a.b to ...`).
    SetsVariable(String),
    /// Noted, not voted on: an item given to the player, by this command; every player in the
    /// conversation gets it.
    GivesItem(String),
    /// Not voted on: a line of a quest each player has on their own ([`OWN_QUESTS`]).
    OwnQuest(String),
    /// Not voted on: a line the owner named ([`NOT_VOTED`]), for this reason.
    Named(&'static str),
}

impl Reason {
    /// A short text for the list.
    pub fn text(&self) -> String {
        match self {
            Reason::EndsConversation => "ends the conversation".into(),
            Reason::ChangesQuest(c) => format!("moves a quest ({c})"),
            Reason::GivesOrTakes(c) => format!("gives or takes something ({c})"),
            Reason::Fight(c) => format!("fight or standing ({c})"),
            Reason::SendsOff(c) => format!("sends someone off ({c})"),
            Reason::SpeechCheck => "speech check".into(),
            Reason::SkillCheck => "skill check".into(),
            Reason::UnreadableScript => "result script not readable here".into(),
            Reason::OpensMenu(c) => format!("opens a menu for the player who picks it ({c})"),
            Reason::SetsVariable(v) => format!("sets a script variable ({v})"),
            Reason::GivesItem(c) => format!("gives a free item, every player gets it ({c})"),
            Reason::OwnQuest(q) => format!("each player's own ({q})"),
            Reason::Named(why) => format!("named by the owner: {why}"),
        }
    }

    /// Whether the reason is only noted: the line is not voted on for it.
    fn is_note(&self) -> bool {
        matches!(
            self,
            Reason::OpensMenu(_)
                | Reason::SetsVariable(_)
                | Reason::GivesItem(_)
                | Reason::OwnQuest(_)
                | Reason::Named(_)
        )
    }
}

/// A line's verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Voted on, for these reasons.
    Vote(Vec<Reason>),
    /// Not voted on: either player picks it, for both; with what the line does that is only noted
    /// (a variable set, a menu for the picker), none for a plain question.
    NoVote(Vec<Reason>),
}

impl Verdict {
    /// `"vote"` or `"no vote"`.
    pub fn name(&self) -> &'static str {
        match self {
            Verdict::Vote(_) => "vote",
            Verdict::NoVote(_) => "no vote",
        }
    }

    /// The reasons (or notes), as one text.
    pub fn reasons(&self) -> String {
        let (Verdict::Vote(r) | Verdict::NoVote(r)) = self;
        r.iter().map(Reason::text).collect::<Vec<_>>().join("; ")
    }

    /// The verdict on these reasons: voted on if any is more than a note.
    fn of(mut reasons: Vec<Reason>) -> Verdict {
        let vote = reasons.iter().any(|r| !r.is_note());
        if vote {
            reasons.retain(|r| !r.is_note());
        }
        reasons.sort();
        reasons.dedup();
        if vote {
            Verdict::Vote(reasons)
        } else {
            Verdict::NoVote(reasons)
        }
    }
}

const QUEST: &[&str] = &[
    "setstage",
    "completequest",
    "startquest",
    "stopquest",
    "setobjectivecompleted",
    "setobjectivedisplayed",
    "completeallobjectives",
    "failallobjectives",
];
const GIVES: &[&str] = &[
    "additem",
    "removeitem",
    "removeallitems",
    "additemhealthpercent",
    "rewardxp",
    "rewardkarma",
    "addperk",
    "removeperk",
    "addreputation",
    "removereputation",
    "addreputationexact",
    "removereputationexact",
    "modpcskill",
];
const FIGHT: &[&str] = &[
    "startcombat",
    "setenemy",
    "setally",
    "kill",
    "killactor",
    "sendassaultalarm",
    "addtofaction",
    "removefromfaction",
    "setfactionrank",
];
const SENDS: &[&str] = &["addscriptpackage", "moveto"];
const MENUS: &[&str] = &[
    "showbartermenu",
    "showrepairmenu",
    "showrecipemenu",
    "showrecipemenucategory",
    "openteammatecontainer",
    "showcompanionwheel",
];

/// The commands of a script's source text: per statement, the command's name in lower case
/// (after a reference and a dot, `player.AddItem` gives `additem`; control words such as `if` come
/// out as names no list holds) with the reference it is run on (`player`, none for the speaker),
/// and for `set a.b to ...` statements the variable set. Comments (after `;`) are left out.
fn statements(text: &str) -> Vec<Statement> {
    let mut out = Vec::new();
    for line in text.lines() {
        let code = line.split(';').next().unwrap_or("");
        let mut words = code.split_whitespace();
        let Some(first) = words.next() else {
            continue;
        };
        let first = first.to_ascii_lowercase();
        if first == "set" {
            let var = words.next().unwrap_or("").to_string();
            out.push(Statement {
                name: "set".into(),
                var: Some(var),
                on: None,
            });
            continue;
        }
        let (on, name) = match first.rsplit_once('.') {
            Some((on, name)) => (Some(on.to_string()), name.to_string()),
            None => (None, first),
        };
        out.push(Statement {
            name,
            var: None,
            on,
        });
    }
    out
}

/// One statement of a script.
struct Statement {
    /// The command's name, lower case.
    name: String,
    /// For `set`: the variable set.
    var: Option<String>,
    /// The reference it is run on, lower case.
    on: Option<String>,
}

/// Commands that give the player an item, when run on the player.
const GIFTS: &[&str] = &["additem", "additemhealthpercent"];

/// Whether a reference is the player.
fn is_player(on: Option<&str>) -> bool {
    matches!(on, Some("player" | "playerref"))
}

/// The verdict on a line.
pub fn classify(f: &LineFacts) -> Verdict {
    let mut reasons = Vec::new();
    if f.goodbye {
        reasons.push(Reason::EndsConversation);
    }
    if f.speech_challenge {
        reasons.push(Reason::SpeechCheck);
    }
    if f.skill_check {
        reasons.push(Reason::SkillCheck);
    }
    for text in &f.scripts {
        for Statement { name: cmd, var, on } in statements(text) {
            reasons.push(if let Some(var) = var {
                Reason::SetsVariable(var)
            } else if GIFTS.contains(&cmd.as_str()) && is_player(on.as_deref()) {
                Reason::GivesItem(cmd)
            } else if QUEST.contains(&cmd.as_str()) {
                Reason::ChangesQuest(cmd)
            } else if GIVES.contains(&cmd.as_str()) {
                Reason::GivesOrTakes(cmd)
            } else if FIGHT.contains(&cmd.as_str()) {
                Reason::Fight(cmd)
            } else if SENDS.contains(&cmd.as_str()) {
                Reason::SendsOff(cmd)
            } else if MENUS.contains(&cmd.as_str()) {
                Reason::OpensMenu(cmd)
            } else {
                continue;
            });
        }
    }
    if f.unreadable_script {
        reasons.push(Reason::UnreadableScript);
    }
    Verdict::of(reasons)
}

/// The verdict on a line together with the lines that follow it on their own (the game says the
/// first of a line's follow-ups whose conditions hold, with no choice in between): what those do
/// is what picking the line does, so a line is voted on when it or any of its follow-ups is.
pub fn with_follow_ups(own: &Verdict, follow_ups: &[Verdict]) -> Verdict {
    let mut reasons = Vec::new();
    for verdict in std::iter::once(own).chain(follow_ups) {
        let (Verdict::Vote(r) | Verdict::NoVote(r)) = verdict;
        reasons.extend(r.iter().cloned());
    }
    Verdict::of(reasons)
}

/// Quests each player has on their own: their lines are not voted on. Doc Mitchell's tutorial,
/// the character's creation: his test's answers only suggest the tag skills, which each player
/// picks for their own build.
pub const OWN_QUESTS: &[&str] = &["VCG01"];

/// Lines not voted on by name (the owner, 2026-11-21): form IDs of `FalloutNV.esm` (the same in
/// the game: its mod index is 0), with the reason.
pub const NOT_VOTED: &[(u32, &str)] = &[
    (0x0010_B753, "Sunny gives free ammo; every player gets it"),
    (
        0x0010_B754,
        "Sunny says to finish the quest first; nothing changes",
    ),
    (
        0x0015_F508,
        "Sunny says to finish the quest first; nothing changes",
    ),
];

/// The final verdict on a line of quest `quest` (its editor ID) and form ID `line` (a
/// `FalloutNV.esm` form ID, 0 for a line of another plugin), from its verdict with its follow-ups:
/// a line of a quest each player has on their own, or one named, is not voted on.
pub fn settle(verdict: Verdict, quest: &str, line: u32) -> Verdict {
    if let Some(own) = OWN_QUESTS.iter().find(|q| q.eq_ignore_ascii_case(quest)) {
        return Verdict::NoVote(vec![Reason::OwnQuest((*own).to_string())]);
    }
    if let Some(&(_, why)) = NOT_VOTED.iter().find(|(id, _)| *id == line) {
        return Verdict::NoVote(vec![Reason::Named(why)]);
    }
    verdict
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(text: &str) -> LineFacts {
        LineFacts {
            scripts: vec![text.into()],
            ..LineFacts::default()
        }
    }

    #[test]
    fn a_question_that_changes_nothing_is_not_voted_on() {
        assert_eq!(classify(&LineFacts::default()), Verdict::NoVote(vec![]));
        assert_eq!(
            classify(&script("; just talk\nSunnyREF.evp\nShowMessage VMS01Msg")),
            Verdict::NoVote(vec![])
        );
    }

    #[test]
    fn lines_that_matter_are_voted_on_with_their_reasons() {
        // Sunny's tutorial: the quest moved and she walks off.
        let v = classify(&script(
            "SetObjectiveCompleted VCG02 10 1\nSetStage VCG02 25 ; next\nSunnyREF.AddScriptPackage VCG02SunnyFollow",
        ));
        assert_eq!(
            v,
            Verdict::Vote(vec![
                Reason::ChangesQuest("setobjectivecompleted".into()),
                Reason::ChangesQuest("setstage".into()),
                Reason::SendsOff("addscriptpackage".into()),
            ])
        );
        assert_eq!(v.name(), "vote");
        assert!(v.reasons().contains("moves a quest (setstage)"));
        for (text, reason) in [
            (
                "player.AddPerk Toughness",
                Reason::GivesOrTakes("addperk".into()),
            ),
            ("RewardXP 50", Reason::GivesOrTakes("rewardxp".into())),
            ("StartCombat player", Reason::Fight("startcombat".into())),
            (
                "player.MoveTo VegasMarker",
                Reason::SendsOff("moveto".into()),
            ),
        ] {
            assert_eq!(classify(&script(text)), Verdict::Vote(vec![reason]));
        }
        let marks = LineFacts {
            goodbye: true,
            speech_challenge: true,
            skill_check: true,
            ..LineFacts::default()
        };
        assert_eq!(
            classify(&marks),
            Verdict::Vote(vec![
                Reason::EndsConversation,
                Reason::SpeechCheck,
                Reason::SkillCheck
            ])
        );
    }

    #[test]
    fn variables_and_menus_are_noted_but_not_voted_on_and_unreadable_scripts_are_voted_on() {
        assert_eq!(
            classify(&script("set VMS01.bRude to 1")),
            Verdict::NoVote(vec![Reason::SetsVariable("VMS01.bRude".into())])
        );
        let menu = classify(&script("ShowBarterMenu\nset a.b to 2"));
        assert_eq!(
            menu,
            Verdict::NoVote(vec![
                Reason::OpensMenu("showbartermenu".into()),
                Reason::SetsVariable("a.b".into())
            ])
        );
        assert_eq!(menu.name(), "no vote");
        assert!(menu
            .reasons()
            .contains("opens a menu for the player who picks it"));
        let unread = LineFacts {
            unreadable_script: true,
            ..LineFacts::default()
        };
        assert_eq!(
            classify(&unread),
            Verdict::Vote(vec![Reason::UnreadableScript])
        );
        // A line that matters is voted on, whatever else it does; notes are left out then.
        assert_eq!(
            classify(&script(
                "set VMS01.bRude to 1\nShowRepairMenu\nSetStage VMS01 20"
            )),
            Verdict::Vote(vec![Reason::ChangesQuest("setstage".into())])
        );
    }

    #[test]
    fn a_line_is_voted_on_when_a_line_that_follows_it_is() {
        // Sunny's "[END TUTORIAL]": nothing itself; her next line ends the tutorial.
        let none = Verdict::NoVote(vec![]);
        let quest = Verdict::Vote(vec![Reason::ChangesQuest("setstage".into())]);
        let note = Verdict::NoVote(vec![Reason::SetsVariable("a.b".into())]);
        assert_eq!(with_follow_ups(&none, &[]), none);
        assert_eq!(
            with_follow_ups(&none, &[none.clone(), quest.clone()]),
            quest
        );
        assert_eq!(with_follow_ups(&note, std::slice::from_ref(&none)), note);
        assert_eq!(with_follow_ups(&none, std::slice::from_ref(&note)), note);
        // Voted on beats a note; reasons merged once, in order.
        let goodbye = Verdict::Vote(vec![Reason::EndsConversation]);
        assert_eq!(
            with_follow_ups(&quest, &[note, goodbye, quest.clone()]),
            Verdict::Vote(vec![
                Reason::EndsConversation,
                Reason::ChangesQuest("setstage".into())
            ])
        );
    }

    #[test]
    fn a_free_item_alone_is_not_voted_on_unless_the_line_moves_a_quest() {
        // Sunny's "I ran out of ammo.": ends the conversation, gives ammo.
        let ammo = LineFacts {
            goodbye: true,
            scripts: vec!["player.additem Ammo556mm 30".into()],
            ..LineFacts::default()
        };
        assert_eq!(
            classify(&ammo),
            Verdict::Vote(vec![Reason::EndsConversation])
        );
        let free = script("Player.AddItem Ammo556mm 30");
        let given = classify(&free);
        assert_eq!(
            given,
            Verdict::NoVote(vec![Reason::GivesItem("additem".into())])
        );
        assert!(given.reasons().contains("every player gets it"));
        assert_eq!(
            classify(&script("PlayerREF.AddItemHealthPercent Gun 1 0.5")),
            Verdict::NoVote(vec![Reason::GivesItem("additemhealthpercent".into())])
        );
        // Given with a quest moved: voted on.
        assert_eq!(
            classify(&script("player.additem Caps001 50\nSetStage VMS01 30")),
            Verdict::Vote(vec![Reason::ChangesQuest("setstage".into())])
        );
        // Given to someone else, or taken: voted on as before.
        for (text, cmd) in [
            ("SunnyREF.AddItem Ammo556mm 30", "additem"),
            ("AddItem Ammo556mm 30", "additem"),
            ("player.RemoveItem Caps001 100", "removeitem"),
        ] {
            assert_eq!(
                classify(&script(text)),
                Verdict::Vote(vec![Reason::GivesOrTakes(cmd.into())])
            );
        }
    }

    #[test]
    fn doc_mitchells_tutorial_and_named_lines_are_not_voted_on() {
        let vote = Verdict::Vote(vec![Reason::EndsConversation]);
        // Doc Mitchell's test answers and his goodbye (VCG01).
        assert_eq!(
            settle(vote.clone(), "VCG01", 0x0010_55B0),
            Verdict::NoVote(vec![Reason::OwnQuest("VCG01".into())])
        );
        assert_eq!(
            settle(vote.clone(), "vcg01", 0),
            Verdict::NoVote(vec![Reason::OwnQuest("VCG01".into())])
        );
        // Sunny's "I wanted to ask you some questions." while her quest runs.
        for line in [0x0010_B754, 0x0015_F508] {
            let settled = settle(vote.clone(), "VCG02", line);
            assert_eq!(settled.name(), "no vote");
            assert!(settled.reasons().contains("finish the quest first"));
        }
        // Any other line keeps its verdict; a line of another plugin (0) is never named.
        assert_eq!(settle(vote.clone(), "VCG02", 0x0010_A1E5), vote);
        assert!(settle(vote.clone(), "VCG02", 0x0010_B753)
            .reasons()
            .contains("free ammo"));
        assert_eq!(settle(vote.clone(), "VCG02", 0), vote);
        let none = Verdict::NoVote(vec![]);
        assert_eq!(settle(none.clone(), "VCG02", 0x0010_A1E5), none);
        // A settled verdict merged again stays not voted on.
        for settled in [
            settle(vote.clone(), "VCG01", 0),
            settle(vote.clone(), "VCG02", 0x0010_B754),
        ] {
            assert_eq!(with_follow_ups(&settled, &[]).name(), "no vote");
        }
    }

    #[test]
    fn comments_control_words_and_repeats_are_read_as_the_game_would() {
        assert_eq!(
            classify(&script(
                "if GetStage VCG02 < 25 ; SetStage in a comment\n  SetStage VCG02 25\nelse\n  setstage vcg02 25\nendif"
            )),
            Verdict::Vote(vec![Reason::ChangesQuest("setstage".into())])
        );
        // "SetStage" only in a comment: nothing.
        assert_eq!(
            classify(&script("; SetStage VCG02 25")),
            Verdict::NoVote(vec![])
        );
        // A command written against its comment is still read.
        assert_eq!(
            classify(&script("StartCombat; on the player")),
            Verdict::Vote(vec![Reason::Fight("startcombat".into())])
        );
    }
}
