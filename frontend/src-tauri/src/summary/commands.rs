use crate::database::repositories::{
    meeting::MeetingsRepository,
    summary::SummaryProcessesRepository, transcript_chunk::TranscriptChunksRepository,
};
use crate::state::AppState;
use crate::summary::metadata::{
    read_detected_summary_language_from_metadata, read_summary_language_from_metadata,
    write_detected_summary_language_to_metadata, write_summary_language_to_metadata,
};
use crate::summary::language_detection::{
    detect_summary_language, SummaryLanguageDetection,
};
use crate::summary::processor::{clean_llm_markdown_detailed, contains_reasoning_marker};
use crate::summary::service::SummaryService;
use log::{error as log_error, info as log_info, warn as log_warn};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use tauri::{AppHandle, Runtime};


static SUMMARY_START_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));
static LAST_SUMMARY_START: LazyLock<Mutex<Option<DateTime<Utc>>>> =
    LazyLock::new(|| Mutex::new(None));
fn next_summary_start(now: DateTime<Utc>) -> DateTime<Utc> {
    let mut last = LAST_SUMMARY_START
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let next = match *last {
        Some(previous) if now <= previous => previous + Duration::nanoseconds(1),
        _ => now,
    };
    *last = Some(next);
    next
}
#[derive(Debug, Serialize, Deserialize)]
pub struct SummaryResponse {
    pub status: String,
    #[serde(rename = "meetingName")]
    pub meeting_name: Option<String>,
    pub meeting_id: String,
    pub start: Option<String>,
    pub end: Option<String>,
    pub data: Option<serde_json::Value>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProcessTranscriptResponse {
    pub message: String,
    pub process_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedRecapResponse {
    pub meeting_id: String,
    pub title: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedRecapFromLinkResponse {
    pub meeting_id: String,
    pub title: String,
    pub obsidian_file_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SummaryLanguageStorage {
    Metadata,
    LocalFallback,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MeetingSummaryLanguagePreference {
    pub language: Option<String>,
    pub storage: SummaryLanguageStorage,
}

impl MeetingSummaryLanguagePreference {
    fn metadata(language: Option<String>) -> Self {
        Self {
            language,
            storage: SummaryLanguageStorage::Metadata,
        }
    }

    fn local_fallback() -> Self {
        Self {
            language: None,
            storage: SummaryLanguageStorage::LocalFallback,
        }
    }
}

enum MeetingFolderResolution {
    Folder(PathBuf),
    NoFolder,
}

/// Persist a recap already extracted from the authenticated Teams webview.
/// This intentionally bypasses recording download, speech-to-text, and LLM
/// summary generation.
async fn save_extracted_copilot_recap(
    state: tauri::State<'_, AppState>,
    title: String,
    recap: String,
    source_url: Option<String>,
) -> Result<ImportedRecapResponse, String> {
    let title = title.trim();
    let recap = recap.trim();
    if title.is_empty() {
        return Err("Meeting title is required.".into());
    }
    if title.chars().count() > 500 {
        return Err("Meeting title is too long (maximum 500 characters).".into());
    }
    if recap.is_empty() {
        return Err("Copilot recap is required.".into());
    }
    if recap.chars().count() > 500_000 {
        return Err("Copilot recap is too large (maximum 500,000 characters).".into());
    }
    if contains_reasoning_marker(recap) {
        return Err("The recap contains a model reasoning marker and was not imported.".into());
    }

    let source_url = source_url
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if let Some(url) = source_url.as_deref() {
        if url.len() > 8_192
            || !url::Url::parse(url)
                .ok()
                .is_some_and(|parsed| matches!(parsed.scheme(), "http" | "https"))
        {
            return Err("Source link must be a valid HTTP or HTTPS URL.".into());
        }
    }

    let summary = serde_json::json!({
        "markdown": recap,
        "imported_from": "microsoft_copilot_recap",
        "source_url": source_url.as_deref(),
    });
    if !summary_is_renderable(&summary) {
        return Err("Copilot recap contains no visible content and was not imported.".into());
    }

    let meeting_id = SummaryProcessesRepository::create_meeting_with_imported_summary(
        state.db_manager.pool(),
        title,
        &summary,
        source_url.as_deref(),
    )
    .await
    .map_err(|e| format!("Failed to import Copilot recap: {e}"))?;

    log_info!(
        "Imported Microsoft Copilot recap as transcript-free meeting {}",
        meeting_id
    );
    Ok(ImportedRecapResponse {
        meeting_id,
        title: title.to_string(),
    })
}

/// Open a Teams recap in the existing SharePoint-authenticated WebView2
/// profile, read its AI summary, and write the imported meeting to Obsidian.
#[tauri::command]
pub async fn api_import_copilot_recap_from_link<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    title: String,
    source_url: String,
) -> Result<ImportedRecapFromLinkResponse, String> {
    let validated_url = crate::audio::teams_recap::validate_recap_url(source_url.trim())
        .map_err(|e| e.to_string())?;
    let source_url = validated_url.as_str();

    // An explicit "import to Obsidian" must not silently succeed only in
    // Meetily because the vault has not been configured on this machine.
    let settings = crate::obsidian::load_obsidian_settings(&app)
        .await
        .map_err(|e| e.to_string())?;
    let vault_path = settings
        .vault_path
        .ok_or_else(|| "Set an Obsidian vault in Meetily Settings before importing a Teams recap link.".to_string())?;
    if !std::path::Path::new(&vault_path).is_dir() {
        return Err("The configured Obsidian vault folder is not available on this machine.".into());
    }

    let recap = crate::audio::teams_recap::extract_teams_recap(&app, source_url)
        .await
        .map_err(|e| e.to_string())?;
    let recap_markdown = format!(
        "{}\n[Open original Teams recap](<{}>)",
        recap.markdown.trim(),
        source_url
    );
    let imported = save_extracted_copilot_recap(
        state,
        title,
        recap_markdown.clone(),
        Some(source_url.to_string()),
    )
    .await?;
    let exported = crate::obsidian::export_copilot_recap_note(
        &app,
        &imported.meeting_id,
        &imported.title,
        &Utc::now().to_rfc3339(),
        &recap_markdown,
    )
    .await
    .map_err(|e| format!(
        "The recap was saved in Meetily as meeting {}, but the Obsidian export failed: {e}",
        imported.meeting_id
    ))?;
    Ok(ImportedRecapFromLinkResponse {
        meeting_id: imported.meeting_id,
        title: imported.title,
        obsidian_file_path: exported.file_path,
    })
}

/// Saves a meeting summary (Native SQLx implementation)
///
/// Expected format: { "markdown": "...", "summary_json": [...BlockNote blocks...] }
#[tauri::command]
pub async fn api_save_meeting_summary<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    meeting_id: String,
    summary: serde_json::Value,
    _auth_token: Option<String>,
) -> Result<serde_json::Value, String> {
    log_info!(
        "api_save_meeting_summary (native) called for meeting_id: {}",
        meeting_id
    );
    if !summary_is_renderable(&summary) {
        return Err("Summary contains no visible content or model reasoning markers and was not saved.".into());
    }
    let pool = state.db_manager.pool();


    match SummaryProcessesRepository::update_meeting_summary(pool, &meeting_id, &summary).await {
        Ok(true) => {
            log_info!("Summary saved successfully for meeting_id: {}", meeting_id);
            Ok(serde_json::json!({
                "message": "Meeting summary saved successfully"
            }))
        }
        Ok(false) => {
            log_warn!(
                "Meeting not found or invalid JSON for meeting_id: {}",
                meeting_id
            );
            Err("Meeting not found or can't convert the json".into())
        }
        Err(e) => {
            log_error!("Failed to save meeting summary for {}: {}", meeting_id, e);
            Err(e.to_string())
        }
    }
}

/// Gets the per-meeting summary language override from metadata.json.
#[tauri::command]
pub async fn api_get_meeting_summary_language<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    meeting_id: String,
) -> Result<MeetingSummaryLanguagePreference, String> {
    log_info!(
        "api_get_meeting_summary_language called for meeting_id: {}",
        meeting_id
    );

    match resolve_meeting_folder(state.db_manager.pool(), &meeting_id).await? {
        MeetingFolderResolution::Folder(folder) => read_summary_language_from_metadata(&folder)
            .map(MeetingSummaryLanguagePreference::metadata)
            .map_err(|e| e.to_string()),
        MeetingFolderResolution::NoFolder => Ok(MeetingSummaryLanguagePreference::local_fallback()),
    }
}

/// Saves or clears the per-meeting summary language override in metadata.json.
#[tauri::command]
pub async fn api_save_meeting_summary_language<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    meeting_id: String,
    summary_language: Option<String>,
) -> Result<MeetingSummaryLanguagePreference, String> {
    log_info!(
        "api_save_meeting_summary_language called for meeting_id: {}, language: {:?}",
        meeting_id,
        summary_language
    );

    match resolve_meeting_folder(state.db_manager.pool(), &meeting_id).await? {
        MeetingFolderResolution::Folder(folder) => {
            write_summary_language_to_metadata(&folder, summary_language.as_deref())
                .map_err(|e| e.to_string())?;
            read_summary_language_from_metadata(&folder)
                .map(MeetingSummaryLanguagePreference::metadata)
                .map_err(|e| e.to_string())
        }
        MeetingFolderResolution::NoFolder => Ok(MeetingSummaryLanguagePreference::local_fallback()),
    }
}

/// Gets the cached Auto-detected summary language from metadata.json.
#[tauri::command]
pub async fn api_get_meeting_detected_summary_language<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    meeting_id: String,
) -> Result<MeetingSummaryLanguagePreference, String> {
    log_info!(
        "api_get_meeting_detected_summary_language called for meeting_id: {}",
        meeting_id
    );

    match resolve_meeting_folder(state.db_manager.pool(), &meeting_id).await? {
        MeetingFolderResolution::Folder(folder) => read_detected_summary_language_from_metadata(&folder)
            .map(MeetingSummaryLanguagePreference::metadata)
            .map_err(|e| e.to_string()),
        MeetingFolderResolution::NoFolder => Ok(MeetingSummaryLanguagePreference::local_fallback()),
    }
}

