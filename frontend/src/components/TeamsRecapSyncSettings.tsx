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
}

/** Mirrors `RecapSyncReport` in src-tauri/src/audio/teams_recap_sync.rs. */
interface RecapSyncReport {
  scanned: number;
  new: number;
  updated: number;
  skipped: number;
  notReady: number;
  failed: { title: string; error: string }[];
  meetingsFolder: string;
}

interface SyncProgress {
  done: number;
  total: number;
  currentTitle: string;
}

/** One line of counts for the finished-sync toast. */
export function describeSyncReport(report: RecapSyncReport): string {
  if (report.scanned === 0) {
    return 'No recaps listed in the scan window.';
  }
  const parts = [
    `${report.new} new`,
    `${report.updated} updated`,
    `${report.skipped} unchanged`,
  ];
  if (report.notReady > 0) parts.push(`${report.notReady} not ready yet`);
  if (report.failed.length > 0) parts.push(`${report.failed.length} failed`);
  return `${report.scanned} scanned: ${parts.join(', ')}.`;
}

/** Sync Teams recaps into Meetily and the vault. Lives inside the Obsidian settings block. */
export function TeamsRecapSyncSettings({ vaultConfigured }: { vaultConfigured: boolean }) {
  const platform = usePlatform();
  const [state, setState] = useState<TeamsRecapSyncStateView | null>(null);
  const [sinceDate, setSinceDate] = useState("");
  const [isSyncing, setIsSyncing] = useState(false);
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
    setProgress(null);
    try {
      const report = await invoke<RecapSyncReport>('sync_teams_recaps_command');
      const description = describeSyncReport(report);
      if (report.failed.length > 0) {
        toast.warning('Teams recap sync finished with failures', {
          description: `${description} First failure: ${report.failed[0].title}: ${report.failed[0].error}`,
        });
      } else {
        toast.success('Teams recap sync finished', { description });
      }
      await loadState();
    } catch (error) {
      toast.error('Teams recap sync failed', { description: String(error) });
    } finally {
      setIsSyncing(false);
      setProgress(null);
    }
  };

  const lastSync = state?.lastSyncDate ? new Date(state.lastSyncDate).toLocaleString() : 'never';

  return (
    <div className="mt-3 pt-3 border-t border-gray-200 flex flex-col gap-2">
      <div className="text-sm font-medium">Teams recaps</div>
      <div className="text-xs text-gray-600">
        Reads the recap list in the Teams Meet app and saves every Copilot recap to Meetily and the
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
          disabled={!available || isSyncing}
          className="flex items-center gap-2 px-3 py-2 text-sm border border-gray-300 rounded-md hover:bg-gray-100 transition-colors disabled:opacity-60"
        >
          <RefreshCw className={`w-4 h-4 ${isSyncing ? 'animate-spin' : ''}`} />
          {isSyncing ? 'Syncing...' : 'Sync Teams recaps'}
        </button>
        {isSyncing && progress && progress.total > 0 && (
          <span className="text-xs text-gray-600">
            {progress.done}/{progress.total}{progress.currentTitle ? ` · ${progress.currentTitle}` : ''}
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
