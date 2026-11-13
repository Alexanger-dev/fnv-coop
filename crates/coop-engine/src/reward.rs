//! Rewards the shared story gives (docs/coop/SPECS.md S25). The story's scripts run in the host's
//! game only, and there `Player` is the host's player: a `RewardXP` in a quest's stage or in a
//! line's result gives that player alone the experience. The rules give every player the full
//! reward once (`reward: everyone`, every row of the bundled table): the host's game notes each
//! such reward ([`take`]), and each guest's game gives its own player the same ([`give_xp`]).
//!
//! A reward counts as the story's when the script running is a quest's own or has an owner quest
//! (a stage's result, a line's result, a quest's script), unless that quest is one each player
//! plays for themself ([`crate::gate::set_own_quests`]): its rewards are that player's. An object
//! script with no quest runs in every game that has the object, so each game gives its own.

use engine_core::extend::{Owner, WrapSlot};
use engine_core::prelude::*;
use std::sync::{Mutex, PoisonError};

use crate::{addr, gate};

/// The wrap at `0094fd10`, `PlayerCharacter::RewardExperience`.
pub(crate) const EXPERIENCE: WrapSlot = WrapSlot::new(Owner::Coop, addr::PLAYER_REWARD_EXPERIENCE);

/// One reward the story gave in the host's game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Given {
    /// The quest whose script gave it.
    pub quest: u32,
    /// Experience points.
    pub xp: u32,
}

#[derive(Default)]
struct Rewards {
    given: Mutex<Vec<Given>>,
}

/// Host: the story's rewards given since the last call, in order.
pub fn take(e: &mut Engine) -> Vec<Given> {
    std::mem::take(
        &mut *e
            .ext_free::<Rewards>()
            .given
            .lock()
            .unwrap_or_else(PoisonError::into_inner),
    )
}

/// Guest: gives this game's player `xp` experience points the game's own way (the message
/// included), as `RewardXP` does. False when there is no player yet.
pub fn give_xp(e: &mut Engine, xp: u32) -> bool {
    let player = e.mem.u32(addr::PLAYER_SINGLETON);
    if player == 0 {
        return false;
    }
    e.call(addr::PLAYER_REWARD_EXPERIENCE, &args![player, xp]);
    true
}

/// `PlayerCharacter::RewardExperience` (player, amount): on the host, while a story script runs
/// (see the module), the reward is noted for the guests. It is given here as ever.
pub(crate) fn experience(e: &mut Engine, a: &[u32]) -> Ret {
    let xp = a.get(1).copied().unwrap_or(0);
    if xp != 0 && !gate::is_guest(e) {
        if let Some(c) = gate::cause(e) {
            // A line's result script names no owner quest: its line's quest is the one.
            let quest = match c.owner_quest {
                0 => crate::talk::result_quest(e).unwrap_or(0),
                q => q,
            };
            if quest != 0 && !gate::is_own_quest(e, quest) {
                e.ext_free::<Rewards>()
                    .given
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(Given { quest, xp });
            }
        }
    }
    e.call_next(addr::PLAYER_REWARD_EXPERIENCE, a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{Story, StorySpec};

    const MQ: u32 = 0x0010_4c0c;
    const OPENING: u32 = 0x0010_4c1c;

    fn story() -> Story {
        let mut s = Story::new(&StorySpec {
            quests: vec![(MQ, vec![10]), (OPENING, vec![10])],
            ..StorySpec::default()
        });
        crate::arm(&mut s.e, true);
        gate::set_own_quests(&mut s.e, [OPENING]);
        s
    }

    /// Harness, 2026-11-13: Trudy's line that ends Sunny's last tutorial (`RewardXP 50`), said
    /// to the guest, ran in the host's game, whose player alone got the 50. On the host, the
    /// story's experience is given here as ever and noted for the guests.
    #[test]
    fn the_storys_experience_on_the_host_is_noted_for_the_guests() {
        let mut s = story();
        s.run_script_rewarding_xp(MQ, 50);
        assert_eq!(s.xp(), 50);
        assert_eq!(take(&mut s.e), vec![Given { quest: MQ, xp: 50 }]);
        assert_eq!(take(&mut s.e), vec![]);
    }

    /// Not noted: a reward of a quest each player plays for themself, of an object script with no
    /// quest (every game that has the object runs it), one given while the host's state is applied,
    /// and any on a guest. Each is given in its own game.
    #[test]
    fn other_experience_is_its_own_games() {
        let mut s = story();
        s.run_script_rewarding_xp(OPENING, 10);
        s.run_script_rewarding_xp(0, 20);
        gate::applying(&mut s.e, |e| {
            let player = e.mem.u32(addr::PLAYER_SINGLETON);
            e.call(addr::PLAYER_REWARD_EXPERIENCE, &args![player, 5u32]);
        });
        assert_eq!(s.xp(), 35);
        assert_eq!(take(&mut s.e), vec![]);
        // On a guest, a story script the rules let it run itself (it acts on its own player).
        gate::set_guest(&mut s.e, true);
        gate::set_own_scripts(&mut s.e, [0x0015_0aaa]);
        s.run_named_script_rewarding_xp(0x0015_0aaa, MQ, 40);
        assert_eq!(s.xp(), 75);
        assert_eq!(take(&mut s.e), vec![]);
    }

    /// A guest's game gives its player what the host's noted, the game's own way.
    #[test]
    fn a_guest_gives_its_player_the_experience() {
        let mut s = story();
        gate::set_guest(&mut s.e, true);
        assert!(give_xp(&mut s.e, 50));
        assert_eq!(s.xp(), 50);
        assert_eq!(take(&mut s.e), vec![]);
    }
}