/// Saves or clears the cached Auto-detected summary language in metadata.json.
#[tauri::command]
pub async fn api_save_meeting_detected_summary_language<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    meeting_id: String,
    detected_summary_language: Option<String>,
) -> Result<MeetingSummaryLanguagePreference, String> {
    log_info!(
        "api_save_meeting_detected_summary_language called for meeting_id: {}, language: {:?}",
        meeting_id,
        detected_summary_language
    );

    match resolve_meeting_folder(state.db_manager.pool(), &meeting_id).await? {
        MeetingFolderResolution::Folder(folder) => {
            write_detected_summary_language_to_metadata(&folder, detected_summary_language.as_deref())
                .map_err(|e| e.to_string())?;
            read_detected_summary_language_from_metadata(&folder)
                .map(MeetingSummaryLanguagePreference::metadata)
                .map_err(|e| e.to_string())
        }
        MeetingFolderResolution::NoFolder => Ok(MeetingSummaryLanguagePreference::local_fallback()),
    }
}

/// Detects the dominant supported summary language from transcript segments.
#[tauri::command]
pub async fn api_detect_transcript_summary_language(
    transcript_texts: Vec<String>,
) -> Result<SummaryLanguageDetection, String> {
    Ok(detect_summary_language(&transcript_texts))
}

