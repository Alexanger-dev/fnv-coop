//! `NV_HARNESS=FILE` (a test aid for co-op's two-game harness): lines appended to FILE while the
//! game runs are commands, each run once, between two of main's frames, in order. Output goes to
//! the console log, each line starting `harness:`.
//!
//! - `con TEXT`: TEXT run as a console line, as the console runs one (`Script::CompileAndRun` with
//!   the console's compiler): `player.moveto SunnyREF`, `SunnyREF.activate player`, ...
//! - `load NAME`: the save called NAME loaded through the save manager's `LoadGame`
//!   (`00850760`), as the quick load does.
//! - `key HEX FRAMES`: the key with DirectInput code HEX held for FRAMES of main's frames.
//! - `mouse DX DY FRAMES [BUTTONS]`: the mouse moved by (DX, DY) on each of FRAMES frames, with the
//!   buttons of the bit mask BUTTONS (bit 0 the left one) held.
//! - `warp SPACEHEX X Y Z`: the player moved there as a door moves it (co-op's `live::warp`).
//! - `wait FRAMES`: the next commands wait that many frames.
//! - `pos`: the player's place and space. `actor HEX`: an actor's place, space, life and package.
//!   `quest HEX`: a quest's stage, done stages and objectives. `var HEX`: a quest's variables.
//! - `say TEXT`: TEXT printed (to mark a step in the log). `menu`: the dialogue's speaker, if open.

use std::io::Read as _;

use engine_core::prelude::*;

/// The save manager singleton and its `LoadGame` (name, device, statistics, quiet).
const SAVE_MANAGER: u32 = 0x011d_e134;
const SAVE_MANAGER_LOAD: u32 = 0x0085_0760;
/// `MenuConsole::GetSingleton(create)`; the console's compiler is its first word.
const CONSOLE_INSTANCE: u32 = 0x0071_b160;
/// `Script::Script`, `SetTemporary`, `SetText`, `CompileAndRun`, the destructor; its size.
const SCRIPT_CONSTRUCT: u32 = 0x005a_a0f0;
const SCRIPT_SET_TEMPORARY: u32 = 0x0048_4490;
const SCRIPT_SET_TEXT: u32 = 0x005a_be50;
const SCRIPT_COMPILE_AND_RUN: u32 = 0x005a_c400;
const SCRIPT_DESTRUCT: u32 = 0x005a_a1a0;
const SCRIPT_SIZE: u32 = 0x54;

/// The harness: the command file, how much of it was read, and what is held.
pub struct Harness {
    path: std::path::PathBuf,
    read: usize,
    pending: std::collections::VecDeque<String>,
    frame: u64,
    wait_until: u64,
    keys: Vec<(u8, u64)>,
    mouse: Vec<(i32, i32, u64, u8)>,
}

impl Harness {
    /// The harness when `NV_HARNESS` names a file (made empty if missing).
    pub fn from_env() -> Option<Self> {
        let path = std::path::PathBuf::from(std::env::var_os("NV_HARNESS")?);
        install();
        if !path.exists() {
            let _ = std::fs::write(&path, b"");
        }
        Some(Self {
            path,
            read: 0,
            pending: Default::default(),
            frame: 0,
            wait_until: 0,
            keys: Vec::new(),
            mouse: Vec::new(),
        })
    }

    /// The keys held for the next frame.
    pub fn keys(&self) -> Vec<u8> {
        self.keys
            .iter()
            .filter(|(_, until)| *until > self.frame)
            .map(|(k, _)| *k)
            .collect()
    }

    /// The mouse motion and buttons for the next frame.
    pub fn mouse(&self) -> ([i32; 3], [bool; 8]) {
        let mut motion = [0; 3];
        let mut buttons = [false; 8];
        for (dx, dy, until, b) in &self.mouse {
            if *until > self.frame {
                motion[0] += dx;
                motion[1] += dy;
                for (i, slot) in buttons.iter_mut().enumerate() {
                    *slot |= b & (1 << i) != 0;
                }
            }
        }
        (motion, buttons)
    }

