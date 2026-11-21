//! `coop-votelist OUT.tsv PLUGIN...`: every line a player can pick in the given plugins (masters
//! first), with the co-op vote verdict of `coop_core::dialogue::votes` and its reasons, as a
//! tab-separated table; a summary on standard output.
//!
//! A line here is a topic info of a topic the player can pick (a `DIAL` of type Topic that is
//! top-level, or is a choice (`TCLT`) or an added topic (`NAME`) of some line, other than the
//! greeting): the option the player picks and the NPC's answer. The facts the rule reads come
//! from the info's own data: the goodbye and speech challenge marks (`DATA`), a condition on one
//! of the player's skills or S.P.E.C.I.A.L. (`CTDA`, `GetActorValue` on the target or the
//! player, any comparison: the failure line of a check is a check too), and the source text of
//! its result scripts (`SCTX`). The lines that follow a line on their own (`TCFU`, followed
//! further) count as that line's (`with_follow_ups`): Sunny's "[END TUTORIAL]" does nothing
//! itself, the line she says after it ends the tutorial.
//! Last, `settle`: the owner's exceptions (quests each player has on their own, named lines).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use coop_core::dialogue::votes::{classify, settle, with_follow_ups, LineFacts, Verdict};
use esm::{sig, FormId, FourCC, Plugin, RecordEntry, Subrecord};

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

struct Loaded {
    name: String,
    plugin: Plugin,
}

struct Game {
    plugins: Vec<Loaded>,
}

impl Game {
    /// The plugin and the record a form ID of plugin `at` names, looked up in the plugin that
    /// defines it.
    fn lookup(&self, at: usize, id: FormId) -> Option<(usize, &RecordEntry)> {
        let masters = &self.plugins[at].plugin.header().masters;
        let index = usize::from(id.mod_index());
        let (target, local) = if index < masters.len() {
            let target = self
                .plugins
                .iter()
                .position(|p| p.name.eq_ignore_ascii_case(&masters[index]))?;
            let own = self.plugins[target].plugin.header().masters.len() as u32;
            (target, FormId((id.0 & 0x00ff_ffff) | (own << 24)))
        } else {
            (at, id)
        };
        self.plugins[target]
            .plugin
            .get(local)
            .map(|entry| (target, entry))
    }

    /// The name a form ID shows (its full name, else its editor ID), or its number.
    fn name(&self, at: usize, id: FormId) -> String {
        if let Some((target, entry)) = self.lookup(at, id) {
            if let Ok(record) = self.plugins[target].plugin.record(entry) {
                if let Some(name) = record.full_name().filter(|n| !n.is_empty()) {
                    return name;
                }
                if let Some(name) = record.editor_id() {
                    return name;
                }
            }
        }
        format!("{:08X}", id.0)
    }

    fn editor_id(&self, at: usize, id: FormId) -> String {
        self.lookup(at, id)
            .and_then(|(target, entry)| self.plugins[target].plugin.record(entry).ok())
            .and_then(|r| r.editor_id())
            .unwrap_or_else(|| format!("{:08X}", id.0))
    }
}

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

/// A record anywhere in the game: the plugin that defines it and its form ID there.
type Key = (usize, FormId);

struct Row {
    plugin: String,
    form: String,
    quest: String,
    speaker: String,
    topic: String,
    option: String,
    answer: String,
    own: Verdict,
    follow: Verdict,
    verdict: Verdict,
    /// The owner's exception that decided the verdict, if one did.
    exception: String,
}

fn clean(text: &str) -> String {
    text.replace(['\t', '\r', '\n'], " ").trim().to_string()
}

/// One line as read, before its follow-ups are known.
struct Line {
    key: Key,
    topic: Key,
    row: Row,
    follow_ups: Vec<Key>,
}

/// The topics a player can pick: the choices and added topics of every line.
fn picked_topics(game: &Game) -> HashSet<Key> {
    let mut out = HashSet::new();
    for at in 0..game.plugins.len() {
        let plugin = &game.plugins[at].plugin;
        for entry in plugin.records_of_type(sig::INFO) {
            let Ok(record) = plugin.record(entry) else {
                continue;
            };
            for sub in record.get_all(TCLT).chain(record.get_all(sig::NAME)) {
                if let Some(id) = u32_at(&sub.data, 0) {
                    if let Some((target, topic)) = game.lookup(at, FormId(id)) {
                        out.insert((target, topic.header.form_id));
                    }
                }
            }
        }
    }
    out
}

