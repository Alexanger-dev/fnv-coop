//! `coop-votelist OUT.tsv PLUGIN...`: every line a player can pick in the given plugins (masters
//! first, as the game loads them), with the co-op vote verdict and its reasons
//! (`coop_votelist::lines`), as a tab-separated table; a summary on standard output.
//!
//! `coop-votelist --dump FORMID PLUGIN...`: a record's subrecords (its form ID as the plugin
//! stores it), to check what it holds.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use coop_core::dialogue::votes::Verdict;
use esm::{FormId, LoadOrder, Plugin};

fn dump(plugins: &[(String, Option<PathBuf>, Plugin)], id: u32) {
    for (name, _, plugin) in plugins {
        let Some(entry) = plugin.get(FormId(id)) else {
            continue;
        };
        println!("{name} {id:08X} topic {:?}", entry.topic);
        for sub in plugin.subrecords(entry).unwrap_or_default() {
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
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: coop-votelist OUT.tsv PLUGIN... (masters first)");
        std::process::exit(2);
    }
    let first = if args[0] == "--dump" { 2 } else { 1 };
    let mut plugins = Vec::new();
    for path in &args[first..] {
        let plugin = Plugin::open(path).unwrap_or_else(|e| {
            eprintln!("{path}: {e}");
            std::process::exit(1);
        });
        let name = Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone());
        plugins.push((name, Some(PathBuf::from(path)), plugin));
    }
    if args[0] == "--dump" {
        dump(&plugins, u32::from_str_radix(&args[1], 16).unwrap_or(0));
        return;
    }
    let order = LoadOrder::from_plugins(plugins).unwrap_or_else(|e| {
        eprintln!("load order: {e}");
        std::process::exit(1);
    });
    let (mut all, other) = coop_votelist::lines(&order);
    all.sort_by(|a, b| {
        (&a.plugin, &a.quest, &a.speaker, &a.topic, a.form)
            .cmp(&(&b.plugin, &b.quest, &b.speaker, &b.topic, b.form))
    });
    let mut text = String::from(
        "Plugin\tLine ID\tQuest\tSpeaker\tTopic\tOption\tNPC answer\tVerdict\tReasons (this line)\tReasons (the lines that follow it)\tException\n",
    );
    let mut counts: BTreeMap<(String, &'static str), usize> = BTreeMap::new();
    let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
    for r in &all {
        let _ = writeln!(
            text,
            "{}\t{:08X}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
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
