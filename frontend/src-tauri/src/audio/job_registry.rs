// Shared job registry for batch transcription jobs (imports, retranscriptions).
//
// `ImportGuard` and `RetranscriptionGuard` grew as near-identical ~80-line
// state machines (active-ID set + cancel set + engine claim / remote permit +
// drain-time cleanup). This module is the single implementation both wrap, so
// the two cannot drift apart.

use crate::audio::engine_coordinator::{LocalEngineClaim, LocalEngineUser};
use crate::audio::remote_concurrency::RemoteJobPermit;
use std::collections::HashSet;
use std::sync::Mutex;

/// Registry of in-flight jobs of one kind (imports or retranscriptions).
///
/// Tracks which job IDs are active and which have been asked to cancel, and
/// coordinates each job's claim on the shared local engines (local jobs) or
/// the remote concurrency cap (remote jobs).
pub(crate) struct JobRegistry {
    /// Which local-engine consumer this registry's local jobs claim as.
    engine_user: LocalEngineUser,
    /// Sentence-initial job label, e.g. "Import".
    kind_title: &'static str,
    /// Mid-sentence job label, e.g. "import".
    kind: &'static str,
    /// What a job ID identifies, e.g. "job" or "meeting".
    id_noun: &'static str,
    active: Mutex<HashSet<String>>,
    cancelled: Mutex<HashSet<String>>,
}

/// RAII guard for one active job. Dropping it clears the job's active and
/// cancelled state, and releases its engine claim or remote slot.
pub(crate) struct JobGuard<'a> {
    registry: &'a JobRegistry,
    job_id: String,
    /// Claim on the shared local engines, held for the guard's lifetime.
    /// `None` for remote jobs, which never touch the local engines.
    _engine_claim: Option<LocalEngineClaim<'static>>,
    /// Remote concurrency slot, held for the guard's lifetime.
    /// `None` for local jobs, which are capped by the engine claim instead.
    _remote_permit: Option<RemoteJobPermit>,
}

impl JobRegistry {
    pub(crate) fn new(
        engine_user: LocalEngineUser,
        kind_title: &'static str,
        kind: &'static str,
        id_noun: &'static str,
    ) -> Self {
        Self {
            engine_user,
            kind_title,
            kind,
            id_noun,
            active: Mutex::new(HashSet::new()),
            cancelled: Mutex::new(HashSet::new()),
        }
    }

    /// Reserve a slot for the given job ID.
    pub(crate) fn acquire(&self, job_id: String, use_remote: bool) -> Result<JobGuard<'_>, String> {
        {
            let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());

            if active.contains(&job_id) {
                return Err(format!(
                    "{} already in progress for {} {}",
                    self.kind_title, self.id_noun, job_id
                ));
            }

            active.insert(job_id.clone());
        }

        // A cancel request for a previous job with this ID must not carry
        // over into the new job.
        self.cancelled
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&job_id);

        let release_active = |job_id: &str| {
            self.active
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(job_id);
        };

        let mut engine_claim = None;
        let mut remote_permit = None;

        if use_remote {
            match crate::audio::remote_concurrency::try_acquire_remote_slot() {
                Ok(permit) => remote_permit = Some(permit),
                Err(e) => {
                    release_active(&job_id);
                    return Err(format!("Cannot start a remote {}: {}", self.kind, e));
                }
            }
        } else {
            use crate::audio::engine_coordinator::LOCAL_ENGINE_COORDINATOR;
            match LOCAL_ENGINE_COORDINATOR.try_claim(self.engine_user) {
                Ok(claim) => engine_claim = Some(claim),
                Err(holder) => {
                    release_active(&job_id);
                    return Err(format!(
                        "Cannot start a local {}: {} is already using the on-device transcription engine. Wait for it to finish, or select a remote transcription provider to run jobs simultaneously.",
                        self.kind, holder
                    ));
                }
            }
        }

        Ok(JobGuard {
            registry: self,
            job_id,
            _engine_claim: engine_claim,
            _remote_permit: remote_permit,
        })
    }

    /// Whether the given job ID is currently active.
    pub(crate) fn is_active(&self, job_id: &str) -> bool {
        self.active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(job_id)
    }

    /// Request cancellation of one job, or of every currently active job when
    /// `job_id` is `None`.
    ///
    /// Cancel-all snapshots the active set rather than setting a sticky global
    /// flag, so a job started while the cancelled ones drain runs normally.
    pub(crate) fn cancel(&self, job_id: Option<&str>) {
        let job_ids_to_cancel = if let Some(job_id) = job_id {
            vec![job_id.to_string()]
        } else {
            self.active
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .cloned()
                .collect()
        };

        self.cancelled
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(job_ids_to_cancel);
    }

    /// Whether the given job has been asked to cancel.
    pub(crate) fn is_cancelled(&self, job_id: &str) -> bool {
        self.cancelled
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(job_id)
    }

    /// Whether any job in this registry is active.
    pub(crate) fn has_active_jobs(&self) -> bool {
        !self
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }
}