    /// One step after one of main's frames: new lines read, due commands run.
    pub fn after_frame(&mut self, e: &mut Engine) -> Vec<String> {
        self.frame += 1;
        self.keys.retain(|(_, until)| *until > self.frame);
        self.mouse.retain(|m| m.2 > self.frame);
        let mut out = Vec::new();
        if let Ok(mut f) = std::fs::File::open(&self.path) {
            let mut text = String::new();
            if f.read_to_string(&mut text).is_ok() && text.len() > self.read {
                let new = &text[self.read..];
                // Only whole lines.
                if let Some(end) = new.rfind('\n') {
                    for line in new[..end].lines() {
                        let line = line.trim();
                        if !line.is_empty() {
                            self.pending.push_back(line.to_string());
                        }
                    }
                    self.read += end + 1;
                }
            }
        }
        while self.frame >= self.wait_until {
            let Some(line) = self.pending.pop_front() else {
                break;
            };
            out.push(format!("harness: [{}] {line}", self.frame));
            // Game calls run on the game's main thread, at the end of its next frame.
            let word = line.split(' ').next().unwrap_or("");
            if matches!(word, "key" | "mouse" | "wait" | "say") {
                out.extend(
                    self.run(e, &line)
                        .into_iter()
                        .map(|l| format!("harness: {l}")),
                );
            } else {
                lock(&TO_MAIN).push_back(line);
            }
        }
        out.extend(
            std::mem::take(&mut *lock(&FROM_MAIN))
                .into_iter()
                .map(|l| format!("harness: {l}")),
        );
        out
    }

