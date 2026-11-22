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

use coop_core::dialogue::votes::{classify, settle, with_follow_ups, LineFacts, Verdict};
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
    lines(order)
        .0
        .into_iter()
        .filter(|l| matches!(l.verdict, Verdict::Vote(_)))
        .map(|l| l.form)
        .collect()
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
