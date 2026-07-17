// Coordinates exclusive use of the shared on-device transcription engines.
//
// Recording with realtime transcription, local audio imports, and local
// retranscriptions all use the same global WHISPER_ENGINE / PARAKEET_ENGINE
// instances. Two of them running at once can contend for the engine or swap
// the loaded model out from under the other's in-flight transcription, so at
// most one local-engine consumer may be active at a time. Remote
// (openaiCompatible) jobs never touch the local engines and are not tracked
// here.
//
// Coordination story:
// - Recording claims [`LocalEngineUser::Recording`] at start (only when
//   realtime transcription is enabled — that is the only recording mode that
//   touches the engines) and releases it in `stop_recording`, after the
//   post-recording model unload.
// - `ImportGuard` / `RetranscriptionGuard` claim their user for local jobs and
//   release on guard drop; remote jobs skip the claim and run concurrently.
// - Claims are taken with a synchronous `try_claim` that is never held across
//   an `.await`, so this cannot deadlock: contenders fail fast with an error
//   naming the current holder instead of queueing.
//
// History: recording start used to take the async `ENGINE_LIFECYCLE_LOCK`
// (audio/common.rs) across its whole start path. That was removed because it
// was held across long-running `.await`s (model validation, stream startup)
// and never actually excluded imports/retranscriptions, which do not take
// that lock while transcribing. The lifecycle lock now only serializes
// engine unload after batch jobs; this module is the actual mutual-exclusion
// mechanism between engine consumers.

use std::fmt;
use std::sync::Mutex;

/// A consumer of the shared local transcription engines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalEngineUser {
    Recording,
    Import,
    Retranscription,
}

impl fmt::Display for LocalEngineUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let description = match self {
            LocalEngineUser::Recording => "a recording with realtime transcription",
            LocalEngineUser::Import => "a local audio import",
            LocalEngineUser::Retranscription => "a local retranscription",
        };
        f.write_str(description)
    }
}

/// Single-holder registry for the shared local engines.
pub(crate) struct LocalEngineCoordinator {
    holder: Mutex<Option<LocalEngineUser>>,
}

/// RAII claim on the shared local engines. Dropping it releases the claim.
pub(crate) struct LocalEngineClaim<'a> {
    coordinator: &'a LocalEngineCoordinator,
}

impl LocalEngineCoordinator {
    pub(crate) const fn new() -> Self {
        Self {
            holder: Mutex::new(None),
        }
    }

    /// Claim exclusive use of the shared local engines.
    ///
    /// Fails with the current holder if any consumer — including another
    /// instance of the same kind — already holds the claim.
    pub(crate) fn try_claim(
        &self,
        user: LocalEngineUser,
    ) -> Result<LocalEngineClaim<'_>, LocalEngineUser> {
        let mut holder = self.holder.lock().unwrap_or_else(|e| e.into_inner());
        match *holder {
            Some(existing) => Err(existing),
            None => {
                *holder = Some(user);
                Ok(LocalEngineClaim { coordinator: self })
            }
        }
    }
}

impl Drop for LocalEngineClaim<'_> {
    fn drop(&mut self) {
        *self
            .coordinator
            .holder
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// The process-wide coordinator shared by recording, import, and retranscription.
pub(crate) static LOCAL_ENGINE_COORDINATOR: LocalEngineCoordinator = LocalEngineCoordinator::new();

/// Recording's claim outlives the `start_recording` call, so it is parked here
/// until `stop_recording` releases it. Import/retranscription instead keep
/// their claims inside their RAII job guards.
static RECORDING_ENGINE_CLAIM: Mutex<Option<LocalEngineClaim<'static>>> = Mutex::new(None);

/// Claim the shared local engines for a recording with realtime transcription.
///
/// Returns the claim as an RAII value so an aborted recording start releases it
/// automatically; once the recording is actually running, park it with
/// [`store_recording_claim`].
pub(crate) fn claim_recording_engine() -> Result<LocalEngineClaim<'static>, String> {
    LOCAL_ENGINE_COORDINATOR
        .try_claim(LocalEngineUser::Recording)
        .map_err(|holder| {
            format!(
                "Cannot start recording with realtime transcription: {} is using the on-device transcription engine. Wait for it to finish or cancel it before recording.",
                holder
            )
        })
}

