//! Instance liveness as a pure fold: `(state, event, ttl) -> state`. No clock, no I/O — the
//! caller passes the instant in the event, so the TTL behaviour is a function of its inputs.
//!
//! ```text
//! Registered{at}            -> seen at `at`, healthy, not expired
//! Heartbeat{at, healthy}    -> seen at `at`, reports `healthy`, not expired
//! Expired{now}              -> expired iff never seen, or now - last_seen >= ttl
//! ```
//!
//! An instance is *healthy* only if it has been seen, its last heartbeat reported healthy, and it
//! has not expired. `list()` folds an `Expired{now}` over each instance at read time, so there is
//! no sweeper task and no stored "expired" flag that could go stale.
//!
//! **Why not `fleet-lease`** (Tasmanian-Cloud/saga, crate `fleet-lease`): it resolves and builds
//! as a git dependency, but its `Lease::deadline_tick` is assigned exactly once, in `Requested`
//! (`state.now + spec.ttl_secs`), and a repeated `Requested` on a live lease is a no-op. No event
//! renews a deadline, and there is no `Expire` event — expiry is derived inside `Tick`. A
//! heartbeat therefore cannot be expressed as a lease event without minting a new `job_id` per
//! heartbeat. It is also `AGPL-3.0-or-later`, which this repo's `deny.toml` does not allow. The
//! event vocabulary below (`Expired{now}` ~ `Tick`) is kept close to it so adoption is mechanical
//! once fleet-lease grows a renewal event.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Liveness {
    /// Unix seconds of the last sign of life. `None` = not seen by this process yet (e.g. just
    /// replayed from the log after a restart — heartbeats are deliberately not logged).
    pub last_seen: Option<i64>,
    /// What the last heartbeat reported.
    pub reported_healthy: bool,
    pub expired: bool,
}

impl Liveness {
    /// An instance known from the log but not yet heard from by this process.
    pub const UNSEEN: Liveness = Liveness {
        last_seen: None,
        reported_healthy: false,
        expired: true,
    };

    pub fn is_healthy(&self) -> bool {
        self.last_seen.is_some() && self.reported_healthy && !self.expired
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LivenessEvent {
    Registered { at: i64 },
    Heartbeat { at: i64, healthy: bool },
    Expired { now: i64 },
}

/// The real fold. `ttl_secs`: an instance last seen at `t` is live while `now - t < ttl_secs`.
pub fn fold(state: Liveness, event: LivenessEvent, ttl_secs: i64) -> Liveness {
    match event {
        LivenessEvent::Registered { at } => Liveness {
            last_seen: Some(at),
            reported_healthy: true,
            expired: false,
        },
        LivenessEvent::Heartbeat { at, healthy } => Liveness {
            last_seen: Some(at),
            reported_healthy: healthy,
            expired: false,
        },
        LivenessEvent::Expired { now } => {
            let stale = match state.last_seen {
                None => true,
                Some(t) => now.saturating_sub(t) >= ttl_secs,
            };
            Liveness {
                expired: state.expired || stale,
                ..state
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: i64 = 30;

    #[test]
    fn a_registered_instance_is_healthy_until_the_ttl_elapses() {
        let s = fold(Liveness::UNSEEN, LivenessEvent::Registered { at: 100 }, TTL);
        assert!(s.is_healthy());
        let at_29 = fold(s, LivenessEvent::Expired { now: 129 }, TTL);
        assert!(
            at_29.is_healthy(),
            "one second inside the TTL is still live"
        );
        let at_30 = fold(s, LivenessEvent::Expired { now: 130 }, TTL);
        assert!(!at_30.is_healthy(), "now - last_seen == ttl is expired");
    }

    #[test]
    fn a_heartbeat_revives_an_expired_instance() {
        let s = fold(Liveness::UNSEEN, LivenessEvent::Registered { at: 0 }, TTL);
        let s = fold(s, LivenessEvent::Expired { now: 1000 }, TTL);
        assert!(!s.is_healthy());
        let s = fold(
            s,
            LivenessEvent::Heartbeat {
                at: 1000,
                healthy: true,
            },
            TTL,
        );
        assert!(s.is_healthy());
    }

    #[test]
    fn an_unhealthy_heartbeat_is_not_healthy_even_inside_the_ttl() {
        let s = fold(Liveness::UNSEEN, LivenessEvent::Registered { at: 0 }, TTL);
        let s = fold(
            s,
            LivenessEvent::Heartbeat {
                at: 5,
                healthy: false,
            },
            TTL,
        );
        let s = fold(s, LivenessEvent::Expired { now: 6 }, TTL);
        assert!(!s.is_healthy());
    }

    #[test]
    fn an_unseen_instance_is_never_healthy() {
        assert!(!Liveness::UNSEEN.is_healthy());
        let s = fold(Liveness::UNSEEN, LivenessEvent::Expired { now: 0 }, TTL);
        assert!(!s.is_healthy());
    }

    #[test]
    fn expiry_is_a_function_of_its_inputs_only() {
        let a = fold(Liveness::UNSEEN, LivenessEvent::Registered { at: 7 }, TTL);
        let b = fold(Liveness::UNSEEN, LivenessEvent::Registered { at: 7 }, TTL);
        assert_eq!(
            fold(a, LivenessEvent::Expired { now: 50 }, TTL),
            fold(b, LivenessEvent::Expired { now: 50 }, TTL)
        );
    }

    #[test]
    fn the_ttl_is_configurable() {
        let s = fold(Liveness::UNSEEN, LivenessEvent::Registered { at: 0 }, 5);
        assert!(!fold(s, LivenessEvent::Expired { now: 5 }, 5).is_healthy());
        assert!(fold(s, LivenessEvent::Expired { now: 4 }, 5).is_healthy());
    }
}
