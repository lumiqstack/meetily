import React, { useState, useEffect, useCallback, useMemo, useRef } from 'react';
import {
  Upload,
  Globe,
  Link2,
  Loader2,
  AlertCircle,
  CheckCircle2,
  X,
  Cpu,
  FileAudio,
  Clock,
  HardDrive,
  ChevronDown,
  ChevronUp,
} from 'lucide-react';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '../ui/dialog';
import { Button } from '../ui/button';
import { Input } from '../ui/input';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '../ui/select';
import { toast } from 'sonner';
import { useConfig } from '@/contexts/ConfigContext';
import { useImportAudio, ImportResult } from '@/hooks/useImportAudio';
import { useRouter } from 'next/navigation';
import { useSidebar } from '../Sidebar/SidebarProvider';
import { LANGUAGES } from '@/constants/languages';
import { detectLinkContentMode } from '@/lib/link-content-mode';
import { useTranscriptionModels, ModelOption } from '@/hooks/useTranscriptionModels';


interface ImportAudioDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  preselectedFile?: string | null;
  onComplete?: () => void;
}

function formatDuration(seconds: number): string {
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const secs = Math.floor(seconds % 60);

  if (hours > 0) {
    return `${hours}:${minutes.toString().padStart(2, '0')}:${secs.toString().padStart(2, '0')}`;
  }
  return `${minutes}:${secs.toString().padStart(2, '0')}`;
}

function formatFileSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
}

// Derive a meeting title from a SharePoint/Stream recording link by taking the
// recording's file name (the `id` query param, or the URL path) without its
// extension.
function deriveTitleFromUrl(url: string): string {
  try {
    const u = new URL(url);
    const id = u.searchParams.get('id');
    const raw = id ? decodeURIComponent(id) : decodeURIComponent(u.pathname);
    const base = raw.split('/').filter(Boolean).pop() || '';
    const title = base.replace(/\.[^.]+$/, '').trim();
    return title || 'Imported recording';
  } catch {
    return 'Imported recording';
  }
}

function isLikelyHttpUrl(value: string): boolean {
  return /^https?:\/\/\S+$/i.test(value.trim());
}