async fn resolve_meeting_folder(
    pool: &sqlx::SqlitePool,
    meeting_id: &str,
) -> Result<MeetingFolderResolution, String> {
    let meeting = MeetingsRepository::get_meeting_metadata(pool, meeting_id)
        .await
        .map_err(|e| format!("Failed to load meeting metadata: {}", e))?
        .ok_or_else(|| format!("Meeting not found: {}", meeting_id))?;

    let Some(folder_path) = meeting.folder_path.filter(|p| !p.trim().is_empty()) else {
        return Ok(MeetingFolderResolution::NoFolder);
    };

    Ok(MeetingFolderResolution::Folder(PathBuf::from(folder_path)))
}

fn blocknote_has_visible_text(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Array(items) => items.iter().any(blocknote_has_visible_text),
        serde_json::Value::Object(object) => {
            object
                .get("text")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| !text.trim().is_empty())
                || object
                    .get("content")
                    .is_some_and(blocknote_has_visible_text)
                || object
                    .get("children")
                    .is_some_and(blocknote_has_visible_text)
        }
        _ => false,
    }
}

fn legacy_section_has_visible_text(value: &serde_json::Value) -> bool {
    value
        .get("blocks")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|blocks| {
            blocks.iter().any(|block| {
                block
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|content| !content.trim().is_empty())
            })
        })
}