    fn run(&mut self, e: &mut Engine, line: &str) -> Vec<String> {
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        let rest = rest.trim();
        let hex = |s: &str| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok();
        match word {
            "con" => vec![console(e, rest)],
            "load" => vec![load(e, rest)],
            "key" => {
                let mut p = rest.split_whitespace();
                match (
                    p.next().and_then(|k| u8::from_str_radix(k, 16).ok()),
                    p.next(),
                ) {
                    (Some(k), Some(n)) => {
                        let n: u64 = n.parse().unwrap_or(1);
                        self.keys.push((k, self.frame + n));
                        vec![]
                    }
                    _ => vec!["key HEX FRAMES".into()],
                }
            }
            "mouse" => {
                let v: Vec<i64> = rest
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                if v.len() < 3 {
                    return vec!["mouse DX DY FRAMES [BUTTONS]".into()];
                }
                self.mouse.push((
                    v[0] as i32,
                    v[1] as i32,
                    self.frame + v[2] as u64,
                    v.get(3).copied().unwrap_or(0) as u8,
                ));
                vec![]
            }
            "wait" => {
                self.wait_until = self.frame + rest.parse::<u64>().unwrap_or(0);
                vec![]
            }
            "warp" => {
                let p: Vec<&str> = rest.split_whitespace().collect();
                let v: Vec<f32> = p.iter().skip(1).filter_map(|s| s.parse().ok()).collect();
                match (p.first().and_then(|s| hex(s)), v.len()) {
                    (Some(space), 3) => vec![format!(
                        "warp: {:?}",
                        coop_engine::live::warp(e, space, [v[0], v[1], v[2]], 0.0)
                    )],
                    _ => vec!["warp SPACEHEX X Y Z".into()],
                }
            }
            // The open dialogue: `dnext` clicks as on the shown text (on to the next screen);
            // `dtopic N` clicks the list's Nth topic tile (0 the first); `dtopics` counts them.
            "dnext" | "dtopic" | "dtopics" => {
                let menu = e.mem.u32(coop_engine::addr::DIALOG_MENU_OPEN);
                if menu == 0 {
                    return vec!["no dialogue open".into()];
                }
                let mut tiles = Vec::new();
                let mut node = menu + 0x40 + 4;
                while node != 0 && tiles.len() < 32 {
                    let item = e.mem.u32(node);
                    if item != 0 {
                        tiles.push(e.mem.u32(item));
                    }
                    node = e.mem.u32(node + 4);
                }
                match word {
                    "dnext" => {
                        e.vcall(menu, 0xc, &args![0u32, 0u32]);
                        vec!["dnext: clicked".into()]
                    }
                    // With the line being said: `MenuTopicManager::pCurrentTopic` (+0x70, +0x00),
                    // a list node whose item is the `MenuTopic`, and its `pInfo` (+0x18). Each
                    // pointer is checked before it is read (a wrong one stops the game).
                    "dtopics" => {
                        let read = |e: &mut Engine, at: u32| {
                            if at != 0 && e.mem.is_mapped(at) && e.mem.is_mapped(at + 3) {
                                e.mem.u32(at)
                            } else {
                                0
                            }
                        };
                        let node = read(e, menu + 0x70);
                        let node = read(e, node);
                        let topic = read(e, node);
                        let info = read(e, topic.wrapping_add(0x18) * (topic != 0) as u32);
                        let id = read(
                            e,
                            info.wrapping_add(coop_engine::addr::FORM_ID_OFFSET) * (info != 0) as u32,
                        );
                        vec![format!("{} topics; saying info {id:08x}", tiles.len())]
                    }
                    _ => match rest.parse::<usize>().ok().and_then(|n| tiles.get(n)) {
                        Some(&tile) => {
                            e.vcall(menu, 0xc, &args![0xffff_ffffu32, tile]);
                            vec![format!("dtopic {rest}: clicked")]
                        }
                        None => vec![format!("dtopic {rest}: {} topics", tiles.len())],
                    },
                }
            }
            "ref" => {
                match hex(rest).and_then(|id| coop_engine::forms::form(e, id).map(|r| (id, r))) {
                    Some((id, r)) => {
                        let pos = coop_engine::fight::place_of_ptr(e, r);
                        let space = coop_engine::fight::space_of(e, r);
                        vec![format!(
                            "ref {id:08x}: at ({:.0}, {:.0}, {:.0}) in {space:08x}, disabled {:?}",
                            pos[0],
                            pos[1],
                            pos[2],
                            coop_engine::actors::disabled(e, id)
                        )]
                    }
                    None => vec![format!("ref {rest}: not here")],
                }
            }
            "global" => match hex(rest).and_then(|id| coop_engine::forms::global_value(e, id)) {
                Some(v) => vec![format!("global {rest}: {v}")],
                None => vec![format!("global {rest}: not a global")],
            },
            "input" => vec![format!(
                "input devices (keyboard, acquired, buffer, waiting): {:?}; controls {:?}",
                engine_platform::dinput::devices(e),
                coop_engine::fight::controls(e)
            )],
            // Game calls without the console: a quest's stage set as `SetStage` does (its result
            // runs), an actor's package evaluated as `evp` does, an actor killed by the player.
            "stage" => {
                let p: Vec<&str> = rest.split_whitespace().collect();
                match (
                    p.first().and_then(|s| hex(s)),
                    p.get(1).and_then(|s| s.parse().ok()),
                ) {
                    (Some(q), Some(n)) => vec![format!(
                        "stage {:08x} {n}: {}",
                        q,
                        coop_engine::forms::set_quest_stage(e, q, n)
                    )],
                    _ => vec!["stage QUESTHEX N".into()],
                }
            }
            "evp" => match hex(rest) {
                Some(id) => vec![format!(
                    "evp {id:08x}: {}",
                    coop_engine::commands::evaluate_package(e, id)
                )],
                None => vec!["evp HEX".into()],
            },
            "kill" => match hex(rest) {
                Some(id) => vec![format!(
                    "kill {id:08x}: {}",
                    coop_engine::fight::kill_by(e, id, coop_engine::actors::PLAYER_REF_ID)
                )],
                None => vec!["kill HEX".into()],
            },
            // The player activates a reference (talks to an actor), as the activate key does
            // (`TESObjectREFR::Activate`, `00573170`).
            "activate" => {
                match hex(rest).and_then(|id| coop_engine::forms::form(e, id).map(|r| (id, r))) {
                    Some((id, r)) => {
                        let player = e.mem.u32(coop_engine::addr::PLAYER_SINGLETON);
                        let ok = e
                            .call(0x0057_3170, &args![r, player, 0u32, 0u32, 1u32])
                            .u32()
                            & 0xff;
                        vec![format!("activate {id:08x}: {ok}")]
                    }
                    None => vec![format!("activate {rest}: not here")],
                }
            }
            // A dialogue line's result scripts run as the game runs them when the line is said
            // (`TESTopicInfo::RunResultScript`, begin then end), with SPEAKER as the speaker.
            "info" => {
                let p: Vec<u32> = rest.split_whitespace().filter_map(hex).collect();
                match (
                    p.first().and_then(|i| coop_engine::forms::form(e, *i)),
                    p.get(1).and_then(|a| coop_engine::actors::actor(e, *a)),
                ) {
                    (Some(info), Some(speaker)) => {
                        for end in [0u32, 1] {
                            e.call(
                                coop_engine::addr::INFO_RUN_RESULT,
                                &args![info, end, speaker],
                            );
                        }
                        vec![format!("info {rest}: ran")]
                    }
                    _ => vec!["info INFOHEX SPEAKERHEX".into()],
                }
            }
            // How many of ITEM the player (or ACTOR) carries (`Actor::GetItemCountinContainer`,
            // `00575610`).
            "count" => {
                let p: Vec<u32> = rest.split_whitespace().filter_map(hex).collect();
                let who = p
                    .get(1)
                    .copied()
                    .unwrap_or(coop_engine::actors::PLAYER_REF_ID);
                match (
                    p.first().and_then(|i| coop_engine::forms::form(e, *i)),
                    coop_engine::actors::actor(e, who),
                ) {
                    (Some(item), Some(a)) => {
                        let n = e.call(0x0057_5610, &args![a, item]).u32() as i32;
                        vec![format!("count {:08x} on {who:08x}: {n}", p[0])]
                    }
                    _ => vec!["count ITEMHEX [ACTORHEX]".into()],
                }
            }
            // Gives or takes N of ITEM from the player (or ACTOR) the way the crafting menu does
            // (`007284f0`): `00574fa0(ref, item, 0, n)` to add, `TESObjectREFR::RemoveItem`
            // (vtable +0x17c) to take.
            "additem" | "removeitem" => {
                let p: Vec<u32> = rest.split_whitespace().filter_map(hex).collect();
                let who = p
                    .get(2)
                    .copied()
                    .unwrap_or(coop_engine::actors::PLAYER_REF_ID);
                let n = rest
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse::<u32>().ok());
                match (
                    p.first().and_then(|i| coop_engine::forms::form(e, *i)),
                    n,
                    coop_engine::actors::actor(e, who),
                ) {
                    (Some(item), Some(n), Some(a)) => {
                        if word == "additem" {
                            e.call(0x0057_4fa0, &args![a, item, 0u32, n]);
                        } else {
                            e.vcall(
                                a,
                                0x17c,
                                &args![item, 0u32, n, 0u32, 0u32, 0u32, 0u32, 0u32, 1u32, 0u32],
                            );
                        }
                        let now = e.call(0x0057_5610, &args![a, item]).u32() as i32;
                        vec![format!("{word} {:08x} x{n} on {who:08x}: now {now}", p[0])]
                    }
                    _ => vec![format!("{word} ITEMHEX N [ACTORHEX]")],
                }
            }
            "say" => vec![rest.to_string()],
            "menu" => vec![match coop_engine::talk::menu_npc(e) {
                Some(npc) => format!("dialogue open with {npc:08x}"),
                None => "no dialogue open".into(),
            }],
            "pos" => vec![place(e, coop_engine::actors::PLAYER_REF_ID)],
            "actor" => match hex(rest) {
                Some(id) => vec![place(e, id)],
                None => vec!["actor HEX".into()],
            },
            "quest" => match hex(rest).and_then(|id| coop_engine::quest::record(e, id)) {
                Some(r) => vec![format!(
                    "quest {rest}: stage {} done {:?} objectives {:?} flags {:02x}",
                    r.stage, r.done, r.objectives, r.flags
                )],
                None => vec![format!("quest {rest}: not a quest")],
            },
            "var" => match hex(rest).and_then(|id| coop_engine::vars::quest_vars(e, id)) {
                Some(vs) => vec![format!(
                    "quest {rest} variables: {:?}",
                    vs.iter()
                        .map(|(i, b)| (*i, f64::from_bits(*b)))
                        .collect::<Vec<_>>()
                )],
                None => vec![format!("quest {rest}: no variables")],
            },
            // A quest variable set as `set QUEST.var to VALUE` stores it (the variable by its
            // index, in decimal): for a state the replay skipped (`bMetSunny`, set by a line of
            // the opening this replay does not play).
            "setvar" => {
                let p: Vec<&str> = rest.split_whitespace().collect();
                match (
                    p.first().and_then(|s| hex(s)),
                    p.get(1).and_then(|s| s.parse::<u32>().ok()),
                    p.get(2).and_then(|s| s.parse::<f64>().ok()),
                ) {
                    (Some(q), Some(var), Some(v)) => vec![format!(
                        "setvar {rest}: {}",
                        coop_engine::vars::set_quest_var(e, q, var, v.to_bits())
                    )],
                    _ => vec!["setvar QUESTHEX VARINDEX VALUE".into()],
                }
            }
            _ => vec![format!("unknown command {word}")],
        }
    }
}