export function ImportAudioDialog({
  open,
  onOpenChange,
  preselectedFile,
  onComplete,
}: ImportAudioDialogProps) {
  const router = useRouter();
  const { refetchMeetings } = useSidebar();
  const { selectedLanguage, transcriptModelConfig } = useConfig();

  const [title, setTitle] = useState('');
  const [selectedLang, setSelectedLang] = useState(selectedLanguage || 'auto');
  const [showAdvanced, setShowAdvanced] = useState(false);
  const [titleModifiedByUser, setTitleModifiedByUser] = useState(false);
  const [sourceMode, setSourceMode] = useState<'file' | 'link'>('file');
  const [linkUrl, setLinkUrl] = useState('');
  const [linkContentMode, setLinkContentMode] = useState<'audio' | 'transcript'>('audio');
  const [linkModeTouched, setLinkModeTouched] = useState(false);

  // Always start as false — represents "dialog has not yet been opened".
  // Do NOT initialize from the `open` prop: if the component mounts with open=true
  // (e.g. drag-drop path), we still need the initialization effect to run.
  const prevOpenRef = useRef(false);

  // Use centralized model fetching hook
  const {
    availableModels,
    selectedModelKey,
    setSelectedModelKey,
    loadingModels,
    fetchModels,
    resetSelection,
  } = useTranscriptionModels(transcriptModelConfig);

  const handleImportComplete = useCallback((result: ImportResult) => {
    toast.success(`Import complete! ${result.segments_count} segments created.`);

    // Refresh meetings list then navigate to the imported meeting
    refetchMeetings();
    onComplete?.();
    onOpenChange(false);
    router.push(`/meeting-details?id=${result.meeting_id}`);
  }, [router, refetchMeetings, onComplete, onOpenChange]);

  const handleImportError = useCallback((error: string) => {
    toast.error('Import failed', { description: error });
  }, []);

  const {
    status,
    fileInfo,
    progress,
    error,
    isProcessing,
    isBusy,
    selectFile,
    validateFile,
    startImport,
    startImportFromUrl,
    cancelImport,
    reset,
  } = useImportAudio({
    onComplete: handleImportComplete,
    onError: handleImportError,
  });

  // Reset state only when dialog transitions from closed to open
  // This prevents re-initialization when config changes while dialog is already open (Bug #4 & #5)
  useEffect(() => {
    const wasOpen = prevOpenRef.current;
    prevOpenRef.current = open;

    // Only initialize when transitioning from closed (false) to open (true)
    if (open && !wasOpen) {
      reset();
      resetSelection();
      setTitle('');
      setTitleModifiedByUser(false);
      setSelectedLang(selectedLanguage || 'auto');
      setShowAdvanced(false);
      setSourceMode('file');
      setLinkUrl('');
      setLinkContentMode('audio');
      setLinkModeTouched(false);

      // Validate preselected file if provided
      if (preselectedFile) {
        validateFile(preselectedFile).then((info) => {
          if (info) {
            setTitle(info.filename);
          }
        });
      }

      // Fetch available models using centralized hook
      fetchModels();
    }
  }, [open, preselectedFile, selectedLanguage, transcriptModelConfig, reset, resetSelection, validateFile, fetchModels]);

  // Update title when fileInfo changes
  useEffect(() => {
    if (fileInfo && !title && !titleModifiedByUser) {
      setTitle(fileInfo.filename);
    }
  }, [fileInfo, title, titleModifiedByUser]);

  const selectedModel = useMemo((): ModelOption | undefined => {
    if (!selectedModelKey) return undefined;
    const colonIndex = selectedModelKey.indexOf(':');
    if (colonIndex === -1) return undefined;
    const provider = selectedModelKey.slice(0, colonIndex);
    const name = selectedModelKey.slice(colonIndex + 1);
    return availableModels.find((m) => m.provider === provider && m.name === name);
  }, [selectedModelKey, availableModels]);
  const isParakeetModel = selectedModel?.provider === 'parakeet';
  const isRemoteModel = selectedModel?.provider === 'openaiCompatible';

  useEffect(() => {
    if (isParakeetModel && selectedLang !== 'auto') {
      setSelectedLang('auto');
    }
  }, [isParakeetModel, selectedLang]);

  const handleSelectFile = async () => {
    const info = await selectFile();
    if (info) {
      setTitle(info.filename);
    }
  };

  const linkValid = isLikelyHttpUrl(linkUrl);
  const canImport = sourceMode === 'file' ? !!fileInfo : linkValid;

  const handleStartImport = async () => {
    const language = isParakeetModel ? null : selectedLang === 'auto' ? null : selectedLang;
    const modelName = selectedModel?.name || null;
    const providerName = selectedModel?.provider || null;

    if (sourceMode === 'link') {
      if (!linkValid) return;

      const isTranscript = linkContentMode === 'transcript';
      const importTitle = title.trim() || deriveTitleFromUrl(linkUrl);
      const started = await startImportFromUrl(
        linkUrl.trim(),
        importTitle,
        // Transcript import ignores language/model (no Whisper pass).
        isTranscript ? null : language,
        isTranscript ? null : modelName,
        isTranscript ? null : providerName,
        linkContentMode
      );

      // Link imports involve a sign-in window and a download, so they always
      // continue in the background — hand off to the background toast and close.
      if (started) {
        window.dispatchEvent(new CustomEvent('meetily-background-import-started', {
          detail: {
            importId: started.import_id,
            title: importTitle,
          },
        }));

        toast.info(isTranscript ? 'Transcript import started' : 'Import started', {
          description: isTranscript
            ? 'Fetching the Teams transcript in the background. A sign-in window may appear if needed.'
            : 'Signing in and downloading in the background. A sign-in window may appear if needed.',
        });

        reset();
        onOpenChange(false);
      }
      return;
    }

    if (!fileInfo) return;

    const importTitle = title || fileInfo.filename;
    const started = await startImport(
      fileInfo.path,
      importTitle,
      language,
      modelName,
      providerName
    );

    if (started && isRemoteModel) {
      window.dispatchEvent(new CustomEvent('meetily-background-import-started', {
        detail: {
          importId: started.import_id,
          title: importTitle,
        },
      }));

      toast.info('Remote import started', {
        description: 'It will continue in the background, and you can start another remote transcription now.',
      });

      reset();
      onOpenChange(false);
    }
  };

  const handleCancel = async () => {
    if (isProcessing) {
      await cancelImport();
      toast.info('Import cancelled');
    }
    onOpenChange(false);
  };

  // Prevent closing during processing
  const handleOpenChange = (newOpen: boolean) => {
    if (!newOpen && isProcessing) {
      return;
    }
    onOpenChange(newOpen);
  };

  const handleEscapeKeyDown = (event: KeyboardEvent) => {
    if (isProcessing) {
      event.preventDefault();
    }
  };

  const handleInteractOutside = (event: Event) => {
    if (isProcessing) {
      event.preventDefault();
    }
  };

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent
        className="sm:max-w-[500px]"
        onEscapeKeyDown={handleEscapeKeyDown}
        onInteractOutside={handleInteractOutside}
      >
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            {isProcessing ? (
              <>
                <Loader2 className="h-5 w-5 animate-spin text-blue-600" />
                Importing Audio...
              </>
            ) : error ? (
              <>
                <AlertCircle className="h-5 w-5 text-red-600" />
                Import Failed
              </>
            ) : status === 'complete' ? (
              <>
                <CheckCircle2 className="h-5 w-5 text-green-600" />
                Import Complete
              </>
            ) : (
              <>
                <Upload className="h-5 w-5 text-blue-600" />
                Import Audio File
              </>
            )}
          </DialogTitle>
          <DialogDescription>
            {isProcessing
              ? progress?.message || 'Processing audio...'
              : error
              ? 'An error occurred during import'
              : 'Import an audio file to create a new meeting with transcripts'}
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-4 py-4">
          {/* File selection / info */}
          {!isProcessing && !error && (
            <>
              {/* Source toggle: local file vs Teams/SharePoint link */}
              <div className="grid grid-cols-2 gap-1 p-1 bg-gray-100 rounded-lg text-sm font-medium">
                <button
                  type="button"
                  onClick={() => setSourceMode('file')}
                  className={`flex items-center justify-center gap-2 rounded-md py-2 transition-colors ${
                    sourceMode === 'file' ? 'bg-white shadow text-gray-900' : 'text-gray-500 hover:text-gray-700'
                  }`}
                >
                  <FileAudio className="h-4 w-4" />
                  Local File
                </button>
                <button
                  type="button"
                  onClick={() => setSourceMode('link')}
                  className={`flex items-center justify-center gap-2 rounded-md py-2 transition-colors ${
                    sourceMode === 'link' ? 'bg-white shadow text-gray-900' : 'text-gray-500 hover:text-gray-700'
                  }`}
                >
                  <Link2 className="h-4 w-4" />
                  Teams / SharePoint Link
                </button>
              </div>

              {sourceMode === 'file' && (
                fileInfo ? (
                <div className="bg-gray-50 rounded-lg p-4 space-y-3">
                  <div className="flex items-start gap-3">
                    <FileAudio className="h-8 w-8 text-blue-600 flex-shrink-0" />
                    <div className="flex-1 min-w-0">
                      <p className="font-medium text-gray-900 truncate">{fileInfo.filename}</p>
                      <div className="flex items-center gap-4 text-sm text-gray-500 mt-1">
                        <span className="flex items-center gap-1">
                          <Clock className="h-3.5 w-3.5" />
                          {formatDuration(fileInfo.duration_seconds)}
                        </span>
                        <span className="flex items-center gap-1">
                          <HardDrive className="h-3.5 w-3.5" />
                          {formatFileSize(fileInfo.size_bytes)}
                        </span>
                        <span className="text-blue-600 font-medium">{fileInfo.format}</span>
                      </div>
                    </div>
                  </div>

                  {/* Editable title */}
                  <div className="space-y-1">
                    <label className="text-sm font-medium text-gray-700">Meeting Title</label>
                    <Input
                      value={title}
                      onChange={(e) => {
                        setTitle(e.target.value);
                        setTitleModifiedByUser(true);
                      }}
                      placeholder="Enter meeting title"
                    />
                  </div>

                  <Button variant="outline" size="sm" onClick={handleSelectFile} className="w-full">
                    Choose Different File
                  </Button>
                </div>
              ) : (
                <div className="border-2 border-dashed border-gray-300 rounded-lg p-8 text-center">
                  <FileAudio className="h-12 w-12 text-gray-400 mx-auto mb-4" />
                  <Button onClick={handleSelectFile} disabled={status === 'validating'}>
                    {status === 'validating' ? (
                      <>
                        <Loader2 className="h-4 w-4 mr-2 animate-spin" />
                        Validating...
                      </>
                    ) : (
                      <>
                        <Upload className="h-4 w-4 mr-2" />
                        Select Audio File
                      </>
                    )}
                  </Button>
                  <p className="text-sm text-gray-500 mt-2">MP4, WAV, MP3, FLAC, OGG, MKV, WebM, WMA</p>
                </div>
                )
              )}

              {sourceMode === 'link' && (
                <div className="space-y-3">
                  <div className="space-y-1">
                    <label className="text-sm font-medium text-gray-700">Recording Link</label>
                    <Input
                      value={linkUrl}
                      onChange={(e) => {
                        const value = e.target.value;
                        setLinkUrl(value);
                        if (!titleModifiedByUser && value) {
                          setTitle(deriveTitleFromUrl(value));
                        }
                        if (!linkModeTouched && value) {
                          const detected = detectLinkContentMode(value);
                          if (detected) setLinkContentMode(detected);
                        }
                      }}
                      placeholder="https://…sharepoint.com/…/stream.aspx?id=…"
                    />
                    <p className="text-xs text-muted-foreground">
                      Paste a recording or transcript link from Teams or SharePoint. A sign-in window
                      may appear the first time; after that it stays signed in and runs in the background.
                    </p>
                  </div>

                  {/* What to import: recording audio vs Teams transcript */}
                  <div className="space-y-1">
                    <label className="text-sm font-medium text-gray-700">What to import</label>
                    <div className="grid grid-cols-2 gap-1 p-1 bg-gray-100 rounded-lg text-sm font-medium">
                      <button
                        type="button"
                        onClick={() => { setLinkContentMode('audio'); setLinkModeTouched(true); }}
                        className={`rounded-md py-2 transition-colors ${
                          linkContentMode === 'audio' ? 'bg-white shadow text-gray-900' : 'text-gray-500 hover:text-gray-700'
                        }`}
                      >
                        Recording audio
                      </button>
                      <button
                        type="button"
                        onClick={() => { setLinkContentMode('transcript'); setLinkModeTouched(true); }}
                        className={`rounded-md py-2 transition-colors ${
                          linkContentMode === 'transcript' ? 'bg-white shadow text-gray-900' : 'text-gray-500 hover:text-gray-700'
                        }`}
                      >
                        Teams transcript
                      </button>
                    </div>
                    <p className="text-xs text-muted-foreground">
                      {linkContentMode === 'transcript'
                        ? "Imports Teams' generated transcript with speaker names — no re-transcription."
                        : 'Downloads the recording and transcribes the audio.'}
                    </p>
                  </div>

                  <div className="space-y-1">
                    <label className="text-sm font-medium text-gray-700">Meeting Title</label>
                    <Input
                      value={title}
                      onChange={(e) => {
                        setTitle(e.target.value);
                        setTitleModifiedByUser(true);
                      }}
                      placeholder="Enter meeting title"
                    />
                  </div>
                </div>
              )}

              {/* Advanced options (collapsible) — irrelevant for transcript import */}
              {(fileInfo || (sourceMode === 'link' && linkContentMode === 'audio')) && (
                <div className="border rounded-lg">
                  <button
                    onClick={() => setShowAdvanced(!showAdvanced)}
                    className="w-full flex items-center justify-between p-3 text-sm font-medium text-gray-700 hover:bg-gray-50"
                  >
                    <span>Advanced Options</span>
                    {showAdvanced ? (
                      <ChevronUp className="h-4 w-4" />
                    ) : (
                      <ChevronDown className="h-4 w-4" />
                    )}
                  </button>

                  {showAdvanced && (
                    <div className="p-3 pt-0 space-y-4 border-t">
                      {/* Language selector */}
                      {!isParakeetModel ? (
                        <div className="space-y-2">
                          <div className="flex items-center gap-2">
                            <Globe className="h-4 w-4 text-muted-foreground" />
                            <span className="text-sm font-medium">Language</span>
                          </div>
                          <Select value={selectedLang} onValueChange={setSelectedLang}>
                            <SelectTrigger className="w-full">
                              <SelectValue placeholder="Select language" />
                            </SelectTrigger>
                            <SelectContent className="max-h-60">
                              {LANGUAGES.map((lang) => (
                                <SelectItem key={lang.code} value={lang.code}>
                                  {lang.name}
                                </SelectItem>
                              ))}
                            </SelectContent>
                          </Select>
                        </div>
                      ) : (
                        <div className="space-y-2">
                          <div className="flex items-center gap-2">
                            <Globe className="h-4 w-4 text-muted-foreground" />
                            <span className="text-sm font-medium">Language</span>
                          </div>
                          <p className="text-xs text-muted-foreground">
                            Language selection isn't supported for Parakeet. It always uses automatic detection.
                          </p>
                        </div>
                      )}

                      {/* Model selector */}
                      {availableModels.length > 0 && (
                        <div className="space-y-2">
                          <div className="flex items-center gap-2">
                            <Cpu className="h-4 w-4 text-muted-foreground" />
                            <span className="text-sm font-medium">Model</span>
                          </div>
                          <Select
                            value={selectedModelKey}
                            onValueChange={setSelectedModelKey}
                            disabled={loadingModels}
                          >
                            <SelectTrigger className="w-full">
                              <SelectValue placeholder={loadingModels ? 'Loading models...' : 'Select model'} />
                            </SelectTrigger>
                            <SelectContent>
                              {availableModels.map((model) => (
                                <SelectItem
                                  key={`${model.provider}:${model.name}`}
                                  value={`${model.provider}:${model.name}`}
                                >
                                  {model.displayName}{model.size_mb > 0 ? ` (${Math.round(model.size_mb)} MB)` : ''}
                                </SelectItem>
                              ))}
                            </SelectContent>
                          </Select>
                        </div>
                      )}
                    </div>
                  )}
                </div>
              )}
            </>
          )}

          {/* Progress display */}
          {isProcessing && progress && (
            <div className="space-y-2">
              <div className="relative">
                <div className="w-full bg-gray-200 rounded-full h-3">
                  <div
                    className="bg-blue-600 h-3 rounded-full transition-all duration-300 ease-out"
                    style={{ width: `${Math.min(progress.progress_percentage, 100)}%` }}
                  />
                </div>
                <div className="flex justify-between text-xs text-gray-600 mt-1">
                  <span>{progress.stage}</span>
                  <span>{Math.round(progress.progress_percentage)}%</span>
                </div>
              </div>
              <p className="text-sm text-muted-foreground text-center">{progress.message}</p>
            </div>
          )}

          {/* Error display */}
          {error && (
            <div className="bg-red-50 border border-red-200 rounded-lg p-3">
              <p className="text-sm text-red-800">{error}</p>
            </div>
          )}
        </div>

        <DialogFooter>
          {!isProcessing && !error && (
            <>
              <Button variant="outline" onClick={() => onOpenChange(false)}>
                Cancel
              </Button>
              <Button
                onClick={handleStartImport}
                className="bg-blue-600 hover:bg-blue-700"
                disabled={!canImport}
              >
                {sourceMode === 'link' ? (
                  <>
                    <Link2 className="h-4 w-4 mr-2" />
                    {linkContentMode === 'transcript' ? 'Import Transcript' : 'Import from Link'}
                  </>
                ) : (
                  <>
                    <Upload className="h-4 w-4 mr-2" />
                    Import
                  </>
                )}
              </Button>
            </>
          )}
          {isProcessing && (
            <Button variant="outline" onClick={handleCancel}>
              <X className="h-4 w-4 mr-2" />
              Cancel
            </Button>
          )}
          {error && (
            <>
              <Button variant="outline" onClick={() => onOpenChange(false)}>
                Close
              </Button>
              <Button onClick={reset} variant="outline">
                Try Again
              </Button>
            </>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
