//! Every dialogue line a player can pick in a load order, with its co-op vote verdict
//! (`coop_core::dialogue::votes`, S42 step 3): the list for the owner (`coop-votelist`) and the
//! set of lines the game holds for a vote (S42 step 3b).
//!
//! A line here is a topic info of a topic the player can pick (a `DIAL` of type Topic that is
//! top-level, or is a choice (`TCLT`) or an added topic (`NAME`) of some line, other than the
//! greeting): the option the player picks and the NPC's answer. The facts the rule reads come
//! from the info's own data: the goodbye and speech challenge marks (`DATA`), a condition on one
//! of the player's skills or S.P.E.C.I.A.L. (`CTDA`, `GetActorValue` on the target or the
//! player, any comparison: the failure line of a check is a check too), and the source text of
//! its result scripts (`SCTX`; the game keeps only their compiled form, so the rule reads the
//! plugin files). The lines that follow a line on their own (`TCFU`, followed further) count as
//! that line's (`with_follow_ups`): Sunny's "[END TUTORIAL]" does nothing itself, the line she
//! says after it ends the tutorial. Last, `settle`: the owner's exceptions (quests each player
//! has on their own, named lines).
//!
//! Every form ID here is the load order's (the game's), the last plugin's copy of a line the one
//! that counts.

use std::collections::{HashMap, HashSet};

use coop_core::dialogue::votes::{classify, settle, with_follow_ups, LineFacts, Reason, Verdict};
use esm::{sig, FormId, FourCC, LoadOrder, Subrecord};

const QSTI: FourCC = FourCC::new(b"QSTI");
const RNAM: FourCC = FourCC::new(b"RNAM");
const NAM1: FourCC = FourCC::new(b"NAM1");
const CTDA: FourCC = FourCC::new(b"CTDA");
const ANAM: FourCC = FourCC::new(b"ANAM");
const SCDA: FourCC = FourCC::new(b"SCDA");
const SCTX: FourCC = FourCC::new(b"SCTX");
const TCLT: FourCC = FourCC::new(b"TCLT");
const TCFU: FourCC = FourCC::new(b"TCFU");

/// `DATA` flag bits of an info.
const INFO_GOODBYE: u8 = 0x01;
const INFO_SPEECH_CHALLENGE: u8 = 0x80;
/// A topic's type (the first byte of its `DATA`) for the player's topics, and its top-level flag
/// (the second byte).
const DIAL_TOPIC: u8 = 0;
const DIAL_TOP_LEVEL: u8 = 0x02;
/// Condition functions: `GetActorValue`, `GetIsID`.
const FN_GET_ACTOR_VALUE: u32 = 14;
const FN_GET_IS_ID: u32 = 72;
/// Condition targets: the subject (the speaker), the target (the player), a reference.
const RUN_ON_SUBJECT: u32 = 0;
const RUN_ON_REFERENCE: u32 = 2;
const PLAYER_REF: u32 = 0x14;
/// Actor values: S.P.E.C.I.A.L. and the skills.
const SPECIAL: std::ops::RangeInclusive<u32> = 5..=11;
const SKILLS: std::ops::RangeInclusive<u32> = 32..=45;
const ANSWER_MAX: usize = 160;

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    data.get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// One condition's function, first parameter, target and reference.
struct Condition {
    op: u8,
    function: u32,
    param: u32,
    run_on: u32,
    reference: u32,
}

fn condition(sub: &Subrecord) -> Option<Condition> {
    let d = &sub.data;
    Some(Condition {
        op: d.first()? >> 5,
        function: u32_at(d, 8)? & 0xffff,
        param: u32_at(d, 12)?,
        run_on: u32_at(d, 20).unwrap_or(RUN_ON_SUBJECT),
        reference: u32_at(d, 24).unwrap_or(0),
    })
}

/// Whether a condition compares one of the player's skills or S.P.E.C.I.A.L. with a value
/// (greater or less: the success and the failure line of a check).
fn is_skill_check(c: &Condition) -> bool {
    let on_player = c.run_on == 1 || (c.run_on == RUN_ON_REFERENCE && c.reference == PLAYER_REF);
    c.function == FN_GET_ACTOR_VALUE
        && on_player
        && (2..=5).contains(&c.op)
        && (SKILLS.contains(&c.param) || SPECIAL.contains(&c.param))
}

/// Condition functions on the player that can differ between players: `GetActorValue`,
/// `GetItemCount`, `GetLevel`, `GetBaseActorValue`, `HasPerk`, `GetPermanentActorValue`.
const PLAYER_FUNCTIONS: [u32; 6] = [14, 47, 80, 277, 449, 495];