fn summary_contains_reasoning_marker(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Array(items) => items.iter().any(summary_contains_reasoning_marker),
        serde_json::Value::Object(object) => object.iter().any(|(key, value)| match key.as_str() {
            "markdown" | "text" | "content" => value
                .as_str()
                .is_some_and(contains_reasoning_marker)
                || value.is_array() && summary_contains_reasoning_marker(value),
            "summary_json" | "children" => summary_contains_reasoning_marker(value),
            _ => false,
        }),
        _ => false,
    }
}

fn summary_is_renderable(value: &serde_json::Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if summary_contains_reasoning_marker(value) {
        return false;
    }
    if let Some(markdown) = object.get("markdown") {
        return markdown
            .as_str()
            .is_some_and(|markdown| !clean_llm_markdown_detailed(markdown).markdown.is_empty());
    }
    if let Some(blocks) = object.get("summary_json") {
        return blocknote_has_visible_text(blocks);
    }
    object
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "MeetingName" | "_section_order"))
        .any(|(_, value)| legacy_section_has_visible_text(value))
}

fn sanitize_markdown_value(value: Option<&mut serde_json::Value>) -> bool {
    let Some(serde_json::Value::String(markdown)) = value else {
        return false;
    };
    let cleaned = clean_llm_markdown_detailed(markdown);
    let reasoning_stripped = cleaned.reasoning_stripped;
    *markdown = cleaned.markdown;
    reasoning_stripped
}

fn redact_flagged_reasoning(mut result: serde_json::Value) -> serde_json::Value {
    let Some(object) = result.as_object_mut() else {
        return result;
    };
    let mut reasoning_stripped = object
        .get("reasoning_stripped")
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    if reasoning_stripped {
        object.remove("reasoning");
    }
    reasoning_stripped |= sanitize_markdown_value(object.get_mut("markdown"));
    if let Some(cache) = object
        .get_mut("english_cache")
        .and_then(serde_json::Value::as_object_mut)
    {
        reasoning_stripped |= sanitize_markdown_value(cache.get_mut("markdown"));
    }
    if reasoning_stripped {
        object.insert("reasoning_stripped".to_string(), serde_json::Value::Bool(true));
    }
    result
}

