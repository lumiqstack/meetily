//! User-idle detection, so batch transcription only claims the on-device
//! transcription engine while the machine is not in use.

use std::time::Duration;

/// How long since the last keyboard/mouse input, or `None` when the platform
/// cannot tell us (in which case callers treat the machine as busy and defer
/// to explicit user action).
#[cfg(windows)]
pub fn user_idle_duration() -> Option<Duration> {
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };

    // SAFETY: `info` is a correctly sized, initialised LASTINPUTINFO; the
    // call only writes `dwTime`.
    let ok = unsafe { GetLastInputInfo(&mut info) };
    if !ok.as_bool() {
        return None;
    }

    // Both counters are milliseconds since boot and wrap every ~49.7 days;
    // wrapping_sub keeps the delta correct across a wrap.
    let now = unsafe { GetTickCount() };
    Some(Duration::from_millis(
        now.wrapping_sub(info.dwTime) as u64,
    ))
}

#[cfg(not(windows))]
pub fn user_idle_duration() -> Option<Duration> {
    None
}

/// Whether the machine has been idle for at least `minutes`.
///
/// Unknown idle time counts as "not idle": on a platform without detection
/// the pipeline waits for a manual "process now" rather than competing with
/// the user for the CPU/GPU.
pub fn is_idle_for(minutes: u64) -> bool {
    match user_idle_duration() {
        Some(idle) => idle >= Duration::from_secs(minutes * 60),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn idle_duration_is_available_on_windows() {
        // A freshly measured idle time must exist and be sane (< 60 days).
        let idle = user_idle_duration().expect("GetLastInputInfo should succeed");
        assert!(idle < Duration::from_secs(60 * 24 * 60 * 60));
    }

    #[test]
    fn zero_minutes_is_always_satisfied_when_detection_works() {
        if user_idle_duration().is_some() {
            assert!(is_idle_for(0));
        }
    }
}