/// Whether a condition tests something of the player that can differ between players.
fn is_player_check(c: &Condition) -> bool {
    let on_player = c.run_on == 1 || (c.run_on == RUN_ON_REFERENCE && c.reference == PLAYER_REF);
    on_player && PLAYER_FUNCTIONS.contains(&c.function)
}

fn clean(text: &str) -> String {
    text.replace(['\t', '\r', '\n'], " ").trim().to_string()
}

/// One line a player can pick, with its verdict.
#[derive(Clone, Debug)]
pub struct Line {
    /// The plugin whose copy of the line counts.
    pub plugin: String,
    /// The line's form ID in the load order.
    pub form: u32,
    /// Its quest's editor ID.
    pub quest: String,
    /// The NPC who answers (its name), when the line names one.
    pub speaker: String,
    /// Its topic's editor ID.
    pub topic: String,
    /// The option the player picks.
    pub option: String,
    /// The start of the NPC's answer.
    pub answer: String,
    /// The verdict on what the line itself does.
    pub own: Verdict,
    /// The verdict on what the lines that follow it do.
    pub follow: Verdict,
    /// The final verdict.
    pub verdict: Verdict,
    /// The owner's exception that decided the verdict, if one did.
    pub exception: String,
    /// Its conditions test something of the player that can differ between players (an actor
    /// value, items or caps held, a perk, the level): see [`LineSets::checks`].
    pub player_check: bool,
}

/// A line as read, before its follow-ups are known.
struct Read {
    line: Line,
    /// Its topic is one a player can pick.
    picked: bool,
    follow_ups: Vec<u32>,
}

fn name_of(order: &LoadOrder, id: u32) -> String {
    order
        .get(FormId(id))
        .and_then(|r| r.record().ok())
        .and_then(|r| {
            r.full_name()
                .filter(|n| !n.is_empty())
                .or_else(|| r.editor_id())
        })
        .unwrap_or_else(|| format!("{id:08X}"))
}

fn editor_id_of(order: &LoadOrder, id: u32) -> String {
    order
        .get(FormId(id))
        .and_then(|r| r.editor_id().ok().flatten())
        .unwrap_or_else(|| format!("{id:08X}"))
}

