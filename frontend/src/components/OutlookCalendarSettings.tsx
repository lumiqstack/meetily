'use client';

import { useCallback, useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { toast } from 'sonner';
import { Switch } from './ui/switch';
import { useConfig, CalendarSettings, DEFAULT_CALENDAR_SETTINGS } from '@/contexts/ConfigContext';
import type { CalendarEvent } from '@/contexts/CalendarNudgeProvider';
import Analytics from '@/lib/analytics';

// Mirrors calendar::types in the Rust core.
type SourceHealth =
  | { state: 'not_configured' }
  | { state: 'ok' }
  | { state: 'unavailable'; reason: string }
  | { state: 'error'; reason: string };

interface SourceStatus {
  kind: 'outlook_com';
  health: SourceHealth;
  last_poll_at: string | null;
  event_count: number;
}

interface CalendarStatus {
  settings: CalendarSettings;
  running: boolean;
  sources: SourceStatus[];
  next_event: CalendarEvent | null;
}

const STATUS_EVENT = 'calendar-status-changed';
const MAX_NUDGE_LEAD_MINUTES = 15;

const isTauriRuntime = () => (
  typeof window !== 'undefined' &&
  '__TAURI_INTERNALS__' in window
);

/** One line under the switch: what the last read of classic Outlook found. */
export function describeHealth(source: SourceStatus | undefined, enabled: boolean): { text: string; tone: 'muted' | 'ok' | 'warn' } {
  if (!enabled) return { text: 'Off. Turn on to read your Outlook calendar.', tone: 'muted' };
  if (!source) return { text: 'Waiting for the first read of Outlook', tone: 'muted' };
  switch (source.health.state) {
    case 'ok': {
      const polled = source.last_poll_at ? new Date(source.last_poll_at).toLocaleTimeString() : 'just now';
      return { text: `Reading classic Outlook. ${source.event_count} events in the next two days, read ${polled}`, tone: 'ok' };
    }
    case 'unavailable':
      return { text: source.health.reason, tone: 'warn' };
    case 'error':
      return { text: `Last read failed: ${source.health.reason}`, tone: 'warn' };
    case 'not_configured':
    default:
      return { text: 'Waiting for the first read of Outlook', tone: 'muted' };
  }
}

export function OutlookCalendarSettings() {
  const { calendarSettings, updateCalendarSettings } = useConfig();
  const settings = calendarSettings ?? DEFAULT_CALENDAR_SETTINGS;

  const [status, setStatus] = useState<CalendarStatus | null>(null);
  const [isRefreshing, setIsRefreshing] = useState(false);

  const loadStatus = useCallback(async () => {
    if (!isTauriRuntime()) return;
    try {
      setStatus(await invoke<CalendarStatus>('get_calendar_status'));
    } catch (error) {
      console.error('[OutlookCalendarSettings] Failed to load status:', error);
    }
  }, []);

  useEffect(() => {
    loadStatus();
    if (!isTauriRuntime()) return;

    let unlisten: (() => void) | null = null;
    let cleanedUp = false;
    listen<CalendarStatus>(STATUS_EVENT, (event) => {
      setStatus(event.payload);
      setIsRefreshing(false);
    })
      .then((fn) => {
        if (cleanedUp) fn(); else unlisten = fn;
      })
      .catch((error) => console.error('[OutlookCalendarSettings] Failed to listen for status:', error));

    return () => {
      cleanedUp = true;
      unlisten?.();
    };
  }, [loadStatus]);

  const patch = async (changes: Partial<CalendarSettings>) => {
    try {
      const next = { ...settings, ...changes };
      await updateCalendarSettings(next);
      await Analytics.track('calendar_settings_changed', {
        calendar_enabled: next.calendar_enabled.toString(),
        nudge_lead_minutes: next.nudge_lead_minutes.toString(),
        include_body_in_summary: next.include_body_in_summary.toString(),
      });
    } catch (error) {
      console.error('Failed to update calendar settings:', error);
      toast.error(error instanceof Error ? error.message : String(error));
    }
  };

  const refreshNow = async () => {
    setIsRefreshing(true);
    try {
      await invoke('refresh_calendar_now');
      // The status event clears the spinner; this is the fallback if a read hangs.
      setTimeout(() => setIsRefreshing(false), 35000);
    } catch (error) {
      setIsRefreshing(false);
      toast.error(error instanceof Error ? error.message : String(error));
    }
  };

  const outlookStatus = status?.sources.find((source) => source.kind === 'outlook_com');
  const health = describeHealth(outlookStatus, settings.calendar_enabled);
  const healthClass = health.tone === 'ok' ? 'text-green-700' : health.tone === 'warn' ? 'text-amber-700' : 'text-gray-500';
  const nextEvent = status?.next_event ?? null;

  return (
    <div className="bg-white rounded-lg border border-gray-200 p-6 shadow-sm">
      <div className="space-y-5">
        <div className="flex items-center justify-between gap-4">
          <div>
            <h3 className="text-lg font-semibold text-gray-900 mb-2">Outlook Calendar</h3>
            <p className="text-sm text-gray-600">
              Get a nudge before each Teams meeting, with the meeting title ready to record
            </p>
          </div>
          <Switch
            checked={settings.calendar_enabled}
            onCheckedChange={(checked) => patch({ calendar_enabled: checked })}
          />
        </div>

        <div className="space-y-4 border-t border-gray-100 pt-4">
          <div>
            <div className="font-medium text-gray-900">Classic Outlook on this PC</div>
            <p className="text-sm text-gray-600 mb-2">
              Meetily reads your calendar from the classic Outlook app on this computer, so nothing
              needs to be shared or published. The new Outlook app cannot be read this way. Invitation
              bodies are not read; the Teams link and the people come from the appointment itself.
            </p>
            <div className="flex flex-wrap items-center gap-2">
              <span className={`text-xs ${healthClass}`}>{health.text}</span>
              <button
                onClick={refreshNow}
                disabled={!settings.calendar_enabled || isRefreshing}
                className="px-3 py-1.5 text-xs font-medium rounded border border-gray-300 text-gray-700 hover:bg-gray-50 disabled:opacity-50"
              >
                {isRefreshing ? 'Reading…' : 'Refresh now'}
              </button>
            </div>
            {settings.calendar_enabled && nextEvent && (
              <p className="text-xs text-gray-600 mt-2">
                Next Teams meeting: {nextEvent.title}, {new Date(nextEvent.start).toLocaleString()}
              </p>
            )}
          </div>

          <div className="flex items-center justify-between gap-4">
            <div>
              <div className="font-medium text-gray-900">Nudge lead time</div>
              <p className="text-sm text-gray-600">Minutes before a Teams meeting starts</p>
            </div>
            <input
              type="number"
              min={0}
              max={MAX_NUDGE_LEAD_MINUTES}
              value={settings.nudge_lead_minutes}
              disabled={!settings.calendar_enabled}
              onChange={(e) => {
                const value = Math.max(0, Math.min(MAX_NUDGE_LEAD_MINUTES, Number(e.target.value) || 0));
                patch({ nudge_lead_minutes: value });
              }}
              className="w-20 px-2 py-1 text-sm border border-gray-300 rounded text-right disabled:opacity-50"
            />
          </div>

          <div className="flex items-center justify-between gap-4">
            <div>
              <div className="font-medium text-gray-900">Use the invitation text in summaries</div>
              <p className="text-sm text-gray-600">
                Send the meeting agenda from the invitation to the summary model along with the transcript.
                With a cloud provider that text leaves this machine.
              </p>
            </div>
            <Switch
              checked={settings.include_body_in_summary}
              disabled={!settings.calendar_enabled}
              onCheckedChange={(checked) => patch({ include_body_in_summary: checked })}
            />
          </div>
        </div>
      </div>
    </div>
  );
}
