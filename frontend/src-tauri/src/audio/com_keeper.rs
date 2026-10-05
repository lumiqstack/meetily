//! Keeps cpal's Windows device enumerator valid for the life of the process.
//!
//! cpal's WASAPI backend holds one process-wide `IMMDeviceEnumerator`, created
//! lazily by whichever thread enumerates audio devices first, inside that
//! thread's COM initialization. cpal uninitializes COM when that thread exits,
//! so if the first enumeration happened on a short-lived thread (a blocking
//! pool thread, a test thread), every later device listing uses a dangling
//! enumerator and the process dies with STATUS_ACCESS_VIOLATION.
//!
//! [`ensure`] creates the enumerator on a dedicated thread that joins the
//! multithreaded apartment and never exits. Call it before anything touches
//! cpal: at app start, and in tests that enumerate devices. It is a no-op on
//! other platforms and after the first call.

#[cfg(target_os = "windows")]
pub fn ensure() {
    use std::sync::{mpsc, Once};
    use std::time::Duration;

    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let (ready_tx, ready_rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("wasapi-com-keeper".into())
            .spawn(move || {
                use cpal::traits::HostTrait;
                use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

                // Joining the MTA first makes cpal's own STA attempt return
                // RPC_E_CHANGED_MODE, so cpal never uninitializes COM here.
                // This thread is never torn down, so neither is the MTA.
                let joined = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                if joined.is_err() {
                    log::warn!("WASAPI COM keeper: CoInitializeEx failed: {joined:?}");
                }
                // Forces cpal to create its shared enumerator on this thread.
                match cpal::default_host().output_devices() {
                    Ok(devices) => log::info!("WASAPI COM keeper ready ({} output device(s))", devices.count()),
                    Err(e) => log::warn!("WASAPI COM keeper: device enumeration failed: {e}"),
                }
                let _ = ready_tx.send(());
                loop {
                    std::thread::park();
                }
            });
        match spawned {
            Ok(_) => {
                if ready_rx.recv_timeout(Duration::from_secs(10)).is_err() {
                    log::warn!("WASAPI COM keeper did not report ready within 10s");
                }
            }
            Err(e) => log::warn!("WASAPI COM keeper thread could not start: {e}"),
        }
    });
}

#[cfg(not(target_os = "windows"))]
pub fn ensure() {}

#[cfg(test)]
mod tests {
    use cpal::traits::{DeviceTrait, HostTrait};

    fn list_output_device_names() -> Vec<String> {
        cpal::default_host()
            .output_devices()
            .map(|devices| devices.filter_map(|d| d.name().ok()).collect())
            .unwrap_or_default()
    }

    /// The failure this module prevents: a thread that enumerates devices
    /// exits, then another thread enumerates. Without the keeper this crashes
    /// the test process on Windows.
    #[test]
    fn enumerating_after_the_first_enumerating_thread_exits_is_safe() {
        super::ensure();
        let first = std::thread::spawn(list_output_device_names).join().unwrap();
        let second = std::thread::spawn(list_output_device_names).join().unwrap();
        assert_eq!(first, second);
    }
}
