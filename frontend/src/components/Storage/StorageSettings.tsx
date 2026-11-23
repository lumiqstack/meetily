"use client";

import { useCallback, useEffect, useRef, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { ChevronDown, ChevronRight, RefreshCw } from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { PurgeConfirmDialog } from './PurgeConfirmDialog';
import {
  EMPTY_SELECTION,
  audioBytes,
  formatBytes,
  purgeRecordings,
  purgeResultText,
  selectOlderThan,
  summarizeSelection,
  surveyRecordings,
  toggle,
  type MeetingAudio,
  type OrphanFolder,
  type Selection,
  type Survey,
} from '@/lib/recordings';

const AGE_CHOICES = [30, 90, 180, 365] as const;

function formatDate(iso: string): string {
  const date = new Date(iso);
  return Number.isNaN(date.getTime()) ? '' : date.toLocaleDateString();
}

function MeetingRow({ row, selected, onChange }: { row: MeetingAudio; selected: boolean; onChange: (on: boolean) => void }) {
  const held = row.audio.state === 'held' ? row.audio.reason : null;
  return (
    <li className="flex items-center gap-3 px-4 py-2">
      {held ? (
        <span className="w-4" />
      ) : (
        <input
          type="checkbox"
          className="h-4 w-4"
          aria-label={`Select ${row.title}`}
          checked={selected}
          onChange={(e) => onChange(e.target.checked)}
        />
      )}
      <div className="flex-1 min-w-0">
        <div className="truncate text-sm font-medium">{row.title}</div>
        <div className="text-xs text-gray-500">
          {formatDate(row.created_at)}
          {held && <span data-reason className="ml-2 text-amber-700">{held.text}</span>}
        </div>
      </div>
      <div className="text-sm text-gray-600 tabular-nums">{formatBytes(audioBytes(row.audio))}</div>
    </li>
  );
}

function OrphanRow({ row, selected, onChange }: { row: OrphanFolder; selected: boolean; onChange: (on: boolean) => void }) {
  return (
    <li className="flex items-center gap-3 px-4 py-2">
      {row.hold ? (
        <span className="w-4" />
      ) : (
        <input
          type="checkbox"
          className="h-4 w-4"
          aria-label={`Select folder ${row.name}`}
          checked={selected}
          onChange={(e) => onChange(e.target.checked)}
        />
      )}
      <div className="flex-1 min-w-0">
        <div className="truncate text-sm font-medium" title={row.folder}>{row.name}</div>
        <div className="text-xs text-gray-500">
          Changed {formatDate(row.modified_at)}
          {row.hold && <span data-reason className="ml-2 text-amber-700">{row.hold.text}</span>}
        </div>
      </div>
      <div className="text-sm text-gray-600 tabular-nums">{formatBytes(row.bytes)}</div>
    </li>
  );
}

export function StorageSettings() {
  const [survey, setSurvey] = useState<Survey | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [selection, setSelection] = useState<Selection>(EMPTY_SELECTION);
  const [confirming, setConfirming] = useState(false);
  const [showOrphans, setShowOrphans] = useState(false);
  const latest = useRef(0);

  const refresh = useCallback(async () => {
    const request = ++latest.current;
    try {
      const next = await surveyRecordings();
      if (request !== latest.current) return;
      setSurvey(next);
      setError(null);
    } catch (e) {
      if (request === latest.current) setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  useEffect(() => {
    void refresh();
    let unlisten: (() => void) | undefined;
    let disposed = false;
    listen('meetings-changed', () => { void refresh(); })
      .then((fn) => { if (disposed) fn(); else unlisten = fn; })
      .catch((e) => console.error('Failed to listen for meetings-changed:', e));
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [refresh]);

  if (error && !survey) {
    return (
      <div className="p-4 border rounded-lg text-sm text-gray-600">
        {error}
        <Button variant="outline" size="sm" className="ml-3" onClick={() => void refresh()}>Try again</Button>
      </div>
    );
  }

  if (!survey) {
    return (
      <div className="animate-pulse">
        <div className="h-4 bg-gray-200 rounded w-1/4 mb-4"></div>
        <div className="h-8 bg-gray-200 rounded mb-4"></div>
      </div>
    );
  }

  const summary = summarizeSelection(survey, selection);
  const nothingSelected = summary.meetings === 0 && summary.orphans === 0;
  const removableCount = survey.meetings.filter((m) => m.audio.state === 'removable').length;

  const purge = async () => {
    try {
      const report = await purgeRecordings(selection);
      const { title, description } = purgeResultText(report);
      if (report.failed.length > 0) toast.warning(title, { description });
      else toast.success(title, { description });
      setSelection(EMPTY_SELECTION);
    } catch (e) {
      toast.error('Audio was not removed', { description: e instanceof Error ? e.message : String(e) });
    } finally {
      setConfirming(false);
      await refresh();
    }
  };

  return (
    <div className="space-y-6">
      <div>
        <h3 className="text-lg font-semibold mb-4">Storage</h3>
        <p className="text-sm text-gray-600 mb-6">
          Audio recordings take most of the space. Removing a meeting&apos;s audio keeps its transcript and
          summary, but the meeting can no longer be enhanced.
        </p>
      </div>

      <div className="flex items-start justify-between p-4 border rounded-lg bg-gray-50">
        <div>
          <div className="font-medium">Recordings use {formatBytes(survey.audio_bytes)}</div>
          {survey.volumes.length > 0 ? (
            survey.volumes.map((volume) => (
              <div key={volume.mount_point} className="text-sm text-gray-600">
                {formatBytes(volume.free_bytes)} free of {formatBytes(volume.total_bytes)} on {volume.mount_point}
              </div>
            ))
          ) : (
            <div className="text-sm text-gray-600">Free space on the recordings disk could not be read.</div>
          )}
        </div>
        <Button variant="outline" size="sm" onClick={() => void refresh()} title="Look again">
          <RefreshCw />
        </Button>
      </div>

      <div className="space-y-3">
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-sm text-gray-700">Select audio older than</span>
          {AGE_CHOICES.map((days) => (
            <Button
              key={days}
              variant="outline"
              size="sm"
              onClick={() => setSelection(selectOlderThan(survey, days, new Date()))}
            >
              {days} days
            </Button>
          ))}
          <Button variant="ghost" size="sm" disabled={nothingSelected} onClick={() => setSelection(EMPTY_SELECTION)}>
            Clear
          </Button>
        </div>

        <div className="border rounded-lg">
          <div className="px-4 py-2 border-b text-sm font-medium bg-gray-50">
            Meetings with audio on this computer ({survey.meetings.length}, {removableCount} can be removed)
          </div>
          {survey.meetings.length === 0 ? (
            <p className="px-4 py-3 text-sm text-gray-600">No meeting audio is stored on this computer.</p>
          ) : (
            <ul className="divide-y max-h-[50vh] overflow-y-auto">
              {survey.meetings.map((row) => (
                <MeetingRow
                  key={row.meeting_id}
                  row={row}
                  selected={selection.meetingIds.includes(row.meeting_id)}
                  onChange={(on) => setSelection((s) => ({ ...s, meetingIds: toggle(s.meetingIds, row.meeting_id, on) }))}
                />
              ))}
            </ul>
          )}
        </div>

        {survey.orphans.length > 0 && (
          <div className="border rounded-lg">
            <button
              type="button"
              className="flex w-full items-center gap-2 px-4 py-2 text-sm font-medium bg-gray-50 text-left"
              onClick={() => setShowOrphans((v) => !v)}
            >
              {showOrphans ? <ChevronDown className="w-4 h-4" /> : <ChevronRight className="w-4 h-4" />}
              Leftover folders from deleted meetings ({survey.orphans.length})
            </button>
            {showOrphans && (
              <ul className="divide-y border-t max-h-[40vh] overflow-y-auto">
                {survey.orphans.map((row) => (
                  <OrphanRow
                    key={row.folder}
                    row={row}
                    selected={selection.orphanFolders.includes(row.folder)}
                    onChange={(on) => setSelection((s) => ({ ...s, orphanFolders: toggle(s.orphanFolders, row.folder, on) }))}
                  />
                ))}
              </ul>
            )}
          </div>
        )}
      </div>

      <div className="flex justify-end">
        <Button variant="destructive" disabled={nothingSelected} onClick={() => setConfirming(true)}>
          {nothingSelected
            ? 'Remove audio'
            : `Remove selected audio (${summary.meetings + summary.orphans}), frees ${formatBytes(summary.bytes)}`}
        </Button>
      </div>

      <PurgeConfirmDialog
        open={confirming}
        summary={summary}
        onCancel={() => setConfirming(false)}
        onConfirm={purge}
      />
    </div>
  );
}
