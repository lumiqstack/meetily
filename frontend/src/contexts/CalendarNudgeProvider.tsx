'use client';

import React, { useEffect, useRef } from 'react';
import { listen } from '@tauri-apps/api/event';
import { toast } from 'sonner';
import { useConfig } from '@/contexts/ConfigContext';
import { useRecordingState } from '@/contexts/RecordingStateContext';
import Analytics from '@/lib/analytics';

// Mirrors calendar::types::CalendarEvent in the Rust core.
export interface CalendarEvent {
  id: string;
  source: 'ics_feed' | 'outlook_com';
  title: string;
  start: string;
  end: string;
  organizer: string | null;
  attendees: string[];
  location: string | null;
  body: string | null;
  teams_join_url: string | null;
  is_recurring_instance: boolean;
}

interface CalendarMeetingUpcomingPayload {
  type: string;
  event: CalendarEvent;
  lead_minutes: number;
  starts_in_seconds: number;
}

const UPCOMING_EVENT = 'calendar-meeting-upcoming';

const isTauriRuntime = () => (
  typeof window !== 'undefined' &&
  '__TAURI_INTERNALS__' in window
);

function describeStart(startsInSeconds: number): string {
  if (startsInSeconds <= 30) return 'starts now';
  const minutes = Math.ceil(startsInSeconds / 60);
  return `starts in ${minutes} ${minutes === 1 ? 'minute' : 'minutes'}`;
}

/**
 * Shows the in-app nudge for an upcoming Teams meeting from the Outlook
 * calendar. The system notification is raised by the Rust core; this toast
 * is the one with buttons. "Record now" reuses the sidebar's auto-start path
 * and hands over the meeting title.
 */
export function CalendarNudgeProvider({ children }: { children: React.ReactNode }) {
  const recordingState = useRecordingState();
  const { calendarSettings } = useConfig();

  const recordingStateRef = useRef(recordingState);
  const settingsRef = useRef(calendarSettings);

  useEffect(() => {
    recordingStateRef.current = recordingState;
  }, [recordingState]);

  useEffect(() => {
    settingsRef.current = calendarSettings;
  }, [calendarSettings]);

  useEffect(() => {
    if (!isTauriRuntime()) return;

    let unlisten: (() => void) | null = null;
    let cleanedUp = false;

    listen<CalendarMeetingUpcomingPayload>(UPCOMING_EVENT, (event) => {
      showNudge(event.payload);
    }).then((fn) => {
      if (cleanedUp) {
        fn();
      } else {
        unlisten = fn;
      }
    }).catch((error) => {
      console.error('[CalendarNudge] Failed to listen for nudges:', error);
    });

    return () => {
      cleanedUp = true;
      unlisten?.();
    };
  }, []);

  const showNudge = (payload: CalendarMeetingUpcomingPayload) => {
    // The backend only emits while enabled, but settings can change between
    // the emit and the render.
    if (settingsRef.current && !settingsRef.current.calendar_enabled) return;
    const current = recordingStateRef.current;
    if (current.isRecording || current.isStopping) return;

    const { event, starts_in_seconds } = payload;
    let toastId: string | number;
    toastId = toast.info(`${event.title} ${describeStart(starts_in_seconds)}`, {
      description: (
        <div className="space-y-3 min-w-[280px]">
          <p className="text-sm text-gray-700">
            {event.organizer ? `Organized by ${event.organizer}. ` : ''}
            Record this Teams meeting with Meetily?
          </p>
          <div className="flex flex-wrap gap-2">
            <button
              onClick={() => {
                toast.dismiss(toastId);
                Analytics.trackButtonClick('calendar_nudge_record', 'calendar_nudge');
                sessionStorage.setItem('autoStartMeetingName', event.title);
                sessionStorage.setItem('autoStartRecording', 'true');
                window.location.assign('/');
              }}
              className="px-3 py-1.5 text-xs font-medium rounded bg-gray-900 text-white hover:bg-gray-800"
            >
              Record now
            </button>
            <button
              onClick={() => {
                toast.dismiss(toastId);
                Analytics.trackButtonClick('calendar_nudge_dismissed', 'calendar_nudge');
              }}
              className="px-3 py-1.5 text-xs font-medium rounded border border-gray-300 text-gray-700 hover:bg-gray-50"
            >
              Dismiss
            </button>
          </div>
        </div>
      ),
      duration: Math.max(15000, (starts_in_seconds + 120) * 1000),
    });

    console.log('[CalendarNudge] Nudge displayed for', event.id);
  };

  return <>{children}</>;
}
