import { VirtualizedTranscriptView } from '@/components/VirtualizedTranscriptView';
import { PermissionWarning } from '@/components/PermissionWarning';
import { BackgroundJobsPanel } from '@/components/shared/BackgroundJobsPanel';
import { PendingWorkPanel } from '@/components/shared/PendingWorkPanel';
import { Button } from '@/components/ui/button';
import { ButtonGroup } from '@/components/ui/button-group';
import { Copy, GlobeIcon } from 'lucide-react';
import { useTranscripts } from '@/contexts/TranscriptContext';
import { useConfig } from '@/contexts/ConfigContext';
import { useRecordingState } from '@/contexts/RecordingStateContext';
import { usePermissionCheck } from '@/hooks/usePermissionCheck';
import { ModalType } from '@/hooks/useModalState';
import { useIsLinux } from '@/hooks/usePlatform';
import { useMemo } from 'react';

/**
 * TranscriptPanel Component
 *
 * Displays transcript content with controls for copying and language settings.
 * Uses TranscriptContext, ConfigContext, and RecordingStateContext internally.
 */

interface TranscriptPanelProps {
  // indicates stop-processing state for transcripts; derived from backend statuses.
  isProcessingStop: boolean;
  isStopping: boolean;
  showModal: (name: ModalType, message?: string) => void;
}

export function TranscriptPanel({
  isProcessingStop,
  isStopping,
  showModal
}: TranscriptPanelProps) {
  // Contexts
  const { transcripts, transcriptContainerRef, copyTranscript, liveCaption } = useTranscripts();
  const { transcriptModelConfig } = useConfig();
  const { isRecording, isPaused } = useRecordingState();
  const { checkPermissions, isChecking, hasSystemAudio, hasMicrophone } = usePermissionCheck();
  const isLinux = useIsLinux();

  // Convert transcripts to segments for virtualized view
  const segments = useMemo(() =>
    transcripts.map(t => ({
      id: t.id,
      timestamp: t.audio_start_time ?? 0,
      endTime: t.audio_end_time,
      text: t.text,
      confidence: t.confidence,
      speaker: t.speaker,
    })),
    [transcripts]
  );

  return (
    <div ref={transcriptContainerRef} className="w-full border-r border-gray-200 bg-white flex flex-col overflow-hidden">
      {/* Title area - the transcript list below owns the only scroller, so this
          stays put without needing to be sticky */}
      <div className="flex-shrink-0 bg-white p-4 border-gray-200">
        <div className="flex flex-col space-y-3">
          <div className="flex  flex-col space-y-2">
            <div className="flex justify-center  items-center space-x-2">
              <ButtonGroup>
                {transcripts?.length > 0 && (
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={copyTranscript}
                    title="Copy Transcript"
                  >
                    <Copy />
                    <span className='hidden md:inline'>
                      Copy
                    </span>
                  </Button>
                )}
                {transcriptModelConfig.provider === "localWhisper" &&
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => showModal('languageSettings')}
                    title="Language"
                  >
                    <GlobeIcon />
                    <span className='hidden md:inline'>
                      Language
                    </span>
                  </Button>
                }
              </ButtonGroup>
            </div>
          </div>
        </div>
      </div>

      {/* Permission Warning - Not needed on Linux */}
      {!isRecording && !isChecking && !isLinux && (
        <div className="flex-shrink-0 flex justify-center px-4 pt-4">
          <PermissionWarning
            hasMicrophone={hasMicrophone}
            hasSystemAudio={hasSystemAudio}
            onRecheck={checkPermissions}
            isRechecking={isChecking}
          />
        </div>
      )}

      {/* Transcript content - the inner wrapper must have a bounded height or
          the virtualizer inside it renders every row */}
      <div className="flex flex-1 min-h-0 justify-center">
        <div className="w-2/3 max-w-[750px] overflow-hidden flex flex-col">
          <VirtualizedTranscriptView
            segments={segments}
            isRecording={isRecording}
            isPaused={isPaused}
            isProcessing={isProcessingStop}
            isStopping={isStopping}
            enableStreaming={isRecording}
            showConfidence={true}
          />
          {/* Live hypothesis from a streaming provider. Rendered outside the
              virtualized list because it is replaced in place and never
              persisted — putting it in `segments` would break that list's
              sequence-id ordering and dedupe. */}
          {isRecording && liveCaption && (
            <p
              aria-live="polite"
              className="flex-shrink-0 px-2 py-2 text-sm italic text-gray-400"
            >
              {liveCaption}
            </p>
          )}
        </div>
      </div>

      {/* Capped so a long pending list can't squeeze the transcript out;
          pb-20 keeps the floating recording controls off the content. Both
          panels render null when they have nothing, so the region collapses
          rather than reserving its padding against an empty box. */}
      <div className="flex-shrink-0 max-h-[40%] overflow-y-auto pb-20 empty:hidden">
        {/* Background remote imports/retranscriptions in progress */}
        <BackgroundJobsPanel />

        {/* Meetings still needing a transcript or AI summary */}
        {!isRecording && <PendingWorkPanel />}
      </div>
    </div>
  );
}