impl Drop for JobGuard<'_> {
    fn drop(&mut self) {
        self.registry
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.job_id);

        self.registry
            .cancelled
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.job_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::engine_coordinator::global_coordinator_test_lock;

    fn import_like_registry() -> JobRegistry {
        JobRegistry::new(LocalEngineUser::Import, "Import", "import", "job")
    }

    #[test]
    fn acquire_registers_job_and_drop_clears_it() {
        let _serial = global_coordinator_test_lock();
        let registry = import_like_registry();

        assert!(!registry.has_active_jobs());

        let guard = registry
            .acquire("job-1".to_string(), false)
            .expect("acquire on an idle registry must succeed");

        assert!(registry.is_active("job-1"));
        assert!(!registry.is_active("job-2"));
        assert!(registry.has_active_jobs());

        drop(guard);

        assert!(!registry.is_active("job-1"));
        assert!(!registry.has_active_jobs());
    }

    #[test]
    fn double_acquire_of_same_id_is_rejected() {
        let _serial = global_coordinator_test_lock();
        let registry = JobRegistry::new(
            LocalEngineUser::Retranscription,
            "Retranscription",
            "retranscription",
            "meeting",
        );

        // Remote jobs may run concurrently, so the same-ID rejection must come
        // from the registry itself, not from engine exclusivity.
        let first = registry
            .acquire("meeting-1".to_string(), true)
            .expect("first acquire must succeed");

        let second = registry.acquire("meeting-1".to_string(), true);
        let err = match second {
            Ok(_) => panic!("second acquire of the same ID must be rejected"),
            Err(err) => err,
        };
        assert_eq!(err, "Retranscription already in progress for meeting meeting-1");

        // The rejection must not have unregistered the running job.
        assert!(registry.is_active("meeting-1"));

        drop(first);
        assert!(!registry.is_active("meeting-1"));
    }

    #[test]
    fn local_job_requires_exclusive_engine_claim() {
        use crate::audio::engine_coordinator::LOCAL_ENGINE_COORDINATOR;

        let _serial = global_coordinator_test_lock();
        let registry = import_like_registry();

        // While a recording holds the engine, a local job must be rejected
        // with an error naming the conflicting consumer.
        let recording_claim = LOCAL_ENGINE_COORDINATOR
            .try_claim(LocalEngineUser::Recording)
            .unwrap();

        let rejected = registry.acquire("job-local-1".to_string(), false);
        let err = match rejected {
            Ok(_) => panic!("local job must not acquire while recording holds the engine"),
            Err(err) => err,
        };
        assert!(
            err.starts_with("Cannot start a local import:"),
            "error should say which kind of job could not start, got: {err}"
        );
        assert!(
            err.contains("recording"),
            "error should name the conflicting consumer, got: {err}"
        );
        // The failed acquire must not leave the job registered as active.
        assert!(!registry.is_active("job-local-1"));

        drop(recording_claim);

        // With the engine free, the local job acquires and holds the claim as
        // this registry's engine user for the guard's lifetime.
        let guard = registry
            .acquire("job-local-1".to_string(), false)
            .expect("local job must acquire once the engine is free");
        assert_eq!(
            LOCAL_ENGINE_COORDINATOR
                .try_claim(LocalEngineUser::Recording)
                .err(),
            Some(LocalEngineUser::Import),
            "a running local job must hold the engine claim"
        );

        drop(guard);
        assert!(
            LOCAL_ENGINE_COORDINATOR
                .try_claim(LocalEngineUser::Recording)
                .is_ok(),
            "dropping the guard must release the engine claim"
        );
    }

    #[test]
    fn remote_jobs_skip_engine_and_share_the_remote_cap() {
        use crate::audio::engine_coordinator::LOCAL_ENGINE_COORDINATOR;
        use crate::audio::remote_concurrency::MAX_CONCURRENT_REMOTE_JOBS;

        let _serial = global_coordinator_test_lock();
        let registry = import_like_registry();

        let mut guards: Vec<JobGuard<'_>> = (0..MAX_CONCURRENT_REMOTE_JOBS)
            .map(|i| {
                registry
                    .acquire(format!("job-remote-{i}"), true)
                    .unwrap_or_else(|e| panic!("remote job {i} within the cap must acquire: {e}"))
            })
            .collect();

        // Remote jobs never touch the local engines.
        assert!(
            LOCAL_ENGINE_COORDINATOR
                .try_claim(LocalEngineUser::Recording)
                .is_ok(),
            "remote jobs must not claim the local engine"
        );

        let over_cap = registry.acquire("job-remote-over-cap".to_string(), true);
        let err = match over_cap {
            Ok(_) => panic!("remote job beyond the cap must be rejected"),
            Err(err) => err,
        };
        assert!(
            err.starts_with("Cannot start a remote import:"),
            "error should say which kind of job could not start, got: {err}"
        );
        // The rejected job must not be left registered as active.
        assert!(!registry.is_active("job-remote-over-cap"));

        // Finishing one job frees a slot for the next.
        guards.pop();
        let after_free = registry.acquire("job-remote-after-free".to_string(), true);
        assert!(
            after_free.is_ok(),
            "remote job must acquire once a slot frees: {:?}",
            after_free.err()
        );
    }

    #[test]
    fn cancel_marks_a_single_job_and_drain_clears_the_flag() {
        let _serial = global_coordinator_test_lock();
        let registry = import_like_registry();

        let cancelled_job = registry.acquire("job-a".to_string(), true).unwrap();
        let _other_job = registry.acquire("job-b".to_string(), true).unwrap();

        registry.cancel(Some("job-a"));
        assert!(registry.is_cancelled("job-a"));
        assert!(
            !registry.is_cancelled("job-b"),
            "cancelling one job must not affect the other"
        );

        // When the cancelled job drains, its flag must not linger — the same
        // ID may be reused later (retranscriptions key jobs by meeting ID).
        drop(cancelled_job);
        assert!(!registry.is_cancelled("job-a"));
    }

    #[test]
    fn acquire_clears_a_stale_cancel_request_for_a_reused_id() {
        let _serial = global_coordinator_test_lock();
        let registry = import_like_registry();

        // A cancel request with no matching active job leaves a stale flag.
        registry.cancel(Some("job-reused"));
        assert!(registry.is_cancelled("job-reused"));

        let _guard = registry
            .acquire("job-reused".to_string(), true)
            .expect("acquire must succeed for a reused ID");
        assert!(
            !registry.is_cancelled("job-reused"),
            "a new job must not inherit a stale cancel request for its ID"
        );
    }

    #[test]
    fn job_started_during_cancel_all_drain_is_not_cancelled() {
        let _serial = global_coordinator_test_lock();
        let registry = import_like_registry();

        let draining = registry
            .acquire("job-drain-old".to_string(), true)
            .expect("first job should acquire");

        registry.cancel(None);
        assert!(registry.is_cancelled("job-drain-old"));

        // The user cancelled the jobs active at the time, not future ones. A
        // job started while the cancel-all is still draining must run normally.
        let fresh = registry
            .acquire("job-drain-new".to_string(), true)
            .expect("new job should acquire while the old one drains");
        assert!(
            !registry.is_cancelled("job-drain-new"),
            "job started after cancel-all must not inherit the cancellation"
        );

        drop(draining);
        assert!(
            !registry.is_cancelled("job-drain-new"),
            "draining the cancelled job must not affect the new job"
        );
        drop(fresh);
        assert!(!registry.has_active_jobs());
    }

    #[test]
    fn poisoned_locks_are_recovered_instead_of_propagating_the_panic() {
        let _serial = global_coordinator_test_lock();
        let registry = import_like_registry();

        // Poison both internal mutexes: panic on a thread while holding each.
        let result = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let _active = registry.active.lock().unwrap();
                    let _cancelled = registry.cancelled.lock().unwrap();
                    panic!("deliberately poison the registry locks");
                })
                .join()
        });
        assert!(result.is_err(), "the poisoning thread must have panicked");
        assert!(registry.active.lock().is_err(), "lock must be poisoned");

        // Every entry point must keep working on the poisoned locks.
        let guard = registry
            .acquire("job-after-poison".to_string(), true)
            .expect("acquire must recover from a poisoned lock");
        assert!(registry.is_active("job-after-poison"));
        assert!(registry.has_active_jobs());

        registry.cancel(Some("job-after-poison"));
        assert!(registry.is_cancelled("job-after-poison"));

        drop(guard);
        assert!(!registry.has_active_jobs());
        assert!(!registry.is_cancelled("job-after-poison"));
    }
}
