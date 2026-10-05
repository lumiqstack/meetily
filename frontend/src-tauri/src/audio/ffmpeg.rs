use ffmpeg_sidecar::{
    command::ffmpeg_is_installed,
    download::{check_latest_version, download_ffmpeg_package, ffmpeg_download_url, unpack_ffmpeg},
    paths::sidecar_dir,
    version::ffmpeg_version,
};
use log::{debug, error};
use once_cell::sync::Lazy;
use std::path::PathBuf;
use which::which;

#[cfg(not(windows))]
const EXECUTABLE_NAME: &str = "ffmpeg";

#[cfg(windows)]
const EXECUTABLE_NAME: &str = "ffmpeg.exe";

static FFMPEG_PATH: Lazy<Option<PathBuf>> = Lazy::new(find_ffmpeg_path_internal);

pub fn find_ffmpeg_path() -> Option<PathBuf> {
    FFMPEG_PATH.as_ref().map(|p| p.clone())
}

/// ffprobe sits next to ffmpeg in every distribution we bundle or find.
pub fn find_ffprobe_path() -> Option<PathBuf> {
    let ffprobe = find_ffmpeg_path()?.with_file_name(if cfg!(windows) {
        "ffprobe.exe"
    } else {
        "ffprobe"
    });
    ffprobe.exists().then_some(ffprobe)
}

/// Container duration in whole milliseconds.
///
/// Strict, unlike the best-effort probe used for crash recovery: the Gemini
/// batch planner sizes uploads from this, and a silent 0 would plan a single
/// empty chunk and burn a request on nothing.
///
/// Falls back to ffmpeg when ffprobe is unavailable. That is the normal case,
/// not an edge case: the app bundles only `ffmpeg.exe`, so requiring ffprobe
/// would fail every batch job on a stock install.
pub fn probe_duration_ms(path: &std::path::Path) -> Result<u64, String> {
    match probe_duration_ms_via_ffprobe(path) {
        Ok(ms) => Ok(ms),
        Err(ffprobe_error) => probe_duration_ms_via_ffmpeg(path).map_err(|ffmpeg_error| {
            format!("{} (ffprobe: {})", ffmpeg_error, ffprobe_error)
        }),
    }
}

/// Parse the `Duration: HH:MM:SS.cc` line ffmpeg writes to stderr.
///
/// Split out so it is testable without invoking ffmpeg.
pub(crate) fn parse_ffmpeg_duration_ms(stderr: &str) -> Option<u64> {
    let after = stderr.split("Duration:").nth(1)?.trim_start();
    let field = after.split(',').next()?.trim();
    if field.starts_with("N/A") {
        return None;
    }

    let mut parts = field.split(':');
    let hours: u64 = parts.next()?.trim().parse().ok()?;
    let minutes: u64 = parts.next()?.trim().parse().ok()?;
    let seconds: f64 = parts.next()?.trim().parse().ok()?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }

    Some((hours * 3_600_000) + (minutes * 60_000) + (seconds * 1000.0).round() as u64)
}

fn probe_duration_ms_via_ffmpeg(path: &std::path::Path) -> Result<u64, String> {
    let ffmpeg = find_ffmpeg_path().ok_or("ffmpeg not found")?;

    // `ffmpeg -i FILE` with no output prints the container header to stderr and
    // exits non-zero ("At least one output file must be specified"). That exit
    // status is expected, so only the parsed duration decides success.
    let mut command = std::process::Command::new(ffmpeg);
    command.arg("-hide_banner").arg("-i").arg(path);

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let output = command
        .output()
        .map_err(|e| format!("could not run ffmpeg: {}", e))?;

    parse_ffmpeg_duration_ms(&String::from_utf8_lossy(&output.stderr)).ok_or_else(|| {
        format!(
            "ffmpeg reported no duration for {}",
            path.display()
        )
    })
}

