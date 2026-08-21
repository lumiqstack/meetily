//! Single resolver for every on-disk location Meetily writes to.
//!
//! Before this module, each subsystem picked its own directory: models and the
//! SQLite DB under Tauri's `app_data_dir()`, recordings under `dirs::audio_dir()`,
//! import staging under `std::env::temp_dir()`, the release log under
//! `%LOCALAPPDATA%`, settings under `dirs::config_dir()`. On Windows that spreads
//! multi-gigabyte model files and an unbounded recordings folder across the
//! system drive with no way to move them.
//!
//! Everything now hangs off one **data root**, resolved once at startup:
//!
//! 1. `MEETILY_DATA_DIR` — env override, for tests and one-off runs.
//! 2. `data_root` in `<app_data_dir>/storage.json` — the persisted user choice.
//! 3. `app_data_dir()` itself — preserves the historical layout for anyone who
//!    never picks a root, so this change is inert until opted into.
//!
//! The pointer file necessarily stays on the system drive: it is how we find the
//! relocated root in the first place.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, Runtime};

pub mod migrate;

/// Must match `identifier` in `tauri.conf.json`. Used to reconstruct the app
/// data directory in [`init_standalone`], before a Tauri `AppHandle` exists.
const APP_IDENTIFIER: &str = "com.meetily.ai";
const POINTER_FILE: &str = "storage.json";
const ENV_OVERRIDE: &str = "MEETILY_DATA_DIR";

/// Tauri's `app_data_dir()` — where the pointer file lives and where all data
/// lived historically. Kept separately from [`DATA_ROOT`] because the migration
/// needs to know where to move things *from*.
static LEGACY_ROOT: OnceLock<PathBuf> = OnceLock::new();
static DATA_ROOT: OnceLock<PathBuf> = OnceLock::new();

#[derive(Debug, Default, Serialize, Deserialize)]
struct Pointer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    data_root: Option<PathBuf>,
}

/// Reconstruct Tauri's `app_data_dir()` without an `AppHandle`.
///
/// Mirrors what `tauri::path::PathResolver::app_data_dir` does per platform. It
/// has to be duplicated because `main()` opens the log file before the Tauri
/// runtime exists, and the log belongs under the data root like everything else.
fn platform_app_data_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        // Tauri uses FOLDERID_RoamingAppData on Windows, i.e. %APPDATA% —
        // *not* %LOCALAPPDATA%, which is what main.rs used to use for logs.
        std::env::var_os("APPDATA").map(|p| PathBuf::from(p).join(APP_IDENTIFIER))
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library/Application Support").join(APP_IDENTIFIER))
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .map(|p| p.join(APP_IDENTIFIER))
    }
}

/// Resolve the data root given the legacy directory, applying the precedence
/// documented on the module. Falls back to `legacy` whenever the configured root
/// cannot be created — an unplugged external drive must not brick the app.
fn resolve(legacy: &Path) -> PathBuf {
    let configured = std::env::var_os(ENV_OVERRIDE)
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| read_pointer(legacy).and_then(|p| p.data_root));

    let Some(candidate) = configured else {
        return legacy.to_path_buf();
    };

    if candidate == legacy {
        return legacy.to_path_buf();
    }

    match std::fs::create_dir_all(&candidate) {
        Ok(()) => candidate,
        Err(e) => {
            // Deliberately loud: silently writing gigabytes back to C: after the
            // user moved everything to D: is worse than a noisy log line.
            log::error!(
                "Configured data root {} is unusable ({}); falling back to {}",
                candidate.display(),
                e,
                legacy.display()
            );
            legacy.to_path_buf()
        }
    }
}

fn pointer_path(legacy: &Path) -> PathBuf {
    legacy.join(POINTER_FILE)
}

fn read_pointer(legacy: &Path) -> Option<Pointer> {
    let raw = std::fs::read_to_string(pointer_path(legacy)).ok()?;
    match serde_json::from_str::<Pointer>(&raw) {
        Ok(p) => Some(p),
        Err(e) => {
            log::warn!("Ignoring malformed {}: {}", POINTER_FILE, e);
            None
        }
    }
}

