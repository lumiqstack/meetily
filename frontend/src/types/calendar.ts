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

// Mirrors database::repositories::meeting_calendar::MeetingCalendar: the event
// a saved meeting was recorded against.
export interface MeetingCalendar {
  source: 'ics_feed' | 'outlook_com';
  event_id: string;
  start: string;
  end: string;
  organizer: string | null;
  attendees: string[];
  join_url: string | null;
  body: string | null;
}
