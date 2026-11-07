//! Pull every Copilot recap the Teams Recap app lists into Meetily and the
//! Obsidian vault, one button press at a time. Each recap is keyed on the
//! recording it belongs to (see `teams_recap::meeting_identity_key`), so a
//! recap never creates a second meeting or a second note, and a content hash
//! per key turns an unchanged recap into a skip instead of a rewrite.
//!
//! The list itself comes from `teams_recap_list`, which reads the Recap app's
//! ("Meeting Recaps") Home list in a hidden WebView2 window. Rows it lists without a recap link
//! count as not ready. The listed recaps are then read one after another in
//! one more hidden window, `teams_recap::RecapReader`, so Teams starts once
//! per sync. See docs/TEAMS_RECAP_SYNC_SPEC.md.

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use log::{info, warn};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_store::StoreExt;

const SYNC_STORE_FILE: &str = "teams_recap_sync.json";
const SYNC_STORE_KEY: &str = "state";
const LAST_LISTING_KEY: &str = "lastListing";
/// How far back the first sync reaches when the user has not chosen a date.
const DEFAULT_BACKFILL_DAYS: i64 = 30;
/// A recap appears hours after a meeting and Teams may finish generating one
/// days later, so each sync re-reads this much of the previous window.
const OVERLAP_DAYS: i64 = 7;
pub const PROGRESS_EVENT: &str = "teams-recap-sync-progress";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedRecap {
    pub meeting_id: String,
    /// RFC 3339.
    pub imported_at: String,
    /// SHA-256 of the recap markdown last written, hex.
    pub content_hash: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamsRecapSyncState {
    /// `YYYY-MM-DD` the first sync starts from. Unset means
    /// [`DEFAULT_BACKFILL_DAYS`] before the first sync.
    #[serde(default)]
    pub since_date: Option<String>,
    /// RFC 3339 start time of the last completed sync.
    #[serde(default)]
    pub last_sync_date: Option<String>,
    /// Identity key -> what was written for it.
    #[serde(default)]
    pub imported: HashMap<String, ImportedRecap>,
}

/// What the settings panel shows.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamsRecapSyncStateView {
    pub since_date: Option<String>,
    pub last_sync_date: Option<String>,
    pub known_recaps: usize,
    /// The Recap app can be read on this platform.
    pub available: bool,
    /// The line beside the buttons, once the list has been read.
    pub last_listing: Option<LastListingView>,
}

/// The list as the last check or sync read it. Stored apart from
/// [`TeamsRecapSyncState`]: reading the list never moves the scan window or
/// changes the known recaps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LastListing {
    /// RFC 3339, when the list was read.
    pub read_at: String,
    /// RFC 3339 start of the scan window it was read for.
    pub window_start: String,
    /// Recaps listed with a link.
    pub listed: usize,
    /// Identity keys of the listed recaps whose link is valid, to count the
    /// synced ones.
    #[serde(default)]
    pub keys: Vec<String>,
    #[serde(default)]
    pub rows_without_link: usize,
    /// Why the list was not read, when it was not.
    #[serde(default)]
    pub error: Option<String>,
    /// The oldest slot the list reached, when it stopped before the window
    /// start with rows possibly left unread.
    #[serde(default)]
    pub ended_early_at: Option<String>,
}

impl LastListing {
    fn from_listing(
        read_at: DateTime<Utc>,
        window_start: DateTime<Utc>,
        listing: &super::teams_recap_list::RecapListing,
    ) -> Self {
        // The same key the sync records an import under.
        let keys = listing
            .candidates
            .iter()
            .filter_map(|candidate| crate::audio::teams_recap::validate_recap_url(&candidate.recap_url).ok())
            .map(|url| crate::audio::teams_recap::meeting_identity_key(url.as_str()))
            .collect();
        Self {
            read_at: read_at.to_rfc3339(),
            window_start: window_start.to_rfc3339(),
            listed: listing.candidates.len(),
            keys,
            rows_without_link: listing.rows_without_link,
            error: listing.error.clone(),
            ended_early_at: listing
                .end
                .filter(|end| end.may_be_short())
                .and(listing.oldest_row.clone()),
        }
    }

