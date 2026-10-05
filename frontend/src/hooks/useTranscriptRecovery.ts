/**
 * useTranscriptRecovery Hook
 *
 * Orchestrates transcript recovery operations for interrupted meetings.
 * Provides functionality to detect, preview, and recover meetings from IndexedDB.
 */

import { useState, useCallback } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { indexedDBService, MeetingMetadata, StoredTranscript } from '@/services/indexedDBService';
import { storageService } from '@/services/storageService';
import { applyPinnedSummaryLanguageToMeeting } from '@/lib/summary-language-preferences';
import { backgroundJobStore } from '@/components/shared/BackgroundJobToast';
import { useConfig } from '@/contexts/ConfigContext';
import { toast } from 'sonner';
import { BATCH_CAPABLE_PROVIDERS } from '@/lib/transcription-providers';

interface AudioRecoveryStatus {
  status: string; // "success" | "partial" | "failed" | "none"
  chunk_count: number;
  estimated_duration_seconds: number;
  audio_file_path?: string;
  message: string;
}

export interface RecoveryResult {
  success: boolean;
  audioRecoveryStatus?: AudioRecoveryStatus | null;
  meetingId?: string;
  retranscriptionStarted?: boolean;
  retranscriptionSkippedReason?: string;
}

export interface UseTranscriptRecoveryReturn {
  recoverableMeetings: MeetingMetadata[];
  isLoading: boolean;
  isRecovering: boolean;
  checkForRecoverableTranscripts: () => Promise<void>;
  recoverMeeting: (meetingId: string) => Promise<RecoveryResult>;
  loadMeetingTranscripts: (meetingId: string) => Promise<StoredTranscript[]>;
  deleteRecoverableMeeting: (meetingId: string) => Promise<void>;
}

