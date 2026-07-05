//! #569 — per-turn consent-doorbell routing.
//!
//! [`AgentEgressPolicy`](super::policy::AgentEgressPolicy) is a single
//! process-global chokepoint shared by every `EgressClient` in the process
//! (the B1 design). Its consent doorbell is therefore a single slot: each ACP
//! session's bootstrap rebinds that *same* slot (`set_doorbell`), so with
//! concurrent ACP sessions whichever one bootstrapped last silently owns
//! every subsequent `Ask` prompt — including one raised by a DIFFERENT
//! session's in-flight turn (#569).
//!
//! [`with_doorbell`] lets the code driving one session's turn declare "`Ask`s
//! resolved on this task belong to me" for the turn's lifetime, via a
//! `tokio::task_local`. `AgentEgressPolicy`'s `resolve_ask` consults
//! [`current`] first and prefers it over the shared slot, so overlapping
//! turns route their prompts to the right session's approval bridge. A task
//! that never calls `with_doorbell` (a detached sub-agent task, or the
//! single-session TUI/stdin flow, neither of which has a concept of
//! concurrent turns) falls through to the shared doorbell — unchanged,
//! pre-#569 behavior.

use std::future::Future;
use std::sync::Arc;

use super::consent::ConsentDoorbell;

tokio::task_local! {
    static CURRENT: Arc<dyn ConsentDoorbell>;
}

/// Run `fut` with `doorbell` bound as the current task's consent surface.
/// `None` (no interactive surface for this session) runs `fut` unscoped, so
/// `resolve_ask` falls back to the shared doorbell, if any.
pub async fn with_doorbell<F: Future>(
    doorbell: Option<Arc<dyn ConsentDoorbell>>,
    fut: F,
) -> F::Output {
    match doorbell {
        Some(doorbell) => CURRENT.scope(doorbell, fut).await,
        None => fut.await,
    }
}

/// The doorbell bound on the current task by [`with_doorbell`], if any.
pub(crate) fn current() -> Option<Arc<dyn ConsentDoorbell>> {
    CURRENT.try_with(|d| d.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::consent::ConsentDecision;

    struct Stub;

    #[async_trait::async_trait]
    impl ConsentDoorbell for Stub {
        async fn ask(&self, _host: &str, _registrable: &str, _reason: &str) -> ConsentDecision {
            ConsentDecision::Once
        }
    }

    #[tokio::test]
    async fn current_is_none_outside_any_scope() {
        assert!(current().is_none());
    }

    #[tokio::test]
    async fn with_doorbell_binds_current_for_the_duration_only() {
        let doorbell: Arc<dyn ConsentDoorbell> = Arc::new(Stub);
        with_doorbell(Some(doorbell), async {
            assert!(current().is_some(), "must be bound inside the scope");
        })
        .await;
        assert!(current().is_none(), "must not leak past the scope");
    }

    #[tokio::test]
    async fn none_runs_unscoped() {
        with_doorbell(None, async {
            assert!(current().is_none());
        })
        .await;
    }
}