/// Initialise from the environment, before the Tauri runtime exists.
///
/// Call this as the first statement in `main()`. [`init`] is a no-op afterwards,
/// so ordering between the two does not matter beyond "standalone first".
pub fn init_standalone() {
    let legacy = platform_app_data_dir().unwrap_or_else(|| PathBuf::from("."));
    let _ = LEGACY_ROOT.set(legacy.clone());
    let _ = DATA_ROOT.set(resolve(&legacy));
}

/// Initialise from the Tauri `AppHandle`, which is authoritative for
/// `app_data_dir()`. Safe to call after [`init_standalone`]; the first
/// initialisation wins.
pub fn init<R: Runtime>(app: &AppHandle<R>) {
    if DATA_ROOT.get().is_some() {
        return;
    }
    let legacy = app
        .path()
        .app_data_dir()
        .ok()
        .or_else(platform_app_data_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    let _ = LEGACY_ROOT.set(legacy.clone());
    let _ = DATA_ROOT.set(resolve(&legacy));
}

/// Tauri's `app_data_dir()` — the historical home of everything, and where the
/// pointer file lives. Equal to [`root`] when no relocation is configured.
pub fn legacy_root() -> PathBuf {
    LEGACY_ROOT
        .get()
        .cloned()
        .unwrap_or_else(|| platform_app_data_dir().unwrap_or_else(|| PathBuf::from(".")))
}

/// The resolved data root. Every other accessor in this module hangs off it.
pub fn root() -> PathBuf {
    if let Some(p) = DATA_ROOT.get() {
        return p.clone();
    }
    // Only reachable if a code path runs before init (e.g. a unit test calling
    // an accessor directly). Resolve on the fly rather than panicking.
    let legacy = legacy_root();
    resolve(&legacy)
}

/// True when the data root has been moved off `app_data_dir()`, i.e. there is
/// something for [`migrate`] to do.
pub fn is_relocated() -> bool {
    root() != legacy_root()
}

fn subdir(name: &str) -> PathBuf {
    let path = root().join(name);
    if let Err(e) = std::fs::create_dir_all(&path) {
        log::warn!("Failed to create {}: {}", path.display(), e);
    }
    path
}

/// Whisper, Parakeet, and summary-engine model files. The largest consumer by
/// far — individual models run to several gigabytes.
pub fn models_dir() -> PathBuf {
    subdir("models")
}

/// Runtime-downloaded executables (yt-dlp). Not the bundled ffmpeg, which ships
/// next to the exe and is a program file rather than user data.
pub fn bin_dir() -> PathBuf {
    subdir("bin")
}

pub fn logs_dir() -> PathBuf {
    subdir("logs")
}

/// Scratch space for imports and downloads. Replaces `std::env::temp_dir()` so
/// multi-gigabyte SharePoint downloads do not stage on the system drive.
pub fn tmp_dir() -> PathBuf {
    subdir("tmp")
}

/// Small JSON settings files that previously used `dirs::config_dir()`.
pub fn config_dir() -> PathBuf {
    subdir("config")
}

/// WebView2 user-data folder for the SharePoint auth window. Grows with browser
/// cache, so it belongs with the rest of the relocatable data.
pub fn webview_dir() -> PathBuf {
    subdir("sp-webview")
}

/// Parent for the *main* window's WebView2 profile (the runtime creates an
/// `EBWebView` folder inside it).
///
/// Kept separate from [`webview_dir`] on purpose: WebView2 requires that
/// environments created with different options use different user-data folders,
/// and the SharePoint auth window builds its own environment.
pub fn webview2_dir() -> PathBuf {
    subdir("webview2")
}

/// Default location for meeting folders. Users can still override this
/// independently via the `save_folder` recording preference.
///
/// Only moves under the data root once one has actually been chosen. Without
/// that guard, simply installing this version would relocate every new
/// recording from `~/Music/meetily-recordings` into the roaming profile — a
/// worse place than where they started, for a user who opted into nothing.
pub fn default_recordings_dir() -> PathBuf {
    if is_relocated() {
        subdir("recordings")
    } else {
        historical_recordings_dir()
    }
}

/// Where recordings lived before the data root existed: the platform's
/// Music/Movies folder. Also used by the migration to find data to move.
pub fn historical_recordings_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    let base = dirs::audio_dir().or_else(dirs::document_dir);

    #[cfg(target_os = "macos")]
    let base = dirs::video_dir().or_else(dirs::document_dir);

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = dirs::document_dir();

    base.unwrap_or_else(|| PathBuf::from("."))
        .join("meetily-recordings")
}

