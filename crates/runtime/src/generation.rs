use std::sync::Arc;

use conversation_protocol::{GenerationId, TurnId};
use tokio::sync::{Mutex, OwnedMutexGuard};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GenerationIdentity {
    turn_id: TurnId,
    generation_id: GenerationId,
}

#[derive(Clone, Default)]
pub(crate) struct GenerationGuard {
    active: Arc<Mutex<Option<GenerationIdentity>>>,
}

impl GenerationGuard {
    pub(crate) async fn activate(&self, turn_id: TurnId, generation_id: GenerationId) -> bool {
        let mut active = self.active.lock().await;
        if active.is_some() {
            return false;
        }

        *active = Some(GenerationIdentity {
            turn_id,
            generation_id,
        });
        true
    }

    pub(crate) async fn permit(
        &self,
        turn_id: TurnId,
        generation_id: GenerationId,
    ) -> Option<GenerationPermit> {
        let guard = Arc::clone(&self.active).lock_owned().await;
        let expected = GenerationIdentity {
            turn_id,
            generation_id,
        };
        (guard.as_ref() == Some(&expected)).then_some(GenerationPermit { _guard: guard })
    }

    pub(crate) async fn deactivate(&self, turn_id: TurnId, generation_id: GenerationId) {
        let mut active = self.active.lock().await;
        if active.as_ref()
            == Some(&GenerationIdentity {
                turn_id,
                generation_id,
            })
        {
            *active = None;
        }
    }
}

pub(crate) struct GenerationPermit {
    _guard: OwnedMutexGuard<Option<GenerationIdentity>>,
}