fn probe_duration_ms_via_ffprobe(path: &std::path::Path) -> Result<u64, String> {
    let ffprobe = find_ffprobe_path().ok_or("ffprobe not found next to ffmpeg")?;

    let mut command = std::process::Command::new(ffprobe);
    command.args([
        "-v",
        "error",
        "-show_entries",
        "format=duration",
        "-of",
        "default=noprint_wrappers=1:nokey=1",
    ]);
    command.arg(path);

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let output = command
        .output()
        .map_err(|e| format!("could not run ffprobe: {}", e))?;

    if !output.status.success() {
        return Err(format!(
            "ffprobe failed on {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let seconds: f64 = text
        .trim()
        .parse()
        .map_err(|_| format!("ffprobe returned no duration for {}", path.display()))?;

    if !seconds.is_finite() || seconds < 0.0 {
        return Err(format!("ffprobe returned {} for {}", seconds, path.display()));
    }

    Ok((seconds * 1000.0).round() as u64)
}

fn find_ffmpeg_path_internal() -> Option<PathBuf> {
    debug!("Starting search for ffmpeg executable");

    // ============================================================
    // PRIORITY 1: Bundled Binary (Production)
    // ============================================================
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_folder) = exe_path.parent() {
            let bundled = exe_folder.join(EXECUTABLE_NAME);
            if bundled.exists() && bundled.is_file() {
                debug!("Found bundled ffmpeg: {:?}", bundled);
                return Some(bundled);
            }
        }
    }


    // ============================================================
    // PRIORITY 2: Fallback to Existing Logic
    // ============================================================

    // Check if `ffmpeg` is in the PATH environment variable
    if let Ok(path) = which(EXECUTABLE_NAME) {
        debug!("Found ffmpeg in PATH: {:?}", path);
        return Some(path);
    }
    debug!("ffmpeg not found in PATH");

    // Check in $HOME/.local/bin on macOS
    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            let local_bin = PathBuf::from(home).join(".local").join("bin");
            debug!("Checking $HOME/.local/bin: {:?}", local_bin);
            let ffmpeg_in_local_bin = local_bin.join(EXECUTABLE_NAME);
            if ffmpeg_in_local_bin.exists() {
                debug!("Found ffmpeg in $HOME/.local/bin: {:?}", ffmpeg_in_local_bin);
                return Some(ffmpeg_in_local_bin);
            }
            debug!("ffmpeg not found in $HOME/.local/bin");
        }
    }

    // Check in current working directory
    if let Ok(cwd) = std::env::current_dir() {
        debug!("Current working directory: {:?}", cwd);
        let ffmpeg_in_cwd = cwd.join(EXECUTABLE_NAME);
        if ffmpeg_in_cwd.is_file() && ffmpeg_in_cwd.exists() {
            debug!(
                "Found ffmpeg in current working directory: {:?}",
                ffmpeg_in_cwd
            );
            return Some(ffmpeg_in_cwd);
        }
        debug!("ffmpeg not found in current working directory");
    }

    // Check in the same folder as the executable
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_folder) = exe_path.parent() {
            debug!("Executable folder: {:?}", exe_folder);

            // Platform-specific checks
            #[cfg(target_os = "macos")]
            {
                let resources_folder = exe_folder.join("../Resources");
                debug!("Resources folder: {:?}", resources_folder);
                let ffmpeg_in_resources = resources_folder.join(EXECUTABLE_NAME);
                if ffmpeg_in_resources.exists() {
                    debug!(
                        "Found ffmpeg in Resources folder: {:?}",
                        ffmpeg_in_resources
                    );
                    return Some(ffmpeg_in_resources);
                }
                debug!("ffmpeg not found in Resources folder");
            }

            #[cfg(target_os = "linux")]
            {
                let lib_folder = exe_folder.join("lib");
                debug!("Lib folder: {:?}", lib_folder);
                let ffmpeg_in_lib = lib_folder.join(EXECUTABLE_NAME);
                if ffmpeg_in_lib.exists() {
                    debug!("Found ffmpeg in lib folder: {:?}", ffmpeg_in_lib);
                    return Some(ffmpeg_in_lib);
                }
                debug!("ffmpeg not found in lib folder");
            }
        }
    }

    debug!("ffmpeg not found. installing...");

    if let Err(error) = handle_ffmpeg_installation() {
        error!("failed to install ffmpeg: {}", error);
        return None;
    }

    if let Ok(path) = which(EXECUTABLE_NAME) {
        debug!("found ffmpeg after installation: {:?}", path);
        return Some(path);
    }

    let installation_dir = sidecar_dir().map_err(|e| e.to_string()).unwrap();
    let ffmpeg_in_installation = installation_dir.join(EXECUTABLE_NAME);
    if ffmpeg_in_installation.is_file() {
        debug!("found ffmpeg in directory: {:?}", ffmpeg_in_installation);
        return Some(ffmpeg_in_installation);
    }

    // Windows often has nested structure like ffmpeg-6.0-full_build/bin/ffmpeg.exe
    #[cfg(windows)]
    {
        debug!("Searching for nested ffmpeg in {:?}", installation_dir);
        if let Ok(entries) = std::fs::read_dir(&installation_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    // Check bin/ffmpeg.exe
                    let bin_ffmpeg = path.join("bin").join(EXECUTABLE_NAME);
                    if bin_ffmpeg.exists() {
                        debug!("found ffmpeg in nested bin: {:?}", bin_ffmpeg);
                        return Some(bin_ffmpeg);
                    }
                    // Check root of subdir
                    let root_ffmpeg = path.join(EXECUTABLE_NAME);
                    if root_ffmpeg.exists() {
                        debug!("found ffmpeg in nested root: {:?}", root_ffmpeg);
                        return Some(root_ffmpeg);
                    }
                }
            }
        }
    }

    error!("ffmpeg not found even after installation");
    None // Return None if ffmpeg is not found
}

