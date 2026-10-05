-- User-assigned tags for a meeting. Exported to the Obsidian note's front
-- matter alongside the fixed `meetings` tag. Stored normalized (see
-- `MeetingTagsRepository::normalize_tag`), one row per tag.
CREATE TABLE IF NOT EXISTS meeting_tags (
    meeting_id TEXT NOT NULL,
    tag        TEXT NOT NULL,
    PRIMARY KEY (meeting_id, tag)
);
CREATE INDEX IF NOT EXISTS idx_meeting_tags_tag ON meeting_tags(tag);
