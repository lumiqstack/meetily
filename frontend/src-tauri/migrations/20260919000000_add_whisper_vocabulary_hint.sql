-- Contextual vocabulary for local whisper-rs transcription. Existing users
-- receive the same seed terms as new users and may edit or clear them later.
ALTER TABLE transcript_settings
ADD COLUMN whisperVocabularyHint TEXT NOT NULL DEFAULT 'Murex, Banamex, Azteca, Zeinab, Oropeza, Pasquel, MFFX, bpv, cámara';
