/// GitHub Copilot CLI provider - generates summaries by invoking the `copilot`
/// binary in non-interactive prompt mode (`copilot -p ... --output-format json`).
///
/// Authentication rides on the user's existing Copilot credentials: either a
/// prior `copilot login`, or a GitHub token supplied in the provider config
/// (passed to the child process as COPILOT_GITHUB_TOKEN). No API key is sent
/// over HTTP by this app; the CLI handles all network access.
///
/// The prompt (which embeds the full transcript) can exceed OS argument-length
/// limits - notably the ~32KB command-line cap on Windows - so it is written to
/// a temp file that the CLI reads with its built-in file tool. The system temp
/// directory is readable by the CLI without extra permission flags.
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

const COPILOT_TIMEOUT: Duration = Duration::from_secs(300);

/// Resolves the Copilot CLI binary, preferring an explicitly configured path.
///
/// GUI apps on macOS launch with a minimal PATH, so after a plain PATH lookup
/// fails we probe the common install locations (npm global, Homebrew, and the
/// `gh copilot` download directory).
fn resolve_copilot_binary(configured_path: Option<&str>) -> Result<PathBuf, String> {
    if let Some(configured) = configured_path.map(str::trim).filter(|p| !p.is_empty()) {
        let path = PathBuf::from(configured);
        if path.is_file() {
            // Absolutize so the path survives the child's working directory
            // being set to the temp dir
            return Ok(std::path::absolute(&path).unwrap_or(path));
        }
        return Err(format!(
            "Configured GitHub Copilot CLI binary not found at: {}",
            path.display()
        ));
    }

    let binary_name = if cfg!(target_os = "windows") {
        "copilot.exe"
    } else {
        "copilot"
    };

    if let Ok(found) = which::which(binary_name) {
        return Ok(found);
    }
    // npm installs a `copilot.cmd` shim on Windows
    if cfg!(target_os = "windows") {
        if let Ok(found) = which::which("copilot.cmd") {
            return Ok(found);
        }
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        // `gh copilot` downloads the CLI here
        candidates.push(home.join(".local/share/gh/copilot").join(binary_name));
        candidates.push(home.join(".npm-global/bin").join(binary_name));
        if cfg!(target_os = "windows") {
            candidates.push(home.join("AppData/Roaming/npm/copilot.cmd"));
        }
    }
    if cfg!(target_os = "macos") {
        candidates.push(PathBuf::from("/opt/homebrew/bin/copilot"));
        candidates.push(PathBuf::from("/usr/local/bin/copilot"));
    }
    if cfg!(target_os = "linux") {
        candidates.push(PathBuf::from("/usr/local/bin/copilot"));
        candidates.push(PathBuf::from("/usr/bin/copilot"));
    }

    candidates.into_iter().find(|p| p.is_file()).ok_or_else(|| {
        "GitHub Copilot CLI not found. Install it with `npm install -g @github/copilot` \
             (or run `gh copilot` once), then sign in with `copilot login`. If it is installed \
             in a non-standard location, set the binary path in Settings."
            .to_string()
    })
}

/// Extracts the final assistant reply from Copilot CLI JSONL output
/// (`--output-format json`).
///
/// The stream contains ephemeral delta events plus complete `assistant.message`
/// events; intermediate messages narrate tool use (they carry `toolRequests`),
/// so the answer is the last complete message without pending tool requests.
fn extract_final_message(jsonl: &str) -> Option<String> {
    let mut last: Option<String> = None;
    for line in jsonl.lines() {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if event
            .get("ephemeral")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        if event.get("type").and_then(|v| v.as_str()) != Some("assistant.message") {
            continue;
        }
        let Some(data) = event.get("data") else {
            continue;
        };
        let has_tool_requests = data
            .get("toolRequests")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        if has_tool_requests {
            continue;
        }
        if let Some(content) = data.get("content").and_then(|v| v.as_str()) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                last = Some(trimmed.to_string());
            }
        }
    }
    last
}

/// Writes the prompt file restricted to the current user (0600 on Unix) since
/// it contains the meeting transcript and lives briefly in the shared temp dir
fn write_prompt_file(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    use std::io::Write;
    file.write_all(contents.as_bytes())
}

/// Trims process output to a readable snippet for error messages
fn output_snippet(text: &str) -> String {
    let trimmed = text.trim();
    let mut snippet: String = trimmed.chars().take(500).collect();
    if snippet.len() < trimmed.len() {
        snippet.push_str("...");
    }
    snippet
}

