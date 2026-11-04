-- Outlook calendar event a recording was matched to (docs/CALENDAR_MEETING_DETECTION_SPEC.md).
-- All nullable: meetings started without a match keep NULLs.
ALTER TABLE meetings ADD COLUMN calendar_source TEXT;        -- 'ics_feed' | 'outlook_com'
ALTER TABLE meetings ADD COLUMN calendar_event_id TEXT;
ALTER TABLE meetings ADD COLUMN calendar_start TEXT;         -- RFC 3339 UTC
ALTER TABLE meetings ADD COLUMN calendar_end TEXT;
ALTER TABLE meetings ADD COLUMN calendar_organizer TEXT;
ALTER TABLE meetings ADD COLUMN calendar_attendees TEXT;     -- JSON array of strings
ALTER TABLE meetings ADD COLUMN calendar_join_url TEXT;
ALTER TABLE meetings ADD COLUMN calendar_body TEXT;
