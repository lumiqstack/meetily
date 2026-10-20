import { useState, useCallback } from 'react';
import { useTauriEvent } from '@/hooks/useTauriEvent';
import { toast } from 'sonner';
import { TranscriptModelProps } from '@/components/TranscriptSettings';
import type { TranscriptionErrorPayload } from '@/services/transcriptService';
import type { TranscriptChunkLossPayload } from '@/services/recordingService';

export type ModalType =
  | 'modelSettings'
  | 'deviceSettings'
  | 'languageSettings'
  | 'modelSelector'
  | 'errorAlert'
  | 'chunkDropWarning';

interface ModalState {
  modelSettings: boolean;
  deviceSettings: boolean;
  languageSettings: boolean;
  modelSelector: boolean;
  errorAlert: boolean;
  chunkDropWarning: boolean;
}

interface ModalMessages {
  errorAlert: string;
  chunkDropWarning: string;
  modelSelector: string;
}

interface UseModalStateReturn {
  modals: ModalState;
  messages: ModalMessages;
  showModal: (name: ModalType, message?: string) => void;
  hideModal: (name: ModalType) => void;
  hideAllModals: () => void;
}

/**
 * Custom hook for managing all modal state and event listeners.
 * Consolidates 9 useState calls and 3 event listeners from page.tsx.
 *
 * Features:
 * - Unified modal state management
 * - Event listeners for chunk drops, transcription errors, model downloads
 * - Auto-close on model download completion
 */
export function useModalState(transcriptModelConfig?: TranscriptModelProps): UseModalStateReturn {
  // Modal visibility state
  const [modals, setModals] = useState<ModalState>({
    modelSettings: false,
    deviceSettings: false,
    languageSettings: false,
    modelSelector: false,
    errorAlert: false,
    chunkDropWarning: false,
  });

  // Modal messages
  const [messages, setMessages] = useState<ModalMessages>({
    errorAlert: '',
    chunkDropWarning: '',
    modelSelector: '',
  });

  // Show modal with optional message
  const showModal = useCallback((name: ModalType, message?: string) => {
    setModals(prev => ({ ...prev, [name]: true }));

    // Set message if provided
    if (message && (name === 'errorAlert' || name === 'chunkDropWarning' || name === 'modelSelector')) {
      setMessages(prev => ({ ...prev, [name]: message }));
    }
  }, []);

  // Hide modal and clear its message
  const hideModal = useCallback((name: ModalType) => {
    setModals(prev => ({ ...prev, [name]: false }));

    // Clear message when closing
    if (name === 'errorAlert' || name === 'chunkDropWarning' || name === 'modelSelector') {
      setMessages(prev => ({ ...prev, [name]: '' }));
    }
  }, []);

  // Hide all modals
  const hideAllModals = useCallback(() => {
    setModals({
      modelSettings: false,
      deviceSettings: false,
      languageSettings: false,
      modelSelector: false,
      errorAlert: false,
      chunkDropWarning: false,
    });
    setMessages({
      errorAlert: '',
      chunkDropWarning: '',
      modelSelector: '',
    });
  }, []);

  useTauriEvent<TranscriptChunkLossPayload>('transcript-chunk-loss-detected', (payload) => {
    console.log('Transcript chunk loss detected');
    const { message, chunks_lost, chunks_queued } = payload;
    showModal('chunkDropWarning', `${message} (${chunks_lost} of ${chunks_queued} chunks lost).`);
  });

  // Model loading failures
  useTauriEvent<TranscriptionErrorPayload>('transcription-error', (payload) => {
    console.log('Transcription error received');
    const { userMessage, actionable } = payload;

    if (actionable) {
      // This is a model-related error that requires user action
      showModal('modelSelector', userMessage);
    } else {
      // Show toast instead of modal for non-actionable errors (consistent with sidebar)
      toast.error('', {
        description: userMessage,
        duration: 5000,
      });
    }
  });

  // Auto-close the model selector once the selected Whisper model finishes downloading
  useTauriEvent<{ modelName: string }>('model-download-complete', ({ modelName }) => {
    console.log('[useModalState] Whisper model download complete:', modelName);

    if (transcriptModelConfig?.provider === 'localWhisper' && transcriptModelConfig?.model === modelName) {
      toast.success('Model ready! Closing window...', { duration: 1500 });
      setTimeout(() => hideModal('modelSelector'), 1500);
    }
  });

  return {
    modals,
    messages,
    showModal,
    hideModal,
    hideAllModals,
  };
}
