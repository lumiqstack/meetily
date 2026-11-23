import { invoke } from '@tauri-apps/api/core';

// Mirrors the wire types in src-tauri/src/recordings/mod.rs. Rust owns every
// reason sentence; the UI shows `text` and never maps codes to words.

export type AbsentCode = 'no_folder' | 'folder_missing' | 'no_audio_files' | 'online_only';
export type HoldCode =
  | 'not_transcribed' | 'transcript_incomplete'
  | 'recording' | 'transcribing' | 'interrupted_job'
  | 'import_running' | 'recently_active'
  | 'shared_folder' | 'unrecognized_folder' | 'online_only' | 'vanished';

export interface AbsentReason { code: AbsentCode; text: string }
export interface HoldReason { code: HoldCode; text: string }

export type AudioStatus =
  | { state: 'absent'; reason: AbsentReason }
  | { state: 'held'; bytes: number; reason: HoldReason }
  | { state: 'removable'; bytes: number };

export interface MeetingAudio { meeting_id: string; title: string; created_at: string; audio: AudioStatus }
export interface OrphanFolder { folder: string; name: string; modified_at: string; bytes: number; hold: HoldReason | null }
export interface Volume { mount_point: string; free_bytes: number; total_bytes: number; audio_bytes: number }
export interface Survey { meetings: MeetingAudio[]; orphans: OrphanFolder[]; volumes: Volume[]; audio_bytes: number }

export interface Selection { meetingIds: string[]; orphanFolders: string[] }
export const EMPTY_SELECTION: Selection = { meetingIds: [], orphanFolders: [] };
export interface SelectionSummary { meetings: number; orphans: number; bytes: number }

export interface PurgeReport {
  freed_bytes: number;
  purged_meetings: string[];
  purged_orphans: number;
  skipped: { target: string; reason: HoldReason }[];
  failed: { target: string; files: string[] }[];
}

export type DeleteMeetingReport =
  | { folder: 'none' }
  | { folder: 'removed'; freed_bytes: number }
  | { folder: 'kept_busy' }
  | { folder: 'kept_shared' }
  | { folder: 'kept_unrecognized' }
  | { folder: 'kept_foreign_files'; freed_bytes: number; path: string }
  | { folder: 'failed'; path: string };

export const surveyRecordings = () => invoke<Survey>('recordings_survey');
export const meetingAudioStatus = (meetingId: string) =>
  invoke<AudioStatus>('recordings_audio_status', { meetingId });
/** Sends ids and orphan lookup keys only; Rust re-checks every target before deleting. */
export const purgeRecordings = (request: Selection) => invoke<PurgeReport>('recordings_purge', { request });

const DAY_MS = 24 * 60 * 60 * 1000;

/** Removable meetings created before `now - days`. Never selects held rows or orphans. */
export function selectOlderThan(survey: Survey, days: number, now: Date): Selection {
  const cutoff = now.getTime() - days * DAY_MS;
  return {
    meetingIds: survey.meetings
      .filter((m) => m.audio.state === 'removable' && Date.parse(m.created_at) < cutoff)
      .map((m) => m.meeting_id),
    orphanFolders: [],
  };
}

/** Counts and bytes of what is selected and still removable in `survey`. */
export function summarizeSelection(survey: Survey, selection: Selection): SelectionSummary {
  const ids = new Set(selection.meetingIds);
  const folders = new Set(selection.orphanFolders);
  const meetings = survey.meetings.filter((m) => ids.has(m.meeting_id) && m.audio.state === 'removable');
  const orphans = survey.orphans.filter((o) => folders.has(o.folder) && !o.hold);
  return {
    meetings: meetings.length,
    orphans: orphans.length,
    bytes: meetings.reduce((sum, m) => sum + audioBytes(m.audio), 0) + orphans.reduce((sum, o) => sum + o.bytes, 0),
  };
}

export function audioBytes(audio: AudioStatus): number {
  return audio.state === 'absent' ? 0 : audio.bytes;
}

export function toggle(list: string[], value: string, on: boolean): string[] {
  const without = list.filter((v) => v !== value);
  return on ? [...without, value] : without;
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
}

function plural(count: number, one: string, many: string): string {
  return `${count} ${count === 1 ? one : many}`;
}

/** The confirmation sentence, built from the summary alone. */
export function purgeConfirmText(summary: SelectionSummary): string {
  const parts: string[] = [];
  if (summary.meetings > 0) parts.push(`the audio of ${plural(summary.meetings, 'meeting', 'meetings')}`);
  if (summary.orphans > 0) parts.push(plural(summary.orphans, 'leftover folder', 'leftover folders'));
  return `Permanently remove ${parts.join(' and ')}? This frees ${formatBytes(summary.bytes)}.`;
}

export function purgeResultText(report: PurgeReport): { title: string; description?: string } {
  const notes: string[] = [];
  if (report.skipped.length > 0) {
    notes.push(`${plural(report.skipped.length, 'item was', 'items were')} kept: ${report.skipped[0].reason.text}`);
  }
  if (report.failed.length > 0) {
    notes.push(`${plural(report.failed.length, 'item', 'items')} could not be fully removed. A file may be open in another app; try again later.`);
  }
  return { title: `Freed ${formatBytes(report.freed_bytes)}`, description: notes.length ? notes.join(' ') : undefined };
}

export function deleteReportText(report: DeleteMeetingReport): string {
  switch (report.folder) {
    case 'none':
      return 'This meeting had no recording folder on this computer.';
    case 'removed':
      return `Recording folder removed, ${formatBytes(report.freed_bytes)} freed.`;
    case 'kept_busy':
      return 'The recording folder is in use and was kept. Settings, Storage lists it so you can remove it later.';
    case 'kept_shared':
      return 'The recording folder is shared with another meeting and was kept.';
    case 'kept_unrecognized':
      return 'The recording folder does not look like a Meetily recording folder and was kept.';
    case 'kept_foreign_files':
      return `Recording removed, ${formatBytes(report.freed_bytes)} freed. The folder holds other files and was kept: ${report.path}`;
    case 'failed':
      return `The recording folder could not be fully removed. A file may be open in another app: ${report.path}`;
  }
}