/// Directory holding `meeting_minutes.sqlite` and its WAL/SHM sidecars.
pub fn db_dir() -> PathBuf {
    let path = root();
    if let Err(e) = std::fs::create_dir_all(&path) {
        log::warn!("Failed to create {}: {}", path.display(), e);
    }
    path
}

/// Persist a new data root. Takes effect on the next launch — rebinding the
/// model directories, an open SQLite pool, and a live WebView2 profile at
/// runtime is not worth the complexity.
pub fn set_root(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(anyhow!("Data root must be an absolute path"));
    }

    std::fs::create_dir_all(path)
        .map_err(|e| anyhow!("Cannot create {}: {}", path.display(), e))?;

    // Prove it is writable now rather than discovering it at next startup, when
    // the only recourse is a silent fallback to the system drive.
    let probe = path.join(".meetily-write-test");
    std::fs::write(&probe, b"ok")
        .map_err(|e| anyhow!("{} is not writable: {}", path.display(), e))?;
    let _ = std::fs::remove_file(&probe);

    let legacy = legacy_root();
    std::fs::create_dir_all(&legacy)
        .map_err(|e| anyhow!("Cannot create {}: {}", legacy.display(), e))?;

    let pointer = Pointer {
        data_root: if path == legacy {
            None
        } else {
            Some(path.to_path_buf())
        },
    };

    let target = pointer_path(&legacy);
    let tmp = legacy.join(".storage.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&pointer)?)?;
    std::fs::rename(&tmp, &target)?;

    log::info!("Data root set to {} (restart required)", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn resolve_prefers_pointer_over_legacy() {
        let legacy = tempdir().unwrap();
        let target = tempdir().unwrap();

        std::fs::write(
            legacy.path().join(POINTER_FILE),
            serde_json::to_string(&Pointer {
                data_root: Some(target.path().to_path_buf()),
            })
            .unwrap(),
        )
        .unwrap();

        assert_eq!(resolve(legacy.path()), target.path());
    }

    #[test]
    fn resolve_falls_back_to_legacy_without_pointer() {
        let legacy = tempdir().unwrap();
        assert_eq!(resolve(legacy.path()), legacy.path());
    }

    #[test]
    fn resolve_falls_back_when_pointer_is_malformed() {
        let legacy = tempdir().unwrap();
        std::fs::write(legacy.path().join(POINTER_FILE), "{ not json").unwrap();
        assert_eq!(resolve(legacy.path()), legacy.path());
    }

    #[test]
    fn resolve_falls_back_when_target_is_uncreatable() {
        let legacy = tempdir().unwrap();

        // A path under a *file* can never be created as a directory, which is
        // the portable stand-in for "the drive is not mounted".
        let blocker = legacy.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        let unusable = blocker.join("root");

        std::fs::write(
            legacy.path().join(POINTER_FILE),
            serde_json::to_string(&Pointer {
                data_root: Some(unusable),
            })
            .unwrap(),
        )
        .unwrap();

        assert_eq!(resolve(legacy.path()), legacy.path());
    }

    #[test]
    fn set_root_writes_a_readable_pointer() {
        let legacy = tempdir().unwrap();
        let target = tempdir().unwrap();

        let _ = LEGACY_ROOT.set(legacy.path().to_path_buf());
        // set_root reads legacy_root(), which falls back to the platform dir if
        // the OnceLock was already claimed by another test in this binary. Only
        // assert when we actually own it.
        if legacy_root() == legacy.path() {
            set_root(target.path()).unwrap();
            let pointer = read_pointer(legacy.path()).unwrap();
            assert_eq!(pointer.data_root.as_deref(), Some(target.path()));
        }
    }

    #[test]
    fn set_root_rejects_relative_paths() {
        assert!(set_root(Path::new("relative/path")).is_err());
    }
}