export function useTranscriptRecovery(): UseTranscriptRecoveryReturn {
  const [recoverableMeetings, setRecoverableMeetings] = useState<MeetingMetadata[]>([]);
  const [isLoading, setIsLoading] = useState(false);
  const [isRecovering, setIsRecovering] = useState(false);
  const { transcriptModelConfig, selectedLanguage } = useConfig();

  /**
   * Check for recoverable meetings in IndexedDB
   */
  const checkForRecoverableTranscripts = useCallback(async () => {
    setIsLoading(true);
    try {
      const meetings = await indexedDBService.getAllMeetings();

      // Filter out meetings older than 7 days and newer than 15 seconds
      // The 15 seconds threshold prevents showing meetings from the current session(jus in case)
      // where recording just stopped but hasn't been fully saved yet
      const cutoffTime = Date.now() - (7 * 24 * 60 * 60 * 1000);
      const secondsAgo = Date.now() - (2 * 1000);

      const recentMeetings = meetings.filter(m => {
        const isWithinRetention = m.lastUpdated > cutoffTime; // Not older than 7 days
        const isOldEnough = m.lastUpdated < secondsAgo; // Older than 15 seconds
        return isWithinRetention && isOldEnough;
      });

      // Verify audio checkpoint availability for each meeting
      const meetingsWithAudioStatus = await Promise.all(
        recentMeetings.map(async (meeting) => {
          if (meeting.folderPath) {
            try {
              const hasAudio = await invoke<boolean>('has_audio_checkpoints', {
                meetingFolder: meeting.folderPath
              });

              // If no audio files, clear folderPath to show "No audio" in UI
              return {
                ...meeting,
                folderPath: hasAudio ? meeting.folderPath : undefined
              };
            } catch (error) {
              console.warn('Failed to check audio for meeting:', error);
              // On error, assume no audio to be safe
              return { ...meeting, folderPath: undefined };
            }
          }
          return meeting;
        })
      );


      setRecoverableMeetings(meetingsWithAudioStatus);
    } catch (error) {
      console.error('Failed to check for recoverable transcripts:', error);
      setRecoverableMeetings([]);
    } finally {
      setIsLoading(false);
    }
  }, []);

  /**
   * Load transcripts for preview
   */
  const loadMeetingTranscripts = useCallback(async (meetingId: string): Promise<StoredTranscript[]> => {
    try {
      const transcripts = await indexedDBService.getTranscripts(meetingId);
      // Sort by sequence ID
      transcripts.sort((a, b) => (a.sequenceId || 0) - (b.sequenceId || 0));
      return transcripts;
    } catch (error) {
      console.error('Failed to load meeting transcripts:', error);
      return [];
    }
  }, []);

  /**
   * Recover a meeting from IndexedDB
   */
  const recoverMeeting = useCallback(async (meetingId: string): Promise<RecoveryResult> => {
    setIsRecovering(true);
    try {
      // 1. Load meeting metadata
      const metadata = await indexedDBService.getMeetingMetadata(meetingId);
      if (!metadata) {
        throw new Error('Meeting metadata not found');
      }

      // 2. Load all transcripts
      const transcripts = await loadMeetingTranscripts(meetingId);

      // 3. Check for folder path
      let folderPath = metadata.folderPath;


      if (!folderPath) {
        // Try to get from backend (might exist if only app crashed, not system)
        try {
          folderPath = await invoke<string>('get_meeting_folder_path');
        } catch (error) {
          folderPath = undefined;
        }
      }

      // Audio-only path: nothing was transcribed before the interruption, but
      // audio checkpoints may still exist on disk.
      if (transcripts.length === 0) {
        if (!folderPath) {
          throw new Error('No transcripts and no audio were saved for this meeting, so there is nothing to recover.');
        }

        // Merge checkpoints into audio.mp4. The command resolves even on
        // failure, so the returned status must be checked explicitly.
        const audioRecoveryStatus = await invoke<AudioRecoveryStatus>(
          'recover_audio_from_checkpoints',
          { meetingFolder: folderPath, sampleRate: 48000 }
        );
        if (audioRecoveryStatus.status !== 'success') {
          throw new Error(`No transcripts were saved and audio recovery failed: ${audioRecoveryStatus.message}`);
        }

        // Create the meeting with an empty transcript list so retranscription
        // has a meeting row to attach to.
        const saveResponse = await storageService.saveMeeting(metadata.title, [], folderPath);
        const savedMeetingId = saveResponse.meeting_id;

        try {
          await applyPinnedSummaryLanguageToMeeting(savedMeetingId);
        } catch (error) {
          console.warn('Failed to apply pinned summary language to recovered meeting:', error);
          toast.warning('Could not apply default summary language', {
            description: 'The recovered meeting was saved, but the default summary language was not applied.',
          });
        }

        await indexedDBService.markMeetingSaved(meetingId);

        // Retranscription reads the merged audio.mp4, not the checkpoints, so
        // cleaning up now is safe.
        try {
          await invoke('cleanup_checkpoints', { meetingFolder: folderPath });
        } catch (error) {
          console.warn('Checkpoint cleanup failed (non-fatal):', error);
        }

        setRecoverableMeetings(prev => prev.filter(m => m.meetingId !== meetingId));

        // Kick off background re-transcription of the recovered audio.
        // Failure here is non-fatal: the audio and meeting are already saved
        // and the user can retranscribe manually from the meeting page.
        let retranscriptionStarted = false;
        let retranscriptionSkippedReason: string | undefined;

        if (!BATCH_CAPABLE_PROVIDERS.has(transcriptModelConfig.provider)) {
          retranscriptionSkippedReason = `Provider "${transcriptModelConfig.provider}" cannot re-transcribe saved audio. Open the meeting and use "Retranscribe" to pick a supported model.`;
        } else {
          const isParakeet = transcriptModelConfig.provider === 'parakeet';
          const language = isParakeet || !selectedLanguage || selectedLanguage === 'auto' ? null : selectedLanguage;
          try {
            // Register the job toast before invoking so no progress event is missed
            window.dispatchEvent(new CustomEvent('meetily-background-retranscription-started', {
              detail: { meetingId: savedMeetingId, title: metadata.title },
            }));
            await invoke('start_retranscription_command', {
              meetingId: savedMeetingId,
              meetingFolderPath: folderPath,
              language,
              model: transcriptModelConfig.model || null,
              provider: transcriptModelConfig.provider,
              // Recovery is unattended, so it does not opt into diarization.
              diarization: false,
            });
            retranscriptionStarted = true;
          } catch (error) {
            backgroundJobStore.remove(savedMeetingId);
            retranscriptionSkippedReason = error instanceof Error ? error.message : String(error);
          }
        }

        return {
          success: true,
          audioRecoveryStatus,
          meetingId: savedMeetingId,
          retranscriptionStarted,
          retranscriptionSkippedReason,
        };
      }

      // 4. Attempt audio recovery if folder path exists
      let audioRecoveryStatus: AudioRecoveryStatus | null = null;
      if (folderPath) {
        try {
          audioRecoveryStatus = await invoke<AudioRecoveryStatus>(
            'recover_audio_from_checkpoints',
            { meetingFolder: folderPath, sampleRate: 48000 }
          );
        } catch (error) {
          console.error('Audio recovery failed:', error);
          audioRecoveryStatus = {
            status: 'failed',
            chunk_count: 0,
            estimated_duration_seconds: 0,
            message: error instanceof Error ? error.message : 'Unknown error'
          };
        }
      } else {
        audioRecoveryStatus = {
          status: 'none',
          chunk_count: 0,
          estimated_duration_seconds: 0,
          message: 'No folder path available'
        };
      }

      // 5. Convert StoredTranscripts to the format expected by storageService
      const formattedTranscripts = transcripts.map((t, index) => ({
        id: t.id?.toString() || `${Date.now()}-${index}`,
        text: t.text,
        timestamp: t.timestamp,
        sequence_id: t.sequenceId || index,
        chunk_start_time: (t as any).chunk_start_time,
        is_partial: (t as any).is_partial || false,
        confidence: t.confidence,
        audio_start_time: (t as any).audio_start_time,
        audio_end_time: (t as any).audio_end_time,
        duration: (t as any).duration,
      }));

      // 6. Save to backend database using existing save utilities
      const saveResponse = await storageService.saveMeeting(
        metadata.title,
        formattedTranscripts,
        folderPath ?? null
      );

      const savedMeetingId = saveResponse.meeting_id;

      try {
        await applyPinnedSummaryLanguageToMeeting(savedMeetingId);
      } catch (error) {
        console.warn('Failed to apply pinned summary language to recovered meeting:', error);
        toast.warning('Could not apply default summary language', {
          description: 'The recovered meeting was saved, but the default summary language was not applied.',
        });
      }

      // 7. Mark as saved in IndexedDB
      await indexedDBService.markMeetingSaved(meetingId);


      // 8. Clean up checkpoint files
      if (folderPath) {
        try {
          await invoke('cleanup_checkpoints', { meetingFolder: folderPath });
        } catch (error) {
          // Non-fatal - don't fail recovery if cleanup fails
          console.warn('Checkpoint cleanup failed (non-fatal):', error);
        }
      }

      // 9. Remove from recoverable list
      setRecoverableMeetings(prev => prev.filter(m => m.meetingId !== meetingId));

      return {
        success: true,
        audioRecoveryStatus,
        meetingId: savedMeetingId
      };
    } catch (error) {
      console.error('Failed to recover meeting:', error);
      throw error;
    } finally {
      setIsRecovering(false);
    }
  }, [loadMeetingTranscripts, transcriptModelConfig, selectedLanguage]);

  /**
   * Delete a recoverable meeting
   */
  const deleteRecoverableMeeting = useCallback(async (meetingId: string): Promise<void> => {
    try {
      await indexedDBService.deleteMeeting(meetingId);
      setRecoverableMeetings(prev => prev.filter(m => m.meetingId !== meetingId));
    } catch (error) {
      console.error('Failed to delete meeting:', error);
      throw error;
    }
  }, []);

  return {
    recoverableMeetings,
    isLoading,
    isRecovering,
    checkForRecoverableTranscripts,
    recoverMeeting,
    loadMeetingTranscripts,
    deleteRecoverableMeeting
  };
}