fn lines(game: &Game, at: usize, picked: &HashSet<Key>) -> Vec<Line> {
    let loaded = &game.plugins[at];
    let mut topics: HashMap<FormId, Option<(Key, String, String)>> = HashMap::new();
    let mut out = Vec::new();
    for entry in loaded.plugin.records_of_type(sig::INFO) {
        if entry.header.is_deleted() {
            continue;
        }
        let Some(topic_id) = entry.topic else {
            continue;
        };
        // The topic: one the player can pick, its editor ID and its text.
        let topic = topics
            .entry(topic_id)
            .or_insert_with(|| {
                let (target, dial) = game.lookup(at, topic_id)?;
                let key = (target, dial.header.form_id);
                let record = game.plugins[target].plugin.record(dial).ok()?;
                let data = &record.get(sig::DATA)?.data;
                let top_level = data.get(1).is_some_and(|f| f & DIAL_TOP_LEVEL != 0);
                let edid = record.editor_id().unwrap_or_default();
                (data.first() == Some(&DIAL_TOPIC)
                    && (top_level || picked.contains(&key))
                    && !edid.eq_ignore_ascii_case("GREETING"))
                .then(|| (key, edid, record.full_name().unwrap_or_default()))
            })
            .clone();
        let Ok(record) = loaded.plugin.record(entry) else {
            continue;
        };
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
            .filter_map(|id| game.lookup(at, FormId(id)))
            .map(|(target, info)| (target, info.header.form_id))
            .collect();
        let speaker = conditions
            .iter()
            .find(|c| c.function == FN_GET_IS_ID && c.run_on == RUN_ON_SUBJECT && c.op == 0)
            .map(|c| FormId(c.param))
            .or_else(|| {
                record
                    .get(ANAM)
                    .and_then(|s| u32_at(&s.data, 0))
                    .map(FormId)
            })
            .map(|id| game.name(at, id))
            .unwrap_or_default();
        let quest = record
            .get(QSTI)
            .and_then(|s| u32_at(&s.data, 0))
            .map(|id| game.editor_id(at, FormId(id)))
            .unwrap_or_default();
        let (topic_key, topic_edid, topic_text) = match topic {
            Some(t) => t,
            // Not a topic the player picks: kept only as a follow-up of other lines.
            None => ((usize::MAX, topic_id), String::new(), String::new()),
        };
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
        // The key of the plugin that defines the line, so a later plugin's copy replaces it.
        let key = game
            .lookup(at, entry.header.form_id)
            .map_or((at, entry.header.form_id), |(t, e)| (t, e.header.form_id));
        out.push(Line {
            key,
            topic: topic_key,
            follow_ups,
            row: Row {
                plugin: loaded.name.clone(),
                form: format!("{:08X}", entry.header.form_id.0),
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
    out
}

/// The verdicts of every line that follows `key` on its own, followed further (each line once).
fn follow_verdicts(key: Key, by_key: &HashMap<Key, usize>, all: &[Line]) -> Vec<Verdict> {
    let mut seen = HashSet::from([key]);
    let mut todo: Vec<Key> = all[by_key[&key]].follow_ups.clone();
    let mut out = Vec::new();
    while let Some(next) = todo.pop() {
        if !seen.insert(next) {
            continue;
        }
        if let Some(&i) = by_key.get(&next) {
            out.push(all[i].row.own.clone());
            todo.extend(all[i].follow_ups.iter().copied());
        }
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: coop-votelist OUT.tsv PLUGIN... (masters first)");
        std::process::exit(2);
    }
    let mut game = Game {
        plugins: Vec::new(),
    };
    let first = if args[0] == "--dump" { 2 } else { 1 };
    for path in &args[first..] {
        let plugin = Plugin::open(path).unwrap_or_else(|e| {
            eprintln!("{path}: {e}");
            std::process::exit(1);
        });
        let name = Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone());
        game.plugins.push(Loaded { name, plugin });
    }
    // `coop-votelist --dump FORMID PLUGIN...`: a record's subrecords, to check what it holds.
    if args[0] == "--dump" {
        let id = u32::from_str_radix(&args[1], 16).unwrap_or(0);
        for loaded in &game.plugins {
            let Some(entry) = loaded.plugin.get(FormId(id)) else {
                continue;
            };
            println!("{} {:08X} topic {:?}", loaded.name, id, entry.topic);
            for sub in loaded.plugin.subrecords(entry).unwrap_or_default() {
                let kind = String::from_utf8_lossy(sub.kind.as_bytes()).into_owned();
                let text: String = sub
                    .data
                    .iter()
                    .map(|&b| {
                        if (32..127).contains(&b) {
                            b as char
                        } else {
                            '.'
                        }
                    })
                    .take(120)
                    .collect();
                println!(
                    "  {kind} {:02X?} {text}",
                    &sub.data[..sub.data.len().min(28)]
                );
            }
        }
        return;
    }
    let picked = picked_topics(&game);
    let mut read = Vec::new();
    for at in 0..game.plugins.len() {
        read.extend(lines(&game, at, &picked));
    }
    // A later plugin's copy of a line replaces the earlier one.
    let by_key: HashMap<Key, usize> = read.iter().enumerate().map(|(i, l)| (l.key, i)).collect();
    let mut all = Vec::new();
    let mut other = 0;
    for i in 0..read.len() {
        if read[i].topic.0 == usize::MAX {
            other += 1;
            continue;
        }
        let follow = follow_verdicts(read[i].key, &by_key, &read);
        let row = &mut read[i].row;
        // Form IDs in the rule's list are the base game's (its mod index 0 in the game too).
        let base = row.plugin.eq_ignore_ascii_case("FalloutNV.esm");
        let id = if base {
            u32::from_str_radix(&row.form, 16).unwrap_or(0)
        } else {
            0
        };
        let merged = with_follow_ups(&row.own, &follow);
        row.verdict = settle(merged.clone(), &row.quest, id);
        if row.verdict != merged {
            row.exception = row.verdict.reasons();
        }
        row.follow = with_follow_ups(&Verdict::NoVote(Vec::new()), &follow);
    }
    for (i, line) in read.into_iter().enumerate() {
        if line.topic.0 != usize::MAX && by_key[&line.key] == i {
            all.push(line.row);
        }
    }
    all.sort_by(|a, b| {
        (&a.plugin, &a.quest, &a.speaker, &a.topic, &a.form)
            .cmp(&(&b.plugin, &b.quest, &b.speaker, &b.topic, &b.form))
    });
    let mut text = String::from(
        "Plugin\tLine ID\tQuest\tSpeaker\tTopic\tOption\tNPC answer\tVerdict\tReasons (this line)\tReasons (the lines that follow it)\tException\n",
    );
    let mut counts: BTreeMap<(String, &'static str), usize> = BTreeMap::new();
    let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
    for r in &all {
        let _ = writeln!(
            text,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            r.plugin,
            r.form,
            r.quest,
            r.speaker,
            r.topic,
            r.option,
            r.answer,
            r.verdict.name(),
            r.own.reasons(),
            r.follow.reasons(),
            r.exception
        );
        *counts
            .entry((r.plugin.clone(), r.verdict.name()))
            .or_default() += 1;
        let (Verdict::Vote(list) | Verdict::NoVote(list)) = &r.verdict;
        for reason in list {
            let kind = reason.text();
            let kind = kind.split(" (").next().unwrap_or(&kind);
            *reasons
                .entry(format!("{}: {kind}", r.verdict.name()))
                .or_default() += 1;
        }
    }
    std::fs::write(&args[0], text).unwrap_or_else(|e| {
        eprintln!("{}: {e}", args[0]);
        std::process::exit(1);
    });
    println!(
        "{} lines a player picks ({other} other lines: greetings, remarks, follow-ups)",
        all.len()
    );
    for ((plugin, verdict), n) in &counts {
        println!("{plugin}\t{verdict}\t{n}");
    }
    for (reason, n) in &reasons {
        println!("reason\t{reason}\t{n}");
    }
}