/// Every line a player can pick in `order`, with its verdict; and how many other lines there are
/// (greetings, remarks, follow-ups).
pub fn lines(order: &LoadOrder) -> (Vec<Line>, usize) {
    // The topics a player can pick: the choices and added topics of every line.
    let mut picked: HashSet<u32> = HashSet::new();
    for r in order.records_of_type(sig::INFO) {
        let Ok(record) = r.record() else {
            continue;
        };
        for sub in record.get_all(TCLT).chain(record.get_all(sig::NAME)) {
            if let Some(id) = u32_at(&sub.data, 0) {
                picked.insert(r.plugin.to_global(FormId(id)).0);
            }
        }
    }
    let mut topics: HashMap<u32, Option<(String, String)>> = HashMap::new();
    let mut read: Vec<Read> = Vec::new();
    for r in order.records_of_type(sig::INFO) {
        if r.entry.header.is_deleted() {
            continue;
        }
        let Some(topic_id) = r.entry.topic.map(|t| r.plugin.to_global(t).0) else {
            continue;
        };
        let topic = topics
            .entry(topic_id)
            .or_insert_with(|| {
                let dial = order.get(FormId(topic_id))?.record().ok()?;
                let data = &dial.get(sig::DATA)?.data;
                let top_level = data.get(1).is_some_and(|f| f & DIAL_TOP_LEVEL != 0);
                let edid = dial.editor_id().unwrap_or_default();
                (data.first() == Some(&DIAL_TOPIC)
                    && (top_level || picked.contains(&topic_id))
                    && !edid.eq_ignore_ascii_case("GREETING"))
                .then(|| (edid, dial.full_name().unwrap_or_default()))
            })
            .clone();
        let Ok(record) = r.record() else {
            continue;
        };
        let global = |id: u32| r.plugin.to_global(FormId(id)).0;
        let flags = record
            .get(sig::DATA)
            .and_then(|d| d.data.get(2).copied())
            .unwrap_or(0);
        let conditions: Vec<Condition> = record.get_all(CTDA).filter_map(condition).collect();
        let scripts: Vec<String> = record
            .get_all(SCTX)
            .map(|s| {
                String::from_utf8_lossy(&s.data)
                    .trim_end_matches('\0')
                    .to_string()
            })
            .filter(|s| !s.trim().is_empty())
            .collect();
        let compiled = record.get_all(SCDA).filter(|s| !s.data.is_empty()).count();
        let facts = LineFacts {
            goodbye: flags & INFO_GOODBYE != 0,
            speech_challenge: flags & INFO_SPEECH_CHALLENGE != 0,
            skill_check: conditions.iter().any(is_skill_check),
            unreadable_script: compiled > scripts.len(),
            scripts,
        };
        let follow_ups = record
            .get_all(TCFU)
            .filter_map(|s| u32_at(&s.data, 0))
            .map(global)
            .collect();
        let speaker = conditions
            .iter()
            .find(|c| c.function == FN_GET_IS_ID && c.run_on == RUN_ON_SUBJECT && c.op == 0)
            .map(|c| c.param)
            .or_else(|| record.get(ANAM).and_then(|s| u32_at(&s.data, 0)))
            .map(|id| name_of(order, global(id)))
            .unwrap_or_default();
        let quest = record
            .get(QSTI)
            .and_then(|s| u32_at(&s.data, 0))
            .map(|id| editor_id_of(order, global(id)))
            .unwrap_or_default();
        let (topic_edid, topic_text) = topic.clone().unwrap_or_default();
        let option = record
            .get(RNAM)
            .map(Subrecord::zstring)
            .filter(|s| !s.is_empty())
            .unwrap_or(topic_text);
        let answer: String = record
            .get(NAM1)
            .map(Subrecord::zstring)
            .unwrap_or_default()
            .chars()
            .take(ANSWER_MAX)
            .collect();
        let own = classify(&facts);
        let player_check = conditions.iter().any(is_player_check);
        read.push(Read {
            picked: topic.is_some(),
            follow_ups,
            line: Line {
                plugin: r.plugin.name.clone(),
                form: r.form_id.0,
                quest,
                speaker: clean(&speaker),
                topic: topic_edid,
                option: clean(&option),
                answer: clean(&answer),
                verdict: own.clone(),
                follow: Verdict::NoVote(Vec::new()),
                exception: String::new(),
                own,
                player_check,
            },
        });
    }
    // A later plugin's copy of a line replaces the earlier one.
    let by_form: HashMap<u32, usize> = read
        .iter()
        .enumerate()
        .map(|(i, l)| (l.line.form, i))
        .collect();
    let mut other = 0;
    for i in 0..read.len() {
        if !read[i].picked {
            other += 1;
            continue;
        }
        let follow = follow_verdicts(read[i].line.form, &by_form, &read);
        let line = &mut read[i].line;
        let merged = with_follow_ups(&line.own, &follow);
        // Form IDs in the rule's list are the base game's (its mod index 0 in the game too).
        let id = if line.plugin.eq_ignore_ascii_case("FalloutNV.esm") {
            line.form
        } else {
            0
        };
        line.verdict = settle(merged.clone(), &line.quest, id);
        if line.verdict != merged {
            line.exception = line.verdict.reasons();
        }
        line.follow = with_follow_ups(&Verdict::NoVote(Vec::new()), &follow);
    }
    let all = read
        .into_iter()
        .enumerate()
        .filter(|(i, r)| r.picked && by_form[&r.line.form] == *i)
        .map(|(_, r)| r.line)
        .collect();
    (all, other)
}

/// The lines voted on in `order` (form IDs in the load order): the game holds a click on one of
/// them for the players' votes (S42 step 3b).
pub fn vote_lines(order: &LoadOrder) -> HashSet<u32> {
    line_sets(order).vote
}

/// The lines of `order` the game treats apart (form IDs in the load order).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineSets {
    /// Voted on (S42 step 3b).
    pub vote: HashSet<u32>,
    /// Each player's own: not voted on, and the line itself opens a menu for the player who
    /// picks it (a trade, repairs, crafting, a companion's things). Another player's copy is
    /// never shown as an option (the owner, 2026-11-22: "Each person's dialogue option to open a
    /// trading window is exclusive to each player").
    pub personal: HashSet<u32>,
    /// A check of the player's S.P.E.C.I.A.L. or a skill (a condition on one, the success or the
    /// failure line; a speech challenge), or of anything else of the player that can differ
    /// between players (items or caps held, a perk, the level, karma): the only lines of another
    /// player shown as an option (the owner, 2026-11-23: "Only supposed to get the other
    /// person's lines IF they're SPECIAL related"; 2026-11-24: Chet's "Fine. Here's 1000 caps."
    /// the host could say, council 3-0, A).
    pub checks: HashSet<u32>,
}