static TO_MAIN: std::sync::Mutex<std::collections::VecDeque<String>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());
static FROM_MAIN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Installs the main thread's part (`engine_core::thread_host::set_end_frame_hook`).
pub fn install() {
    engine_core::thread_host::set_end_frame_hook(on_main);
}

/// On the game's main thread at the end of a frame: the queued game commands.
fn on_main(e: &mut Engine) {
    let lines: Vec<String> = lock(&TO_MAIN).drain(..).collect();
    for line in lines {
        let mut h = Harness {
            path: Default::default(),
            read: 0,
            pending: Default::default(),
            frame: 0,
            wait_until: 0,
            keys: Vec::new(),
            mouse: Vec::new(),
        };
        let out = h.run(e, &line);
        lock(&FROM_MAIN).extend(out);
    }
}

/// An actor's (or the player's) place, space, life and package, in one line.
fn place(e: &mut Engine, id: u32) -> String {
    let Some(a) = coop_engine::actors::actor(e, id) else {
        return format!("{id:08x}: not in this game");
    };
    let pos = coop_engine::fight::place_of_ptr(e, a);
    let space = coop_engine::fight::space_of(e, a);
    let dead = coop_engine::actors::dead(e, id);
    let package = coop_engine::commands::current_package(e, id);
    let ai = e.mem.u8(a + coop_engine::addr::ACTOR_PROCESS_ME_OFFSET);
    let level = if e.mem.u32(a + coop_engine::addr::ACTOR_PROCESS_OFFSET) != 0 {
        e.call(coop_engine::addr::MOBILE_PROCESS_TYPE, &args![a])
            .u32() as i32
    } else {
        -1
    };
    // `Actor::IsWeaponDrawn` (`008a16d0`).
    let drawn = if level >= 0 {
        e.call(0x008a_16d0, &args![a]).u32() & 0xff
    } else {
        0
    };
    format!(
        "{id:08x}: at ({:.0}, {:.0}, {:.0}) in {space:08x}, dead {dead:?}, package {package:08x?}, ai {ai}, process level {level}, weapon drawn {drawn}",
        pos[0], pos[1], pos[2]
    )
}

