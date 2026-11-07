"use client"

import { useEffect, useState } from "react"
import { RefreshCw } from "lucide-react"
import { invoke } from "@tauri-apps/api/core"
import { listen } from "@tauri-apps/api/event"
import { toast } from "sonner"
import { usePlatform } from "@/hooks/usePlatform"

/** Mirrors `TeamsRecapSyncStateView` in src-tauri/src/audio/teams_recap_sync.rs. */
interface TeamsRecapSyncStateView {
  sinceDate: string | null;
  lastSyncDate: string | null;
  knownRecaps: number;
  available: boolean;
  lastListing: LastListingView | null;
}

/** Mirrors `LastListingView` in src-tauri/src/audio/teams_recap_sync.rs. */
export interface LastListingView {
  readAt: string;
  windowStart: string;
  listed: number;
  synced: number;
  pending: number;
  notReady: number;
  error: string | null;
  endedEarlyAt: string | null;
}

/** Mirrors `RecapSyncReport` in src-tauri/src/audio/teams_recap_sync.rs. */
interface RecapSyncReport {
  scanned: number;
  new: number;
  updated: number;
  skipped: number;
  kept: number;
  notReady: number;
  failed: { title: string; error: string }[];
  possibleDuplicates: string[];
  meetingsFolder: string;
}

interface SyncProgress {
  done: number;
  total: number;
  currentTitle: string;
}

/**
 * One line of counts for the finished-sync toast. A recap the sync kept
 * without opening counts as unchanged: a sync read it a day or more after its
 * meeting.
 */
export function describeSyncReport(report: RecapSyncReport): string {
  if (report.scanned === 0) {
    return 'No recaps listed in the scan window.';
  }
  const parts = [
    `${report.new} new`,
    `${report.updated} updated`,
    `${report.skipped + report.kept} unchanged`,
  ];
  if (report.notReady > 0) parts.push(`${report.notReady} not ready yet`);
  if (report.failed.length > 0) parts.push(`${report.failed.length} failed`);
  const counts = `${report.scanned} scanned: ${parts.join(', ')}.`;
  if (report.possibleDuplicates.length === 0) return counts;
  const [first, ...others] = report.possibleDuplicates;
  const named = others.length > 0 ? `"${first}" and ${others.length} more` : `"${first}"`;
  return `${counts} ${named} may duplicate a live recording: saved separately because nothing confirmed the match.`;
}

/** `locale` and `timeZone` default to the system's; tests pin them. */
interface DateOptions {
  now?: Date;
  locale?: string;
  timeZone?: string;
}

/**
 * The line beside the buttons: when the list was last read, by a check or a
 * sync, and how its recaps stand against the ones already synced.
 */
export function describeLastListing(
  last: LastListingView,
  { now = new Date(), locale, timeZone }: DateOptions = {},
): string {
  const day = (value: string | Date) =>
    new Date(value).toLocaleDateString(locale, { timeZone, month: 'short', day: 'numeric' });
  const calendarDay = (value: Date) =>
    value.toLocaleDateString(locale, { timeZone, year: 'numeric', month: 'numeric', day: 'numeric' });
  const readAt = new Date(last.readAt);
  const time = readAt.toLocaleTimeString(locale, { timeZone, hour: 'numeric', minute: '2-digit' });
  const when = calendarDay(readAt) === calendarDay(now) ? `Checked at ${time}` : `Checked ${day(readAt)} at ${time}`;

  if (last.error) return `${when}: the list was not read. ${last.error}`;
  if (last.listed === 0 && last.notReady === 0) return `${when}: no recaps since ${day(last.windowStart)}.`;
  const parts = [
    `${last.listed} ${last.listed === 1 ? 'recap' : 'recaps'}`,
    `${last.synced} already synced`,
    `${last.pending} pending`,
  ];
  if (last.notReady > 0) parts.push(`${last.notReady} without a recap yet`);
  const counts = `${when}: ${parts.join(', ')}.`;
  if (!last.endedEarlyAt) return counts;
  return `${counts} The list stopped at ${day(last.endedEarlyAt)}, before ${day(last.windowStart)}, so it may be missing older recaps. Check again.`;
}