fn handle_ffmpeg_installation() -> Result<(), anyhow::Error> {
    if ffmpeg_is_installed() {
        debug!("ffmpeg is already installed");
        return Ok(());
    }

    debug!("ffmpeg not found. installing...");
    match check_latest_version() {
        Ok(version) => debug!("latest version: {}", version),
        Err(e) => debug!("skipping version check due to error: {e}"),
    }

    let download_url = ffmpeg_download_url()?;
    let destination = get_ffmpeg_install_dir()?;

    debug!("downloading from: {:?}", download_url);
    let archive_path = download_ffmpeg_package(download_url, &destination)?;
    debug!("downloaded package: {:?}", archive_path);

    debug!("extracting...");
    unpack_ffmpeg(&archive_path, &destination)?;

    let version = ffmpeg_version()?;

    debug!("done! installed ffmpeg version {}", version);
    Ok(())
}

#[cfg(target_os = "macos")]
fn get_ffmpeg_install_dir() -> Result<PathBuf, anyhow::Error> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("couldn't find home directory"))?;

    let local_bin = home.join(".local").join("bin");

    // Create directory if it doesn't exist
    if !local_bin.exists() {
        debug!("creating .local/bin directory");
        std::fs::create_dir_all(&local_bin)?;

        // Check both .bashrc and .zshrc
        let shell_configs = vec![
            home.join(".bashrc"),
            home.join(".bash_profile"), // macOS often uses .bash_profile instead of .bashrc
            home.join(".zshrc"),
        ];

        for config in shell_configs {
            if config.exists() {
                let content = std::fs::read_to_string(&config)?;
                if !content.contains(".local/bin") {
                    debug!("adding .local/bin to PATH in {:?}", config);
                    std::fs::write(
                        config,
                        format!("{}\nexport PATH=\"$HOME/.local/bin:$PATH\"\n", content),
                    )?;
                }
            }
        }
    }

    Ok(local_bin)
}

// For other platforms, keep your existing installation directory logic
#[cfg(not(target_os = "macos"))]
fn get_ffmpeg_install_dir() -> Result<PathBuf, anyhow::Error> {
    // Your existing logic for other platforms
    sidecar_dir().map_err(|e| anyhow::anyhow!(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real ffmpeg stderr for a 4m19.2s recording.
    const SAMPLE: &str = r#"Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'audio.mp4':
  Metadata:
    major_brand     : isom
  Duration: 00:04:19.20, start: 0.000000, bitrate: 192 kb/s
  Stream #0:0[0x1](und): Audio: aac (LC), 48000 Hz, mono, fltp, 192 kb/s
At least one output file must be specified"#;

    #[test]
    fn parses_the_duration_ffmpeg_prints_to_stderr() {
        // 4*60 + 19.2 = 259.2s
        assert_eq!(parse_ffmpeg_duration_ms(SAMPLE), Some(259_200));
    }

    #[test]
    fn parses_hours() {
        let text = "  Duration: 01:30:00.00, start: 0.000000, bitrate: 64 kb/s";
        assert_eq!(parse_ffmpeg_duration_ms(text), Some(90 * 60 * 1000));
    }

    #[test]
    fn parses_zero_and_sub_second_durations() {
        assert_eq!(
            parse_ffmpeg_duration_ms("Duration: 00:00:00.50, start: 0"),
            Some(500)
        );
    }

    #[test]
    fn rejects_unknown_or_missing_durations() {
        // A silent 0 here would plan one empty chunk and spend a request on
        // nothing, so "no duration" must stay an error rather than a default.
        assert_eq!(parse_ffmpeg_duration_ms("Duration: N/A, start: 0"), None);
        assert_eq!(parse_ffmpeg_duration_ms("no duration line here"), None);
        assert_eq!(parse_ffmpeg_duration_ms("Duration: garbage,"), None);
        assert_eq!(parse_ffmpeg_duration_ms(""), None);
    }

    /// The batch planner depends on this working with only `ffmpeg.exe`
    /// present — the app does not bundle ffprobe, so a probe that required it
    /// would fail every Gemini job on a stock install.
    #[test]
    fn probes_a_real_file_without_requiring_ffprobe() {
        let Some(ffmpeg) = find_ffmpeg_path() else {
            eprintln!("skipping: ffmpeg not available in this environment");
            return;
        };

        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("probe-test.wav");

        // 2 seconds of silence, encoded by ffmpeg itself.
        let status = std::process::Command::new(&ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(["-f", "lavfi", "-i", "anullsrc=r=16000:cl=mono", "-t", "2"])
            .arg(&wav)
            .status()
            .expect("ffmpeg should run");
        assert!(status.success(), "could not build the fixture");

        let measured = probe_duration_ms(&wav).expect("duration must be readable");
        assert!(
            (measured as i64 - 2000).abs() < 150,
            "expected ~2000ms, got {}",
            measured
        );
    }
}
