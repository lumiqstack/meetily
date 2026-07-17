// Caps how many remote (openaiCompatible) transcription jobs run at once.
//
// Remote jobs skip the local engine coordinator, but each one still decodes
// and resamples audio locally (CPU-heavy) and opens an upload pipeline to the
// remote endpoint. Without a cap, dropping ~20 files starts 20 simultaneous
// decode + upload pipelines.
//
// Jobs beyond the cap are rejected fast with a clear message (matching the
// fail-fast style of `engine_coordinator`) rather than queued, so the user
// immediately sees which jobs did not start.

use once_cell::sync::Lazy;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Maximum number of remote transcription jobs (imports + retranscriptions)
/// allowed to run concurrently.
pub(crate) const MAX_CONCURRENT_REMOTE_JOBS: usize = 3;

static REMOTE_JOB_SLOTS: Lazy<Arc<Semaphore>> =
    Lazy::new(|| Arc::new(Semaphore::new(MAX_CONCURRENT_REMOTE_JOBS)));

/// RAII permit for one running remote job. Dropping it frees the slot.
pub(crate) struct RemoteJobPermit {
    _permit: OwnedSemaphorePermit,
}

/// Reserve a slot for a remote transcription job, failing fast when the cap
/// is reached.
pub(crate) fn try_acquire_remote_slot() -> Result<RemoteJobPermit, String> {
    REMOTE_JOB_SLOTS
        .clone()
        .try_acquire_owned()
        .map(|permit| RemoteJobPermit { _permit: permit })
        .map_err(|_| {
            format!(
                "Too many remote transcription jobs are running (limit {}). Wait for one to finish before starting another.",
                MAX_CONCURRENT_REMOTE_JOBS
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::engine_coordinator::global_coordinator_test_lock;

    #[test]
    fn acquires_up_to_cap_then_rejects_with_clear_message() {
        let _serial = global_coordinator_test_lock();

        let permits: Vec<RemoteJobPermit> = (0..MAX_CONCURRENT_REMOTE_JOBS)
            .map(|i| {
                try_acquire_remote_slot()
                    .unwrap_or_else(|e| panic!("permit {} within the cap must acquire: {e}", i))
            })
            .collect();

        let over_cap = try_acquire_remote_slot();
        let err = match over_cap {
            Ok(_) => panic!("acquire beyond the cap must fail"),
            Err(err) => err,
        };
        assert!(
            err.contains(&MAX_CONCURRENT_REMOTE_JOBS.to_string()),
            "error should state the limit, got: {err}"
        );
        assert!(
            err.to_lowercase().contains("remote"),
            "error should say it is about remote jobs, got: {err}"
        );

        drop(permits);
    }

    #[test]
    fn dropping_a_permit_frees_a_slot() {
        let _serial = global_coordinator_test_lock();

        let mut permits: Vec<RemoteJobPermit> = (0..MAX_CONCURRENT_REMOTE_JOBS)
            .map(|_| try_acquire_remote_slot().expect("permit within the cap must acquire"))
            .collect();
        assert!(try_acquire_remote_slot().is_err());

        permits.pop();

        assert!(
            try_acquire_remote_slot().is_ok(),
            "releasing a permit must free a slot for the next job"
        );
    }
}