/// Gets summary status and data (Native SQLx implementation)
///
/// Returns summary status (pending/processing/completed/failed) and parsed result data
#[tauri::command]
pub async fn api_get_summary<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    meeting_id: String,
    _auth_token: Option<String>,
) -> Result<SummaryResponse, String> {
    log_info!(
        "api_get_summary (native) called for meeting_id: {}",
        meeting_id
    );
    let pool = state.db_manager.pool();

    match SummaryProcessesRepository::get_summary_data_for_meeting(pool, &meeting_id).await {
        Ok(Some(process)) => {
            let mut status = process.status.to_lowercase();
            let mut error = process.error;

            // Parse result data if it exists (regardless of status) so restored
            // summaries remain available after cancellation or failure.
            let mut data = if let Some(result_str) = process.result {
                match serde_json::from_str::<serde_json::Value>(&result_str) {
                    Ok(parsed) => Some(redact_flagged_reasoning(parsed)),
                    Err(e) => {
                        log_error!("Failed to parse summary result JSON: {}", e);
                        None
                    }
                }
            } else {
                None
            };
            if status == "completed" && !data.as_ref().is_some_and(summary_is_renderable) {
                status = "error".to_string();
                error = Some(
                    "Stored summary data is missing or invalid. Generate the summary again."
                        .to_string(),
                );
                data = None;
            }

            // Fetch meeting title from database
            let meeting_name = match MeetingsRepository::get_meeting(pool, &meeting_id).await {
                Ok(Some(meeting_details)) => {
                    log_info!("Fetched meeting title: {}", &meeting_details.title);
                    Some(meeting_details.title)
                }
                Ok(None) => {
                    log_warn!("Meeting not found for meeting_id: {}", meeting_id);
                    None
                }
                Err(e) => {
                    log_error!("Failed to fetch meeting title: {}", e);
                    None
                }
            };

            let response = SummaryResponse {
                status: status.clone(),
                meeting_name,
                meeting_id: meeting_id.clone(),
                start: process
                    .start_time
                    .map(|time| time.to_rfc3339_opts(SecondsFormat::Nanos, true)),
                end: process.end_time.map(|time| time.to_rfc3339()),
                data,
                error,
            };

            log_info!(
                "Summary status for {}: {}, has_data: {}, meeting_name: {:?}",
                meeting_id,
                status,
                response.data.is_some(),
                response.meeting_name
            );
            Ok(response)
        }
        Ok(None) => {
            log_info!("No summary process found for meeting_id: {}", meeting_id);

            // Still fetch meeting title for idle state
            let meeting_name = match MeetingsRepository::get_meeting(pool, &meeting_id).await {
                Ok(Some(meeting_details)) => Some(meeting_details.title),
                _ => None,
            };

            Ok(SummaryResponse {
                status: "idle".to_string(),
                meeting_name,
                meeting_id,
                start: None,
                end: None,
                data: None,
                error: None,
            })
        }
        Err(e) => {
            log_error!("Error retrieving summary for {}: {}", meeting_id, e);
            Err(format!("Failed to retrieve summary: {}", e))
        }
    }
}

/// Processes transcript and generates summary (Native SQLx implementation)
///
/// Spawns a background task and returns immediately with process_id
#[tauri::command]
pub async fn api_process_transcript<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    text: String,
    model: String,
    model_name: String,
    meeting_id: Option<String>,
    _chunk_size: Option<i32>,
    _overlap: Option<i32>,
    custom_prompt: Option<String>,
    template_id: Option<String>,
    summary_language: Option<String>,
    _auth_token: Option<String>,
) -> Result<ProcessTranscriptResponse, String> {
    use uuid::Uuid;

    let m_id = meeting_id.unwrap_or_else(|| format!("meeting-{}", Uuid::new_v4()));
    log_info!(
        "api_process_transcript (native) called for meeting_id: {}, model: {}",
        &m_id,
        &model
    );

    let pool = state.db_manager.pool().clone();
    let final_prompt = custom_prompt.unwrap_or_else(|| "".to_string());
    let final_template_id = template_id.unwrap_or_else(|| "daily_standup".to_string());

    // Normalise empty / whitespace-only to None so "" and null behave identically
    let summary_language = summary_language.and_then(|s| {
        let t = s.trim();
        if t.is_empty() { None } else { Some(t.to_string()) }
    });

    // ponytail: summary starts are rare; use per-meeting locks only if start contention is measured.
    let _start_guard = SUMMARY_START_LOCK.lock().await;
    let started_at = next_summary_start(Utc::now());
    SummaryProcessesRepository::create_or_reset_process(&pool, &m_id, started_at)
        .await
        .map_err(|e| format!("Failed to initialize process: {}", e))?;

    let chunk_size = _chunk_size.unwrap_or(40000);
    let overlap = _overlap.unwrap_or(1000);
    if let Err(error) = TranscriptChunksRepository::save_transcript_data(
        &pool,
        &m_id,
        &text,
        &model,
        &model_name,
        chunk_size,
        overlap,
    )
    .await
    {
        let message = format!("Failed to save transcript data: {}", error);
        let _ = SummaryProcessesRepository::update_process_failed(
            &pool,
            &m_id,
            started_at,
            &message,
        )
        .await;
        return Err(message);
    }

    let cancellation_token =
        SummaryService::register_cancellation_token(&m_id, started_at);
    let meeting_id_clone = m_id.clone();
    tauri::async_runtime::spawn(async move {
        SummaryService::process_transcript_background(
            app,
            pool,
            meeting_id_clone,
            started_at,
            cancellation_token,
            text,
            model,
            model_name,
            final_prompt,
            final_template_id,
            summary_language,
        )
        .await;
    });

    log_info!("🚀 Background task spawned for meeting_id: {}", &m_id);
    Ok(ProcessTranscriptResponse {
        message: "Summary generation started".to_string(),
        process_id: started_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
    })
}