/// Runs `text` as a console line.
fn console(e: &mut Engine, text: &str) -> String {
    let console = e.call(CONSOLE_INSTANCE, &args![1u32]).u32();
    if console == 0 {
        return "con: no console".into();
    }
    let compiler = e.mem.u32(console);
    let len = text.len() as u32 + 1;
    e.with_stack(SCRIPT_SIZE + len + 0x10, |e, base| {
        let script = base.addr();
        let line = script + SCRIPT_SIZE + 8;
        for (i, b) in text.bytes().enumerate() {
            e.mem.set_u8(line + i as u32, b);
        }
        e.mem.set_u8(line + text.len() as u32, 0);
        e.call(SCRIPT_CONSTRUCT, &args![script]);
        e.call(SCRIPT_SET_TEMPORARY, &args![script]);
        e.call(SCRIPT_SET_TEXT, &args![script, line]);
        e.call(SCRIPT_COMPILE_AND_RUN, &args![script, compiler, 1u32, 0u32]);
        e.call(SCRIPT_DESTRUCT, &args![script]);
    });
    "con: ran".into()
}

/// Loads the save called `name` through the save manager.
fn load(e: &mut Engine, name: &str) -> String {
    let manager = e.mem.u32(SAVE_MANAGER);
    if manager == 0 {
        return "load: no save manager".into();
    }
    let len = name.len() as u32 + 1;
    let ok = e.with_stack(len + 0x10, |e, base| {
        let at = base.addr();
        for (i, b) in name.bytes().enumerate() {
            e.mem.set_u8(at + i as u32, b);
        }
        e.mem.set_u8(at + name.len() as u32, 0);
        e.call(SAVE_MANAGER_LOAD, &args![manager, at, u32::MAX, 0u32, 1u32])
            .bool()
    });
    format!("load {name}: {}", if ok { "loaded" } else { "failed" })
}
