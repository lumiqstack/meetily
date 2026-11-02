"use client";

import { useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { toast } from 'sonner';
import { ChevronDown, ChevronRight, ExternalLink, Users } from 'lucide-react';
import type { MeetingCalendar } from '@/types/calendar';

const ATTENDEES_SHOWN_COLLAPSED = 6;

function formatScheduled(start: string, end: string): string | null {
  const startDate = new Date(start);
  const endDate = new Date(end);
  if (Number.isNaN(startDate.getTime()) || Number.isNaN(endDate.getTime())) return null;
  const day = startDate.toLocaleDateString(undefined, { weekday: 'short', month: 'short', day: 'numeric' });
  const time = (date: Date) => date.toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit' });
  return `${day}, ${time(startDate)} to ${time(endDate)}`;
}

/**
 * The Outlook invitation a recording was matched to: who organized it, who
 * was invited, the join link and the invitation text. Shown under the meeting
 * meta line; the body is collapsed by default since invitations run long.
 */
export function CalendarDetails({ calendar }: { calendar: MeetingCalendar }) {
  const [showAllAttendees, setShowAllAttendees] = useState(false);
  const [showBody, setShowBody] = useState(false);

  const scheduled = formatScheduled(calendar.start, calendar.end);
  const attendees = calendar.attendees.filter((name) => name.trim().length > 0);
  const hiddenAttendees = Math.max(0, attendees.length - ATTENDEES_SHOWN_COLLAPSED);
  const visibleAttendees = showAllAttendees ? attendees : attendees.slice(0, ATTENDEES_SHOWN_COLLAPSED);
  const body = calendar.body?.trim() || null;

  const openJoinLink = async () => {
    if (!calendar.join_url) return;
    try {
      await invoke('open_teams_join_url', { url: calendar.join_url });
    } catch (error) {
      toast.error('Could not open the Teams link', {
        description: error instanceof Error ? error.message : String(error),
      });
    }
  };

  return (
    <div
      className="mt-2 mx-1 rounded-md border border-gray-200 bg-gray-50 px-3 py-2 text-xs text-gray-600"
      data-testid="calendar-details"
    >
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
        <span className="font-medium text-gray-700">Outlook invitation</span>
        {scheduled && <span>{scheduled}</span>}
        {calendar.organizer && <span>Organized by {calendar.organizer}</span>}
        {calendar.join_url && (
          <button
            type="button"
            onClick={openJoinLink}
            className="inline-flex items-center gap-1 text-blue-700 hover:underline"
          >
            <ExternalLink size={12} />
            Open in Teams
          </button>
        )}
      </div>

      {attendees.length > 0 && (
        <div className="mt-1.5 flex flex-wrap items-center gap-x-1.5 gap-y-1">
          <Users size={12} className="text-gray-400 shrink-0" />
          <span>{visibleAttendees.join(', ')}</span>
          {hiddenAttendees > 0 && !showAllAttendees && (
            <button
              type="button"
              onClick={() => setShowAllAttendees(true)}
              className="text-blue-700 hover:underline"
            >
              and {hiddenAttendees} more
            </button>
          )}
        </div>
      )}

      {body && (
        <div className="mt-1.5">
          <button
            type="button"
            onClick={() => setShowBody((open) => !open)}
            className="inline-flex items-center gap-1 text-gray-700 hover:underline"
            aria-expanded={showBody}
          >
            {showBody ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
            Invitation text
          </button>
          {showBody && (
            <pre className="mt-1 max-h-64 overflow-auto whitespace-pre-wrap break-words rounded bg-white p-2 font-sans text-xs text-gray-700 border border-gray-200">
              {body}
            </pre>
          )}
        </div>
      )}
    </div>
  );
}
