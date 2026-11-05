-- The subject of the Outlook event a recording was matched to. meetings.title
-- does not keep it: generating a summary replaces that with the summary's first
-- heading. A Teams recap or recording is named after the subject, so this is
-- what they are compared with (docs/TEAMS_RECAP_SYNC_SPEC.md). Older rows stay
-- NULL and fall back to meetings.title.
ALTER TABLE meetings ADD COLUMN calendar_title TEXT;
