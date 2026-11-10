//! What a story script tells an actor to do, as distinct from state (docs/coop/SPECS.md S13,
//! docs/coop/DECISIONS.md D5: host commands). The first: `evp`, an actor told to evaluate its
//! package again. A stage's result script says `SunnyREF.evp` so she acts on the new stage at
//! once; that script runs on the host only, and the guest's copy of her would stand still.

use std::sync::{Mutex, PoisonError};

use engine_core::extend::{Owner, WrapSlot};
use engine_core::prelude::*;

use crate::{actors, addr, gate, journal};

pub(crate) const EVALUATE: WrapSlot = WrapSlot::new(Owner::Coop, addr::ACTOR_EVALUATE_PACKAGE);

/// The commands noted since the last drain (game-wide, as the journal).
#[derive(Debug, Default)]
pub struct Commands {
    /// Reference ids of the actors a script told to evaluate their package.
    evaluate: Mutex<Vec<u32>>,
}

/// The wrap at `008a6ce0`: the game's call runs as it is; on a host whose journal is armed, an
/// evaluation asked for while a script runs, of an actor both games know, is noted.
pub(crate) fn evaluate(e: &mut Engine, a: &[u32]) -> Ret {
    let actor = a.first().copied().unwrap_or(0);
    if actor != 0 && journal::armed(e) && !gate::is_guest(e) && gate::cause(e).is_some() {
        let id = e.mem.u32(actor.wrapping_add(addr::FORM_ID_OFFSET));
        if actors::shareable(id) {
            let c = e.ext_free::<Commands>();
            let mut list = c.evaluate.lock().unwrap_or_else(PoisonError::into_inner);
            if !list.contains(&id) {
                list.push(id);
            }
        }
    }
    e.call_next(addr::ACTOR_EVALUATE_PACKAGE, a)
}

/// Takes the actors scripts told to evaluate their package since the last call.
pub fn drain_evaluations(e: &mut Engine) -> Vec<u32> {
    let c = e.ext_free::<Commands>();
    std::mem::take(&mut *c.evaluate.lock().unwrap_or_else(PoisonError::into_inner))
}

/// Tells actor `id` to evaluate its package again, as a script's `evp` does (buffered; AI reset).
/// False when the actor is not in this game now.
pub fn evaluate_package(e: &mut Engine, id: u32) -> bool {
    let Some(a) = actors::actor(e, id) else {
        return false;
    };
    e.call(addr::ACTOR_EVALUATE_PACKAGE, &args![a, 0u32, 1u32]);
    true
}