/// Cancels an ongoing summary generation process
///
/// This command triggers the cancellation token for the specified meeting,
/// stopping the summary generation gracefully.
#[tauri::command]
pub async fn api_cancel_summary<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    meeting_id: String,
    process_id: String,
) -> Result<serde_json::Value, String> {
    let started_at = DateTime::parse_from_rfc3339(&process_id)
        .map_err(|_| "Invalid summary process ID".to_string())?
        .with_timezone(&Utc);
    log_info!("api_cancel_summary called for meeting_id: {}", meeting_id);

    let cancellation_requested = SummaryService::cancel_summary(&meeting_id, started_at);
    let cancelled = if cancellation_requested {
        let pool = state.db_manager.pool();
        match SummaryProcessesRepository::update_process_cancelled(pool, &meeting_id, started_at)
            .await
        {
            Ok(true) => {
                log_info!("Successfully cancelled summary generation for meeting_id: {}", meeting_id);
                true
            }
            Ok(false) => {
                log_info!("Summary generation was already terminal for meeting_id: {}", meeting_id);
                false
            }
            Err(error) => {
                log_error!("Failed to update cancellation status for {}: {}", meeting_id, error);
                return Err(format!("Failed to update cancellation status: {}", error));
            }
        }
    } else {
        false
    };

    Ok(serde_json::json!({
        "cancelled": cancelled,
        "message": if cancelled {
            "Summary generation cancelled successfully"
        } else if cancellation_requested {
            "Summary generation was already terminal"
        } else {
            "No active summary generation to cancel"
        },
        "meeting_id": meeting_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_only_flagged_historical_reasoning() {
        assert_eq!(
            redact_flagged_reasoning(json!({
                "reasoning_stripped": true,

                "reasoning": "private",
                "markdown": "visible",
            })),
            json!({"reasoning_stripped": true, "markdown": "visible"})
        );
        assert_eq!(
            redact_flagged_reasoning(json!({"reasoning": "custom", "markdown": "visible"})),
            json!({"reasoning": "custom", "markdown": "visible"})
        );
    }

    #[test]
    fn summary_start_tokens_are_monotonic_with_identical_clock_inputs() {
        let first = next_summary_start(Utc::now());
        assert_eq!(
            next_summary_start(first),
            first + Duration::nanoseconds(1)
        );
    }

    #[test]
    fn summary_validation_rejects_blank_and_reasoning_content() {
        assert!(!summary_is_renderable(&json!({"markdown": "   "})));
        assert!(!summary_is_renderable(&json!({"markdown": "```\r\n```"})));
        assert!(!summary_is_renderable(&json!({
            "summary_json": [{"content": [{"type": "text", "text": "<think>private</think>"}]}]
        })));
        assert!(summary_is_renderable(&json!({
            "summary_json": [{"content": [{"type": "text", "text": "Visible"}]}]
        })));
    }

    #[test]
    fn response_redaction_strips_closed_markdown_envelopes() {
        let redacted = redact_flagged_reasoning(json!({
            "markdown": "Visible\n<think>private</think>\nTail"
        }));
        assert_eq!(redacted["markdown"], "Visible\n\nTail");
        assert_eq!(redacted["reasoning_stripped"], true);
    }
}