    /// Counted against the recaps synced so far, so a sync after the check
    /// moves its recaps from pending to synced.
    fn view(&self, imported: &HashMap<String, ImportedRecap>) -> LastListingView {
        let synced = self.keys.iter().filter(|key| imported.contains_key(*key)).count();
        LastListingView {
            read_at: self.read_at.clone(),
            window_start: self.window_start.clone(),
            listed: self.listed,
            synced,
            pending: self.listed.saturating_sub(synced),
            not_ready: self.rows_without_link,
            error: self.error.clone(),
            ended_early_at: self.ended_early_at.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastListingView {
    pub read_at: String,
    pub window_start: String,
    pub listed: usize,
    /// Listed recaps a sync has already saved.
    pub synced: usize,
    /// Listed recaps the next sync would save.
    pub pending: usize,
    /// Listed meetings without a recap link yet.
    pub not_ready: usize,
    pub error: Option<String>,
    pub ended_early_at: Option<String>,
}

/// One meeting row the Recap app lists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecapCandidate {
    pub recap_url: String,
    pub title: String,
    /// When the meeting started, as the list shows it (RFC 3339 when known).
    pub started_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecapSyncFailure {
    pub title: String,
    pub error: String,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecapSyncReport {
    pub scanned: usize,
    pub new: usize,
    pub updated: usize,
    pub skipped: usize,
    pub not_ready: usize,
    pub failed: Vec<RecapSyncFailure>,
    /// Titles of recaps imported as their own meeting although a live
    /// recording overlaps them in time without being confirmed as the same.
    pub possible_duplicates: Vec<String>,
    pub meetings_folder: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncProgress<'a> {
    done: usize,
    total: usize,
    current_title: &'a str,
}

#[derive(Debug, PartialEq)]
enum Outcome {
    New { possible_duplicate: bool },
    Updated,
    Skipped,
    NotReady,
    Failed(String),
}

// ---------------------------------------------------------------------------
// State store
// ---------------------------------------------------------------------------

fn load_state<R: Runtime>(app: &AppHandle<R>) -> TeamsRecapSyncState {
    let Ok(store) = app.store(SYNC_STORE_FILE) else {
        return TeamsRecapSyncState::default();
    };
    store
        .get(SYNC_STORE_KEY)
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

/// Load-modify-save under one lock: the sync loop and the settings panel
/// both write, and each save rewrites the whole record.
static STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn update_state<R: Runtime>(
    app: &AppHandle<R>,
    modify: impl FnOnce(&mut TeamsRecapSyncState),
) -> Result<()> {
    let _guard = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut state = load_state(app);
    modify(&mut state);
    let store = app
        .store(SYNC_STORE_FILE)
        .map_err(|e| anyhow!("Could not open the recap sync store: {e}"))?;
    store.set(
        SYNC_STORE_KEY,
        serde_json::to_value(&state).context("Could not serialize recap sync state")?,
    );
    store
        .save()
        .map_err(|e| anyhow!("Could not persist the recap sync store: {e}"))
}

fn load_last_listing<R: Runtime>(app: &AppHandle<R>) -> Option<LastListing> {
    let store = app.store(SYNC_STORE_FILE).ok()?;
    store
        .get(LAST_LISTING_KEY)
        .and_then(|value| serde_json::from_value(value).ok())
}

/// Keep what the list showed for the line beside the buttons. A failure costs
/// only that line, so it is logged instead of failing the check or sync.
fn record_listing<R: Runtime>(app: &AppHandle<R>, last: &LastListing) {
    let saved = (|| -> Result<()> {
        let _guard = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let store = app
            .store(SYNC_STORE_FILE)
            .map_err(|e| anyhow!("Could not open the recap sync store: {e}"))?;
        store.set(
            LAST_LISTING_KEY,
            serde_json::to_value(last).context("Could not serialize the list result")?,
        );
        store
            .save()
            .map_err(|e| anyhow!("Could not persist the recap sync store: {e}"))
    })();
    if let Err(e) = saved {
        warn!("[recap-sync] could not record the list result: {e}");
    }
}

fn parse_since_date(value: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d")
        .map_err(|_| anyhow!("Enter the since date as YYYY-MM-DD."))
}

/// Where this sync starts reading: the previous sync's start minus the
/// overlap, else the chosen since date, else the default backfill.
fn scan_window_start(state: &TeamsRecapSyncState, now: DateTime<Utc>) -> DateTime<Utc> {
    if let Some(last) = state
        .last_sync_date
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
    {
        return last.with_timezone(&Utc) - Duration::days(OVERLAP_DAYS);
    }
    if let Some(date) = state.since_date.as_deref().and_then(|value| parse_since_date(value).ok()) {
        return date.and_hms_opt(0, 0, 0).expect("midnight exists").and_utc();
    }
    now - Duration::days(DEFAULT_BACKFILL_DAYS)
}

fn content_hash(markdown: &str) -> String {
    let digest = Sha256::digest(markdown.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A recap Teams opened without an AI summary may still be in the works, so it
/// stays unrecorded and the next sync retries it. Any other miss failed, and
/// the report says why.
fn missed_outcome(missed: crate::audio::teams_recap::MissedRecap) -> Outcome {
    match missed {
        crate::audio::teams_recap::MissedRecap::NoSummary => Outcome::NotReady,
        missed => Outcome::Failed(missed.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

/// The Recap app can only be read through the Windows WebView2 profile.
pub const fn sync_available() -> bool {
    cfg!(target_os = "windows")
}

// ---------------------------------------------------------------------------
// Sync loop
// ---------------------------------------------------------------------------

static SYNC_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn import_candidate<R: Runtime>(
    app: &AppHandle<R>,
    pool: &sqlx::SqlitePool,
    known: &HashMap<String, ImportedRecap>,
    candidate: &RecapCandidate,
    reader: &mut crate::audio::teams_recap::RecapReader<R>,
) -> Outcome {
    let validated = match crate::audio::teams_recap::validate_recap_url(&candidate.recap_url) {
        Ok(url) => url,
        Err(e) => return Outcome::Failed(e.to_string()),
    };
    let source_url = validated.as_str();
    let key = crate::audio::teams_recap::meeting_identity_key(source_url);
    let recording = crate::audio::teams_recap::recording_metadata(&validated);

    let recap = match reader.read(&validated).await {
        Ok(recap) => recap,
        Err(missed) => return missed_outcome(missed),
    };
    let markdown = crate::summary::commands::recap_markdown_with_source(&recap.markdown, source_url);
    let hash = content_hash(&markdown);
    let seen_before = known.get(&key);
    if seen_before.is_some_and(|entry| entry.content_hash == hash) {
        return Outcome::Skipped;
    }

    let persisted = match crate::summary::commands::persist_recap_from_link(
        app,
        pool,
        candidate.title.clone(),
        source_url,
        recording.as_ref().map(|metadata| metadata.recorded_at.as_str()),
        candidate.started_at.as_deref(),
        &markdown,
    )
    .await
    {
        Ok(persisted) => persisted,
        Err(e) => return Outcome::Failed(e),
    };

    if let Err(e) = update_state(app, |state| {
        state.imported.insert(
            key,
            ImportedRecap {
                meeting_id: persisted.meeting_id.clone(),
                imported_at: Utc::now().to_rfc3339(),
                content_hash: hash,
            },
        );
    }) {
        warn!("[recap-sync] could not record recap import: {e}");
    }
    written_outcome(seen_before.is_some(), &persisted)
}

/// Whether the scan window may move on. A sync that read no recap learned
/// nothing, and may have misread the list, so moving on could lose the rest of
/// the first sync's backfill for good.
fn read_any_recap(report: &RecapSyncReport) -> bool {
    report.new + report.updated + report.skipped > 0
}

/// How a written recap counts: "updated" when it replaced something, whether an
/// earlier copy of itself or the meeting's own summary.
fn written_outcome(
    seen_before: bool,
    persisted: &crate::summary::commands::ImportedRecapFromLinkResponse,
) -> Outcome {
    if seen_before || persisted.replaced_summary {
        Outcome::Updated
    } else {
        Outcome::New { possible_duplicate: persisted.possible_duplicate }
    }
}

impl RecapSyncReport {
    fn tally(&mut self, title: &str, outcome: Outcome) {
        match outcome {
            Outcome::New { possible_duplicate } => {
                self.new += 1;
                if possible_duplicate {
                    self.possible_duplicates.push(title.to_owned());
                }
            }
            Outcome::Updated => self.updated += 1,
            Outcome::Skipped => self.skipped += 1,
            Outcome::NotReady => self.not_ready += 1,
            Outcome::Failed(error) => {
                warn!("[recap-sync] {title}: {error}");
                self.failed.push(RecapSyncFailure { title: title.to_owned(), error });
            }
        }
    }
}

/// Run one sync: list recaps from the scan window, import each, record the
/// window. Re-running converges: every imported recap is in the store
/// before the next one starts, and unchanged recaps are skipped.
pub async fn sync_recaps<R: Runtime>(app: &AppHandle<R>) -> Result<RecapSyncReport, String> {
    let Ok(_running) = SYNC_LOCK.try_lock() else {
        return Err("A Teams recap sync is already running.".into());
    };
    let vault_path = crate::summary::commands::require_obsidian_vault(app).await?;
    let meetings_folder = crate::obsidian::meetings_path(&vault_path)
        .to_string_lossy()
        .to_string();

    let started_at = Utc::now();
    let state = load_state(app);
    let since = scan_window_start(&state, started_at);
    info!("[recap-sync] listing recaps since {}", since.to_rfc3339());
    let listing = super::teams_recap_list::list_recaps(app, since, false)
        .await
        .map_err(|e| e.to_string())?;
    record_listing(app, &LastListing::from_listing(Utc::now(), since, &listing));
    if let Some(error) = &listing.error {
        return Err(error.clone());
    }

    let app_state = app.state::<crate::state::AppState>();
    let pool = app_state.db_manager.pool();
    let mut report = RecapSyncReport {
        scanned: listing.candidates.len() + listing.rows_without_link,
        // A listed meeting with no recap link yet is retried next sync.
        not_ready: listing.rows_without_link,
        meetings_folder,
        ..Default::default()
    };
    // Oldest first so a sync interrupted midway leaves a contiguous prefix.
    let mut ordered = listing.candidates;
    ordered.sort_by(|a, b| a.started_at.cmp(&b.started_at));
    let total = ordered.len();
    if total > 0 {
        // One hidden window reads every recap, so Teams starts once per sync
        // instead of once per recap. It closes when the loop ends.
        let mut reader = crate::audio::teams_recap::RecapReader::open(app)
            .await
            .map_err(|e| format!("Could not open the Teams window for the recaps: {e}"))?;
        for (done, candidate) in ordered.iter().enumerate() {
            let _ = app.emit(
                PROGRESS_EVENT,
                SyncProgress { done, total, current_title: &candidate.title },
            );
            let outcome = import_candidate(app, pool, &state.imported, candidate, &mut reader).await;
            report.tally(&candidate.title, outcome);
        }
    }
    let _ = app.emit(PROGRESS_EVENT, SyncProgress { done: total, total, current_title: "" });

    if !read_any_recap(&report) {
        info!("[recap-sync] no recap was read, so the scan window stays where it was");
    } else if let Err(e) = update_state(app, |state| state.last_sync_date = Some(started_at.to_rfc3339())) {
        warn!("[recap-sync] could not persist last sync date: {e}");
    }
    info!(
        "[recap-sync] scanned {} new {} updated {} skipped {} not ready {} failed {}",
        report.scanned, report.new, report.updated, report.skipped, report.not_ready, report.failed.len()
    );
    Ok(report)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn sync_teams_recaps_command<R: Runtime>(app: AppHandle<R>) -> Result<RecapSyncReport, String> {
    sync_recaps(&app).await
}

#[tauri::command]
pub async fn get_teams_recap_sync_state_command<R: Runtime>(
    app: AppHandle<R>,
) -> Result<TeamsRecapSyncStateView, String> {
    let state = load_state(&app);
    let last_listing = load_last_listing(&app).map(|last| last.view(&state.imported));
    Ok(TeamsRecapSyncStateView {
        since_date: state.since_date,
        last_sync_date: state.last_sync_date,
        known_recaps: state.imported.len(),
        available: sync_available(),
        last_listing,
    })
}

#[tauri::command]
pub async fn set_teams_recap_sync_prefs_command<R: Runtime>(
    app: AppHandle<R>,
    since_date: Option<String>,
) -> Result<(), String> {
    let since_date = match since_date.as_deref().map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => Some(parse_since_date(value).map_err(|e| e.to_string())?.to_string()),
        None => None,
    };
    update_state(&app, |state| state.since_date = since_date).map_err(|e| e.to_string())
}

/// The raw candidate list, without importing anything. For checking the
/// listing against Teams and for the QA doc. Like a sync, it records what the
/// list showed for the line beside the buttons.
#[tauri::command]
pub async fn teams_recap_list_debug_command<R: Runtime>(
    app: AppHandle<R>,
) -> Result<RecapListDebug, String> {
    let Ok(_running) = SYNC_LOCK.try_lock() else {
        return Err("A Teams recap sync is already running.".into());
    };
    let state = load_state(&app);
    let since = scan_window_start(&state, Utc::now());
    let listing = super::teams_recap_list::list_recaps(&app, since, true)
        .await
        .map_err(|e| e.to_string())?;
    record_listing(&app, &LastListing::from_listing(Utc::now(), since, &listing));
    let probe_file = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Could not resolve app data directory: {e}"))?
        .join(PROBE_FILE);
    let json = serde_json::to_string_pretty(&listing).map_err(|e| e.to_string())?;
    std::fs::write(&probe_file, json)
        .map_err(|e| format!("Could not write {}: {e}", probe_file.display()))?;
    info!(
        "[recap-sync] list check: {} recaps, {} rows without a link, probe saved",
        listing.candidates.len(),
        listing.rows_without_link
    );
    Ok(RecapListDebug {
        candidates: listing.candidates,
        rows_without_link: listing.rows_without_link,
        page_url: listing.page_url,
        error: listing.error,
        probe_file: probe_file.to_string_lossy().to_string(),
    })
}

/// Written next to the app's data by the list check, for reading the Meet
/// app's layout on the machine that ran it. It holds meeting titles and page
/// text, so it stays local.
const PROBE_FILE: &str = "teams_recap_list_probe.json";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecapListDebug {
    pub candidates: Vec<RecapCandidate>,
    pub rows_without_link: usize,
    pub page_url: Option<String>,
    pub error: Option<String>,
    pub probe_file: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn first_sync_defaults_to_thirty_days_back() {
        let now = at("2026-11-02T10:00:00Z");
        assert_eq!(scan_window_start(&TeamsRecapSyncState::default(), now), at("2026-10-03T10:00:00Z"));
    }

    #[test]
    fn chosen_since_date_wins_until_a_sync_has_run() {
        let state = TeamsRecapSyncState { since_date: Some("2026-09-01".into()), ..Default::default() };
        assert_eq!(scan_window_start(&state, at("2026-11-02T10:00:00Z")), at("2026-09-01T00:00:00Z"));
        let bad = TeamsRecapSyncState { since_date: Some("September".into()), ..Default::default() };
        assert_eq!(scan_window_start(&bad, at("2026-11-02T10:00:00Z")), at("2026-10-03T10:00:00Z"));
    }

    #[test]
    fn later_syncs_overlap_the_previous_window() {
        let state = TeamsRecapSyncState {
            since_date: Some("2026-01-01".into()),
            last_sync_date: Some("2026-10-30T08:00:00+00:00".into()),
            ..Default::default()
        };
        assert_eq!(scan_window_start(&state, at("2026-11-02T10:00:00Z")), at("2026-10-23T08:00:00Z"));
    }

    #[test]
    fn since_date_must_be_a_calendar_date() {
        assert_eq!(parse_since_date(" 2026-02-28 ").unwrap().to_string(), "2026-02-28");
        assert!(parse_since_date("2026-02-30").is_err());
        assert!(parse_since_date("28/02/2026").is_err());
    }

    #[test]
    fn unchanged_markdown_hashes_the_same_and_edits_do_not() {
        assert_eq!(content_hash("- a\n"), content_hash("- a\n"));
        assert_ne!(content_hash("- a\n"), content_hash("- b\n"));
        assert_eq!(content_hash("").len(), 64);
    }

    #[test]
    fn only_a_recap_that_opened_without_an_ai_summary_counts_as_not_ready() {
        use crate::audio::teams_recap::MissedRecap;
        assert_eq!(missed_outcome(MissedRecap::NoSummary), Outcome::NotReady);
        // On 2026-11-06 all 30 recaps of a sync timed out before Teams had
        // opened them, and the report called every one "not ready".
        assert_eq!(
            missed_outcome(MissedRecap::NotOpened { waited: std::time::Duration::from_secs(60) }),
            Outcome::Failed("Teams did not open this recap within 60 seconds.".into())
        );
        assert_eq!(
            missed_outcome(MissedRecap::Failed(anyhow!("Teams stayed on its app-choice page."))),
            Outcome::Failed("Teams stayed on its app-choice page.".into())
        );
    }

    #[test]
    fn the_scan_window_moves_only_when_a_recap_was_read() {
        let mut report = RecapSyncReport { scanned: 40, not_ready: 40, ..Default::default() };
        assert!(!read_any_recap(&report), "rows without a link prove nothing about the list");
        report.failed.push(RecapSyncFailure { title: "T".into(), error: "boom".into() });
        assert!(!read_any_recap(&report));
        assert!(!read_any_recap(&RecapSyncReport::default()));
        for read in [
            RecapSyncReport { new: 1, ..Default::default() },
            RecapSyncReport { updated: 1, ..Default::default() },
            RecapSyncReport { skipped: 1, ..Default::default() },
        ] {
            assert!(read_any_recap(&read));
        }
    }

    fn written(replaced_summary: bool, possible_duplicate: bool) -> crate::summary::commands::ImportedRecapFromLinkResponse {
        crate::summary::commands::ImportedRecapFromLinkResponse {
            meeting_id: "m1".into(),
            title: "T".into(),
            obsidian_file_path: "V/Meetings/T.md".into(),
            attached: replaced_summary,
            replaced_summary,
            possible_duplicate,
        }
    }

    #[test]
    fn a_written_recap_counts_as_updated_when_it_replaced_something() {
        // A brand new meeting, flagged or not.
        assert_eq!(written_outcome(false, &written(false, false)), Outcome::New { possible_duplicate: false });
        assert_eq!(written_outcome(false, &written(false, true)), Outcome::New { possible_duplicate: true });
        // An earlier copy of this recap, or a live recording's own summary.
        assert_eq!(written_outcome(true, &written(false, false)), Outcome::Updated);
        assert_eq!(written_outcome(false, &written(true, false)), Outcome::Updated);
    }

    #[test]
    fn the_report_counts_every_outcome_and_names_possible_duplicates() {
        let mut report = RecapSyncReport::default();
        report.tally("A", Outcome::New { possible_duplicate: false });
        report.tally("B", Outcome::New { possible_duplicate: true });
        report.tally("C", Outcome::Updated);
        report.tally("D", Outcome::Skipped);
        report.tally("E", Outcome::NotReady);
        report.tally("F", Outcome::Failed("boom".into()));
        assert_eq!((report.new, report.updated, report.skipped, report.not_ready), (2, 1, 1, 1));
        assert_eq!(report.possible_duplicates, vec!["B".to_string()]);
        assert_eq!(report.failed[0].title, "F");
        assert_eq!(report.failed[0].error, "boom");
    }

    fn recap_link(file: &str) -> String {
        format!(
            "https://teams.microsoft.com/l/meetingrecap?threadId=19%3Aa&fileUrl=https%3A%2F%2Fcontoso-my.sharepoint.com%2Fpersonal%2Fjdoe%2FDocuments%2FRecordings%2F{file}-Meeting%20Recording.mp4"
        )
    }

    /// The key the sync records an import under.
    fn synced_key(link: &str) -> String {
        crate::audio::teams_recap::meeting_identity_key(
            crate::audio::teams_recap::validate_recap_url(link).unwrap().as_str(),
        )
    }

    fn listing_of(links: &[String]) -> crate::audio::teams_recap_list::RecapListing {
        crate::audio::teams_recap_list::RecapListing {
            candidates: links
                .iter()
                .map(|link| RecapCandidate { recap_url: link.clone(), title: "T".into(), started_at: None })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn the_last_listing_counts_its_recaps_against_the_synced_ones() {
        let links = [recap_link("Standup-20261103_090000"), recap_link("Review-20261104_090000"), recap_link("Sync-20261105_090000")];
        let mut listing = listing_of(&links);
        listing.rows_without_link = 2;
        let last = LastListing::from_listing(at("2026-11-06T18:40:20Z"), at("2026-10-29T01:16:21Z"), &listing);
        let mut imported = HashMap::new();
        imported.insert(synced_key(&links[0]), ImportedRecap::default());
        let view = last.view(&imported);
        assert_eq!((view.listed, view.synced, view.pending, view.not_ready), (3, 1, 2, 2));
        assert_eq!((view.read_at.as_str(), view.window_start.as_str()), ("2026-11-06T18:40:20+00:00", "2026-10-29T01:16:21+00:00"));
        assert_eq!((view.error, view.ended_early_at), (None, None));

        // A sync after the check moves its recaps from pending to synced.
        imported.insert(synced_key(&links[2]), ImportedRecap::default());
        let view = last.view(&imported);
        assert_eq!((view.synced, view.pending), (2, 1));
    }

    #[test]
    fn a_listed_link_that_fails_validation_stays_pending() {
        let link = "https://example.com/l/meetingrecap?threadId=1".to_string();
        let last = LastListing::from_listing(at("2026-11-06T18:40:20Z"), at("2026-10-29T00:00:00Z"), &listing_of(&[link.clone()]));
        assert!(last.keys.is_empty());
        let mut imported = HashMap::new();
        imported.insert(crate::audio::teams_recap::meeting_identity_key(&link), ImportedRecap::default());
        let view = last.view(&imported);
        assert_eq!((view.listed, view.synced, view.pending), (1, 0, 1));
    }

    #[test]
    fn a_list_that_stopped_before_the_window_start_says_how_far_back_it_got() {
        use crate::audio::teams_recap_list::ListEnd;
        let oldest = "2026-11-05T15:00:00.000Z";
        for (end, expected) in [
            (Some(ListEnd::Idle), Some(oldest)),
            (Some(ListEnd::Cap), Some(oldest)),
            (Some(ListEnd::Since), None),
            (Some(ListEnd::Empty), None),
            (None, None),
        ] {
            let mut listing = listing_of(&[recap_link("Sync-20261105_090000")]);
            listing.end = end;
            listing.oldest_row = Some(oldest.into());
            let last = LastListing::from_listing(at("2026-11-06T17:04:15Z"), at("2026-10-29T01:16:21Z"), &listing);
            assert_eq!(last.ended_early_at.as_deref(), expected, "{end:?}");
        }
    }

    #[test]
    fn a_list_that_was_not_read_keeps_the_reason() {
        let mut listing = listing_of(&[]);
        listing.error = Some("Teams did not show the Meeting Recaps list.".into());
        let view = LastListing::from_listing(at("2026-11-06T17:04:15Z"), at("2026-10-29T01:16:21Z"), &listing)
            .view(&HashMap::new());
        assert_eq!(view.error.as_deref(), Some("Teams did not show the Meeting Recaps list."));
        assert_eq!((view.listed, view.synced, view.pending), (0, 0, 0));
    }

    #[test]
    fn the_last_listing_round_trips_through_json() {
        let last = LastListing::from_listing(
            at("2026-11-06T18:40:20Z"),
            at("2026-10-29T01:16:21Z"),
            &listing_of(&[recap_link("Sync-20261105_090000")]),
        );
        let json = serde_json::to_value(&last).unwrap();
        assert_eq!(json["readAt"], "2026-11-06T18:40:20+00:00");
        assert_eq!(json["keys"].as_array().unwrap().len(), 1);
        assert_eq!(serde_json::from_value::<LastListing>(json).unwrap(), last);
    }

    #[test]
    fn state_round_trips_through_json() {
        let mut state = TeamsRecapSyncState::default();
        state.imported.insert(
            "host/path.mp4".into(),
            ImportedRecap { meeting_id: "m1".into(), imported_at: "2026-11-02T10:00:00+00:00".into(), content_hash: "ab".into() },
        );
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["imported"]["host/path.mp4"]["meetingId"], "m1");
        let back: TeamsRecapSyncState = serde_json::from_value(json).unwrap();
        assert_eq!(back.imported["host/path.mp4"].content_hash, "ab");
        let empty: TeamsRecapSyncState = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(empty.imported.is_empty());
    }
}
