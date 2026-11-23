"use client";

import { useState, useCallback, useEffect } from 'react';
import { Button } from '@/components/ui/button';
import { ButtonGroup } from '@/components/ui/button-group';
import { Copy, FolderOpen, HardDrive, RefreshCw, Users } from 'lucide-react';
import { toast } from 'sonner';
import Analytics from '@/lib/analytics';
import { RetranscribeDialog } from './RetranscribeDialog';
import { useConfig } from '@/contexts/ConfigContext';
import { SpeakersDialog } from './SpeakersDialog';
import type { RenameSpeaker } from './SpeakerRenamePopover';
import type { MeetingSpeaker } from '@/lib/speaker-names';
import { useMeetingAudio } from '@/hooks/useMeetingAudio';
import { formatBytes } from '@/lib/recordings';
import { PurgeConfirmDialog } from '@/components/Storage/PurgeConfirmDialog';


interface BackgroundRetranscriptionCompleteDetail {
  meetingId: string;
}

interface TranscriptButtonGroupProps {
  transcriptCount: number;
  onCopyTranscript: () => void;
  onOpenMeetingFolder: () => Promise<void>;
  meetingId?: string;
  meetingFolderPath?: string | null;
  onRefetchTranscripts?: () => Promise<void>;
  speakers?: readonly MeetingSpeaker[];
  onRenameSpeaker?: RenameSpeaker;
}


export function TranscriptButtonGroup({
  transcriptCount,
  onCopyTranscript,
  onOpenMeetingFolder,
  meetingId,
  meetingFolderPath,
  onRefetchTranscripts,
  speakers = [],
  onRenameSpeaker,
}: TranscriptButtonGroupProps) {
  const { betaFeatures } = useConfig();
  const [showRetranscribeDialog, setShowRetranscribeDialog] = useState(false);
  const [showSpeakersDialog, setShowSpeakersDialog] = useState(false);
  const [confirmingRemove, setConfirmingRemove] = useState(false);
  const canNameSpeakers = !!onRenameSpeaker && speakers.length > 0;
  const { audio, removeAudio } = useMeetingAudio(meetingFolderPath ? meetingId : undefined);
  const enhanceBlocked = audio?.state === 'absent' ? audio.reason.text : null;
  const removeBlocked = audio?.state === 'held' ? audio.reason.text : null;

  const handleRetranscribeComplete = useCallback(async () => {
    // Refetch transcripts to show the updated data
    if (onRefetchTranscripts) {
      await onRefetchTranscripts();
    }
  }, [onRefetchTranscripts]);

  useEffect(() => {
    if (!meetingId) return;

    const handleBackgroundRetranscriptionComplete = (event: Event) => {
      const detail = (event as CustomEvent<BackgroundRetranscriptionCompleteDetail>).detail;
      if (detail?.meetingId !== meetingId) return;

      handleRetranscribeComplete().catch((error) => {
        console.error('Failed to refresh transcripts after background retranscription:', error);
      });
    };

    window.addEventListener('meetily-background-retranscription-complete', handleBackgroundRetranscriptionComplete);

    return () => {
      window.removeEventListener('meetily-background-retranscription-complete', handleBackgroundRetranscriptionComplete);
    };
  }, [meetingId, handleRetranscribeComplete]);

  return (
    <div className="flex items-center justify-center w-full gap-2">
      <ButtonGroup>
        <Button
          variant="outline"
          size="sm"
          className="px-2 @[22rem]:px-3"
          onClick={() => {
            Analytics.trackButtonClick('copy_transcript', 'meeting_details');
            onCopyTranscript();
          }}
          disabled={transcriptCount === 0}
          title={transcriptCount === 0 ? 'No transcript available' : 'Copy Transcript'}
        >
          <Copy />
          <span className="hidden @[22rem]:inline">Copy</span>
        </Button>

        <Button
          size="sm"
          variant="outline"
          className="px-2 @[22rem]:px-4"
          disabled={!meetingFolderPath}
          onClick={() => {
            Analytics.trackButtonClick('open_recording_folder', 'meeting_details');
            onOpenMeetingFolder();
          }}
          title={meetingFolderPath ? 'Open Recording Folder' : 'No recording folder available'}
        >
          <FolderOpen className="@[22rem]:mr-2" size={18} />
          <span className="hidden @[22rem]:inline">Recording</span>
        </Button>

        {canNameSpeakers && (
          <Button
            size="sm"
            variant="outline"
            className="px-2 @[22rem]:px-4"
            onClick={() => {
              Analytics.trackButtonClick('name_speakers', 'meeting_details');
              setShowSpeakersDialog(true);
            }}
            title="Name the speakers in this meeting"
          >
            <Users className="@[22rem]:mr-2" size={18} />
            <span className="hidden @[22rem]:inline">Speakers</span>
          </Button>
        )}

        {betaFeatures.importAndRetranscribe && meetingId && meetingFolderPath && (
          <Button
            size="sm"
            variant="outline"
            className={`bg-gradient-to-r from-blue-50 to-purple-50 hover:from-blue-100 hover:to-purple-100 border-blue-200 px-2 @[22rem]:px-4 ${enhanceBlocked ? 'opacity-50 cursor-not-allowed' : ''}`}
            aria-disabled={!!enhanceBlocked}
            onClick={() => {
              // aria-disabled rather than disabled, so the reason stays readable on hover.
              if (enhanceBlocked) {
                toast.info(enhanceBlocked);
                return;
              }
              Analytics.trackButtonClick('enhance_transcript', 'meeting_details');
              setShowRetranscribeDialog(true);
            }}
            title={enhanceBlocked ?? 'Retranscribe to enhance your recorded audio'}
          >
            <RefreshCw className="@[22rem]:mr-2" size={18} />
            <span className="hidden @[22rem]:inline">Enhance</span>
          </Button>
        )}

        {audio && audio.state !== 'absent' && (
          <Button
            size="sm"
            variant="outline"
            className={`px-2 @[22rem]:px-4 ${removeBlocked ? 'opacity-50 cursor-not-allowed' : ''}`}
            aria-disabled={!!removeBlocked}
            onClick={() => {
              if (removeBlocked) {
                toast.info(removeBlocked);
                return;
              }
              setConfirmingRemove(true);
            }}
            title={removeBlocked ?? `Remove this meeting's audio and free ${formatBytes(audio.bytes)}`}
          >
            <HardDrive className="@[22rem]:mr-2" size={18} />
            <span className="hidden @[22rem]:inline">Remove audio</span>
          </Button>
        )}
      </ButtonGroup>

      {canNameSpeakers && onRenameSpeaker && (
        <SpeakersDialog
          open={showSpeakersDialog}
          onOpenChange={setShowSpeakersDialog}
          speakers={speakers}
          onRename={onRenameSpeaker}
        />
      )}

      {audio?.state === 'removable' && (
        <PurgeConfirmDialog
          open={confirmingRemove}
          summary={{ meetings: 1, orphans: 0, bytes: audio.bytes }}
          onCancel={() => setConfirmingRemove(false)}
          onConfirm={async () => {
            await removeAudio();
            setConfirmingRemove(false);
          }}
        />
      )}

      {betaFeatures.importAndRetranscribe && meetingId && meetingFolderPath && (
        <RetranscribeDialog
          open={showRetranscribeDialog}
          onOpenChange={setShowRetranscribeDialog}
          meetingId={meetingId}
          meetingFolderPath={meetingFolderPath}
          onComplete={handleRetranscribeComplete}
        />
      )}
    </div>
  );
}