/** Sync Teams recaps into Meetily and the vault. Lives inside the Obsidian settings block. */
export function TeamsRecapSyncSettings({ vaultConfigured }: { vaultConfigured: boolean }) {
  const platform = usePlatform();
  const [state, setState] = useState<TeamsRecapSyncStateView | null>(null);
  const [sinceDate, setSinceDate] = useState("");
  const [isSyncing, setIsSyncing] = useState(false);
  const [isChecking, setIsChecking] = useState(false);
  const [checkError, setCheckError] = useState<string | null>(null);
  const [progress, setProgress] = useState<SyncProgress | null>(null);

  const loadState = async () => {
    try {
      const view = await invoke<TeamsRecapSyncStateView>('get_teams_recap_sync_state_command');
      setState(view);
      setSinceDate(view.sinceDate ?? "");
    } catch (error) {
      console.error('Failed to load Teams recap sync state:', error);
    }
  };

  useEffect(() => {
    loadState();
    const unlisten = listen<SyncProgress>('teams-recap-sync-progress', (event) => {
      setProgress(event.payload);
    });
    return () => {
      unlisten.then((stop) => stop()).catch(() => undefined);
    };
  }, []);

  const available = (state?.available ?? platform === 'windows') && vaultConfigured;
  const busy = isSyncing || isChecking;

  const handleSinceDateBlur = async () => {
    if (sinceDate === (state?.sinceDate ?? "")) return;
    try {
      await invoke('set_teams_recap_sync_prefs_command', { sinceDate: sinceDate || null });
      await loadState();
    } catch (error) {
      toast.error('Could not save the since date', { description: String(error) });
      setSinceDate(state?.sinceDate ?? "");
    }
  };

  const handleSync = async () => {
    setIsSyncing(true);
    setCheckError(null);
    setProgress(null);
    try {
      const report = await invoke<RecapSyncReport>('sync_teams_recaps_command');
      const description = describeSyncReport(report);
      if (report.failed.length > 0) {
        toast.warning('Teams recap sync finished with failures', {
          description: `${description} First failure: ${report.failed[0].title}: ${report.failed[0].error}`,
        });
      } else if (report.possibleDuplicates.length > 0) {
        toast.warning('Teams recap sync finished, check possible duplicates', { description });
      } else {
        toast.success('Teams recap sync finished', { description });
      }
    } catch (error) {
      toast.error('Teams recap sync failed', { description: String(error) });
    } finally {
      // A sync that failed on the list still recorded it for the line.
      await loadState();
      setIsSyncing(false);
      setProgress(null);
    }
  };

  // The result shows on the line beside the buttons, which the state reload
  // fills in. Only a check that never reached the list (a sync running, no
  // WebView2) has nothing stored, so its error shows there instead.
  const handleCheckList = async () => {
    setIsChecking(true);
    setCheckError(null);
    try {
      await invoke('teams_recap_list_debug_command');
    } catch (error) {
      setCheckError(String(error));
    } finally {
      await loadState();
      setIsChecking(false);
    }
  };

  const lastSync = state?.lastSyncDate ? new Date(state.lastSyncDate).toLocaleString() : 'never';
  const lastListing = state?.lastListing ?? null;
  const listingLine = checkError
    ? `Check failed: ${checkError}`
    : lastListing ? describeLastListing(lastListing) : null;
  const listingTone = checkError || lastListing?.error
    ? 'text-red-700'
    : lastListing?.endedEarlyAt ? 'text-amber-700' : 'text-gray-600';

  return (
    <div className="mt-3 pt-3 border-t border-gray-200 flex flex-col gap-2">
      <div className="text-sm font-medium">Teams recaps</div>
      <div className="text-xs text-gray-600">
        Reads Meeting Recaps in Teams and saves every Copilot recap to Meetily and the
        vault, attaching each to the meeting that already holds its recording. Last sync: {lastSync}
        {state && state.knownRecaps > 0 ? ` · ${state.knownRecaps} recaps known` : ''}.
      </div>
      {!state?.lastSyncDate && (
        <label className="flex items-center gap-2 text-sm text-gray-900">
          <span>Start from</span>
          <input
            type="date"
            value={sinceDate}
            onChange={(event) => setSinceDate(event.target.value)}
            onBlur={handleSinceDateBlur}
            className="rounded-md border border-gray-300 bg-white px-2 py-1 text-sm text-gray-900"
          />
          <span className="text-xs text-gray-600">Leave empty for the last 30 days.</span>
        </label>
      )}
      <div className="flex items-center gap-3">
        <button
          onClick={handleSync}
          disabled={!available || busy}
          className="flex items-center gap-2 px-3 py-2 text-sm border border-gray-300 rounded-md hover:bg-gray-100 transition-colors disabled:opacity-60"
        >
          <RefreshCw className={`w-4 h-4 ${isSyncing ? 'animate-spin' : ''}`} />
          {isSyncing ? 'Syncing...' : 'Sync Teams recaps'}
        </button>
        <button
          onClick={handleCheckList}
          disabled={!available || busy}
          title="Reads the recap list without importing anything and saves what the page showed next to Meetily's data"
          className="px-3 py-2 text-sm border border-gray-300 rounded-md hover:bg-gray-100 transition-colors disabled:opacity-60"
        >
          {isChecking ? 'Checking...' : 'Check recap list'}
        </button>
        {isSyncing && progress && progress.total > 0 && (
          <span className="text-xs text-gray-600">
            {progress.done}/{progress.total}{progress.currentTitle ? ` · ${progress.currentTitle}` : ''}
          </span>
        )}
        {listingLine && (
          <span className={`min-w-0 flex-1 text-xs ${listingTone}`} data-testid="recap-list-result">
            {listingLine}
          </span>
        )}
        {!vaultConfigured && (
          <span className="text-xs text-gray-600">Set an Obsidian vault first.</span>
        )}
        {vaultConfigured && state && !state.available && (
          <span className="text-xs text-gray-600">Available on Windows.</span>
        )}
      </div>
    </div>
  );
}