/// [`LineSets`] of `order`, in one reading.
pub fn line_sets(order: &LoadOrder) -> LineSets {
    sets_of(lines(order).0)
}

fn sets_of(lines: Vec<Line>) -> LineSets {
    let mut sets = LineSets::default();
    for l in lines {
        let (Verdict::Vote(own) | Verdict::NoVote(own)) = &l.own;
        if l.player_check
            || own
                .iter()
                .any(|r| matches!(r, Reason::SkillCheck | Reason::SpeechCheck))
        {
            sets.checks.insert(l.form);
        }
        match (&l.verdict, &l.own) {
            (Verdict::Vote(_), _) => {
                sets.vote.insert(l.form);
            }
            (Verdict::NoVote(_), Verdict::NoVote(own))
                if own.iter().any(|r| matches!(r, Reason::OpensMenu(_))) =>
            {
                sets.personal.insert(l.form);
            }
            _ => {}
        }
    }
    sets
}

/// The verdicts of every line that follows `form` on its own, followed further (each line once).
fn follow_verdicts(form: u32, by_form: &HashMap<u32, usize>, all: &[Read]) -> Vec<Verdict> {
    let mut seen = HashSet::from([form]);
    let mut todo: Vec<u32> = all[by_form[&form]].follow_ups.clone();
    let mut out = Vec::new();
    while let Some(next) = todo.pop() {
        if !seen.insert(next) {
            continue;
        }
        if let Some(&i) = by_form.get(&next) {
            out.push(all[i].line.own.clone());
            todo.extend(all[i].follow_ups.iter().copied());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(form: u32, own: Verdict, verdict: Verdict) -> Line {
        Line {
            plugin: "FalloutNV.esm".into(),
            form,
            quest: String::new(),
            speaker: String::new(),
            topic: String::new(),
            option: String::new(),
            answer: String::new(),
            own,
            follow: Verdict::NoVote(Vec::new()),
            verdict,
            exception: String::new(),
            player_check: false,
        }
    }

    /// Trudy's "Show me what you have for sale." (`ShowBarterMenu`) is each player's own; a line
    /// that also moves a quest is voted on (its menu still the picker's); a plain line, and one
    /// whose menu is a later line's, are neither.
    #[test]
    fn a_line_that_opens_a_menu_itself_is_each_players_own() {
        let menu = || Reason::OpensMenu("showbartermenu".into());
        let stage = || Reason::ChangesQuest("setstage".into());
        let sets = sets_of(vec![
            line(
                1,
                Verdict::NoVote(vec![menu()]),
                Verdict::NoVote(vec![menu()]),
            ),
            line(
                2,
                Verdict::Vote(vec![stage()]),
                Verdict::Vote(vec![stage()]),
            ),
            line(3, Verdict::NoVote(vec![]), Verdict::NoVote(vec![])),
            line(4, Verdict::NoVote(vec![]), Verdict::NoVote(vec![menu()])),
        ]);
        assert_eq!(sets.personal, HashSet::from([1]));
        assert_eq!(sets.vote, HashSet::from([2]));
        assert!(sets.checks.is_empty());
    }

    /// Chet's "[Barter 25] The town's at stake." (its success and its failure line), a line
    /// asking for a S.P.E.C.I.A.L., a speech challenge: checks, whatever else they are; a plain
    /// line and Goodbye are not.
    #[test]
    fn a_line_that_checks_the_players_special_or_a_skill_is_a_check() {
        let check = || Reason::SkillCheck;
        let sets = sets_of(vec![
            line(
                1,
                Verdict::Vote(vec![check()]),
                Verdict::Vote(vec![check()]),
            ),
            line(
                2,
                Verdict::Vote(vec![check(), Reason::ChangesQuest("setstage".into())]),
                Verdict::NoVote(vec![Reason::OwnQuest("vms16".into())]),
            ),
            line(
                3,
                Verdict::Vote(vec![Reason::SpeechCheck]),
                Verdict::Vote(vec![Reason::SpeechCheck]),
            ),
            line(4, Verdict::NoVote(vec![]), Verdict::NoVote(vec![])),
            line(
                5,
                Verdict::Vote(vec![Reason::EndsConversation]),
                Verdict::Vote(vec![Reason::EndsConversation]),
            ),
        ]);
        assert_eq!(sets.checks, HashSet::from([1, 2, 3]));
    }
}