/// Park a recording's engine claim until [`release_recording_claim`] is called.
pub(crate) fn store_recording_claim(claim: LocalEngineClaim<'static>) {
    *RECORDING_ENGINE_CLAIM
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(claim);
}

/// Release the parked recording claim, if any. Safe to call when none is held.
pub(crate) fn release_recording_claim() {
    *RECORDING_ENGINE_CLAIM
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
}

/// Serializes tests (across modules) that touch [`LOCAL_ENGINE_COORDINATOR`],
/// since cargo runs tests in parallel threads.
#[cfg(test)]
pub(crate) fn global_coordinator_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_succeeds_when_engine_free() {
        let coordinator = LocalEngineCoordinator::new();

        let claim = coordinator.try_claim(LocalEngineUser::Import);

        assert!(claim.is_ok());
    }

    #[test]
    fn conflicting_claim_fails_and_reports_holder() {
        let coordinator = LocalEngineCoordinator::new();
        let _import_claim = coordinator.try_claim(LocalEngineUser::Import).unwrap();

        let recording_claim = coordinator.try_claim(LocalEngineUser::Recording);

        assert_eq!(recording_claim.err(), Some(LocalEngineUser::Import));
    }

    #[test]
    fn same_kind_of_consumer_cannot_double_claim() {
        let coordinator = LocalEngineCoordinator::new();
        let _first = coordinator.try_claim(LocalEngineUser::Retranscription).unwrap();

        let second = coordinator.try_claim(LocalEngineUser::Retranscription);

        assert_eq!(second.err(), Some(LocalEngineUser::Retranscription));
    }

    #[test]
    fn recording_claim_blocks_local_jobs_until_released() {
        let _serial = global_coordinator_test_lock();

        let claim = claim_recording_engine().unwrap();
        store_recording_claim(claim);

        assert_eq!(
            LOCAL_ENGINE_COORDINATOR.try_claim(LocalEngineUser::Import).err(),
            Some(LocalEngineUser::Recording)
        );

        release_recording_claim();
        assert!(LOCAL_ENGINE_COORDINATOR
            .try_claim(LocalEngineUser::Import)
            .is_ok());
    }

    #[test]
    fn recording_claim_fails_and_names_holder_while_engine_is_busy() {
        let _serial = global_coordinator_test_lock();
        let _import_claim = LOCAL_ENGINE_COORDINATOR
            .try_claim(LocalEngineUser::Import)
            .unwrap();

        let err = match claim_recording_engine() {
            Ok(_) => panic!("recording must not claim the engine while an import holds it"),
            Err(err) => err,
        };
        assert!(
            err.contains("import"),
            "error should name the conflicting consumer, got: {err}"
        );
    }

    #[test]
    fn aborted_recording_start_releases_unparked_claim() {
        let _serial = global_coordinator_test_lock();

        // Simulates start_recording failing after the claim: the RAII value is
        // dropped without ever being parked.
        let claim = claim_recording_engine().unwrap();
        drop(claim);

        assert!(LOCAL_ENGINE_COORDINATOR
            .try_claim(LocalEngineUser::Import)
            .is_ok());
    }

    #[test]
    fn release_recording_claim_without_parked_claim_is_noop() {
        let _serial = global_coordinator_test_lock();

        release_recording_claim();

        assert!(LOCAL_ENGINE_COORDINATOR
            .try_claim(LocalEngineUser::Recording)
            .is_ok());
    }

    #[test]
    fn dropping_claim_releases_engine_for_next_consumer() {
        let coordinator = LocalEngineCoordinator::new();
        let import_claim = coordinator.try_claim(LocalEngineUser::Import).unwrap();
        drop(import_claim);

        let recording_claim = coordinator.try_claim(LocalEngineUser::Recording);

        assert!(recording_claim.is_ok());
    }
}