/// Generates a summary by running the GitHub Copilot CLI as a subprocess
///
/// # Arguments
/// * `binary_path` - Optional explicit path to the `copilot` binary
/// * `model_name` - Copilot model id (e.g. "auto", "claude-sonnet-4.5"); empty or "auto" lets Copilot pick
/// * `github_token` - Optional GitHub token passed as COPILOT_GITHUB_TOKEN (otherwise `copilot login` credentials are used)
/// * `system_prompt` / `user_prompt` - Prompts, written to a temp file the CLI reads
/// * `cancellation_token` - Optional token to abort (kills the child process)
pub async fn generate_with_copilot_cli(
    binary_path: Option<&str>,
    model_name: &str,
    github_token: Option<&str>,
    system_prompt: &str,
    user_prompt: &str,
    cancellation_token: Option<&CancellationToken>,
) -> Result<String, String> {
    let binary = resolve_copilot_binary(binary_path)?;

    let prompt_file =
        crate::storage::tmp_dir().join(format!("meetily-copilot-{}.md", uuid::Uuid::new_v4()));
    let prompt_body = format!("{}\n\n{}", system_prompt, user_prompt);
    write_prompt_file(&prompt_file, &prompt_body)
        .map_err(|e| format!("Failed to write Copilot CLI prompt file: {}", e))?;

    let result = run_copilot(
        &binary,
        &prompt_file,
        model_name,
        github_token,
        cancellation_token,
    )
    .await;

    if let Err(e) = tokio::fs::remove_file(&prompt_file).await {
        warn!(
            "Failed to remove Copilot CLI prompt file {}: {}",
            prompt_file.display(),
            e
        );
    }

    result
}

async fn run_copilot(
    binary: &PathBuf,
    prompt_file: &PathBuf,
    model_name: &str,
    github_token: Option<&str>,
    cancellation_token: Option<&CancellationToken>,
) -> Result<String, String> {
    let instruction = format!(
        "Read the file at {} and follow the instructions it contains. \
         Reply with ONLY the requested output - no preamble and no commentary about reading the file.",
        prompt_file.display()
    );

    let mut command = Command::new(binary);
    command
        .arg("-p")
        .arg(&instruction)
        .arg("--output-format")
        .arg("json")
        .arg("--no-color")
        .arg("--no-auto-update")
        .arg("--no-ask-user")
        .arg("--no-custom-instructions")
        .arg("--disable-builtin-mcps")
        .arg("--log-level")
        .arg("none")
        .current_dir(crate::storage::tmp_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let model = model_name.trim();
    if !model.is_empty() {
        command.arg("--model").arg(model);
    }

    if let Some(token) = github_token.map(str::trim).filter(|t| !t.is_empty()) {
        command.env("COPILOT_GITHUB_TOKEN", token);
    }

    // Hide console window on Windows to prevent CMD popup
    // (creation_flags is an inherent method on tokio's Command)
    #[cfg(target_os = "windows")]
    {
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    info!(
        "🤖 Copilot CLI request: binary={}, model={}",
        binary.display(),
        if model.is_empty() { "auto" } else { model }
    );

    let output_future = tokio::time::timeout(COPILOT_TIMEOUT, command.output());

    // kill_on_drop terminates the child if cancellation wins the select
    let timed_output = if let Some(token) = cancellation_token {
        tokio::select! {
            result = output_future => result,
            _ = token.cancelled() => {
                return Err("Summary generation was cancelled".to_string());
            }
        }
    } else {
        output_future.await
    };

    let output = match timed_output {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            return Err(format!(
                "Failed to run GitHub Copilot CLI ({}): {}",
                binary.display(),
                e
            ))
        }
        Err(_) => {
            return Err(format!(
                "GitHub Copilot CLI timed out after {} seconds",
                COPILOT_TIMEOUT.as_secs()
            ))
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        let detail = if !stderr.trim().is_empty() {
            output_snippet(&stderr)
        } else {
            output_snippet(&stdout)
        };
        return Err(format!(
            "GitHub Copilot CLI failed ({}): {}. If you are not signed in, run `copilot login` \
             or set a GitHub token in Settings.",
            output.status, detail
        ));
    }

    extract_final_message(&stdout).ok_or_else(|| {
        format!(
            "GitHub Copilot CLI returned no assistant response. Output: {}",
            output_snippet(&stdout)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_last_complete_message_without_tool_requests() {
        let jsonl = concat!(
            r#"{"type":"session.tools_updated","data":{"model":"gpt-5-mini"},"ephemeral":true}"#,
            "\n",
            r#"{"type":"assistant.message_delta","data":{"deltaContent":"Read"},"ephemeral":true}"#,
            "\n",
            r#"{"type":"assistant.message","data":{"content":"Reading the transcript file.","toolRequests":[{"name":"view"}]}}"#,
            "\n",
            r#"{"type":"assistant.message","data":{"content":"The team approved the Q3 budget.","toolRequests":[]}}"#,
            "\n",
            r#"{"type":"result","data":{}}"#,
        );

        assert_eq!(
            extract_final_message(jsonl),
            Some("The team approved the Q3 budget.".to_string())
        );
    }

    #[test]
    fn ignores_ephemeral_and_malformed_lines() {
        let jsonl = concat!(
            "not json at all\n",
            r#"{"type":"assistant.message","data":{"content":"ephemeral echo","toolRequests":[]},"ephemeral":true}"#,
            "\n",
            r#"{"type":"assistant.message","data":{"content":"  final answer  ","toolRequests":[]}}"#,
        );

        assert_eq!(
            extract_final_message(jsonl),
            Some("final answer".to_string())
        );
    }

    #[test]
    fn returns_none_when_no_final_message() {
        let jsonl = r#"{"type":"assistant.message","data":{"content":"narration","toolRequests":[{"name":"view"}]}}"#;
        assert_eq!(extract_final_message(jsonl), None);
    }

    #[test]
    fn configured_binary_path_must_exist() {
        let err = resolve_copilot_binary(Some("/nonexistent/path/to/copilot")).unwrap_err();
        assert!(err.contains("not found at"));
    }
}
