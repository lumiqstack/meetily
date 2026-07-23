-- URL-sourced imports (audio or Teams-transcript mode) journal the original
-- link and mode so an interrupted job can be retried faithfully; file imports
-- leave both NULL. Nullable so existing rows survive unchanged.
ALTER TABLE background_jobs ADD COLUMN source_url TEXT;
ALTER TABLE background_jobs ADD COLUMN mode TEXT;
