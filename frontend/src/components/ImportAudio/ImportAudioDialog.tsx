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
  FolderOpen,
  Files,
  Cloud,
  RefreshCw,
} from 'lucide-react';
import { invoke } from '@tauri-apps/api/core';
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
import { getSharedImportQueue } from '@/lib/import-queue';
import { backgroundJobStore } from '@/components/shared/BackgroundJobToast';

/** Matches the Rust BatchCandidate returned by the batch-selection commands. */
interface BatchCandidate {
  path: string;
  file_name: string;
  size_bytes: number;
}

/** Meeting title for a batch item: the file name without its extension. */
function titleFromFileName(fileName: string): string {
  return fileName.replace(/\.[^.]+$/, '').trim() || fileName;
}

/** Matches the Rust SharePointScanItem (SharePointRecording + flags). */
interface SharePointScanItem {
  name: string;
  file_url: string;
  stream_url: string;
  created: string;
  size_bytes: number | null;
  already_imported: boolean;
  /** "mine" (own OneDrive Recordings folder) or "shared" (found via search). */
  source: 'mine' | 'shared';
}

interface SharePointSyncState {
  hubUrl?: string | null;
  lastSyncDate?: string | null;
  imported?: Record<string, string>;
}

/** Default scan window: the stored last sync date, else 30 days back. */
function defaultSinceDate(lastSyncDate?: string | null): string {
  if (lastSyncDate && /^\d{4}-\d{2}-\d{2}/.test(lastSyncDate)) {
    return lastSyncDate.slice(0, 10);
  }
  const d = new Date(Date.now() - 30 * 24 * 60 * 60 * 1000);
  return d.toISOString().slice(0, 10);
}


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
  const [sourceMode, setSourceMode] = useState<'file' | 'link' | 'sharepoint'>('file');
  const [batchFiles, setBatchFiles] = useState<BatchCandidate[]>([]);
  const [linkUrl, setLinkUrl] = useState('');
  const [linkContentMode, setLinkContentMode] = useState<'audio' | 'transcript'>('audio');
  const [linkModeTouched, setLinkModeTouched] = useState(false);
  const [spHubUrl, setSpHubUrl] = useState('');
  const [spSinceDate, setSpSinceDate] = useState(() => defaultSinceDate());
  const [spScanning, setSpScanning] = useState(false);
  const [spItems, setSpItems] = useState<SharePointScanItem[] | null>(null);
  const [spSelected, setSpSelected] = useState<Set<string>>(new Set());
  const spPrefsLoaded = useRef(false);

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
      setBatchFiles([]);
      setLinkUrl('');
      setLinkContentMode('audio');
      setLinkModeTouched(false);
      setSpScanning(false);
      setSpItems(null);
      setSpSelected(new Set());
      spPrefsLoaded.current = false;

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
      setBatchFiles([]);
    }
  };

  const addBatchCandidates = useCallback((incoming: BatchCandidate[]) => {
    if (incoming.length === 0) return;
    setBatchFiles((prev) => {
      const seen = new Set(prev.map((f) => f.path));
      return [...prev, ...incoming.filter((f) => !seen.has(f.path))];
    });
  }, []);

  const handleAddFiles = async () => {
    try {
      const picked = await invoke<BatchCandidate[]>('select_audio_files_command');
      addBatchCandidates(picked);
    } catch (e) {
      toast.error('Could not select files', { description: String(e) });
    }
  };

  const handleAddFolder = async () => {
    try {
      const result = await invoke<{ candidates: BatchCandidate[]; truncated: boolean } | null>(
        'select_audio_folder_command'
      );
      if (!result) return;
      if (result.candidates.length === 0) {
        toast.info('No audio files found in that folder');
        return;
      }
      if (result.truncated) {
        toast.warning('Folder scan stopped at 500 files', {
          description: 'Import this batch first, then add the rest.',
        });
      }
      addBatchCandidates(result.candidates);
    } catch (e) {
      toast.error('Could not scan folder', { description: String(e) });
    }
  };

  const handleRemoveBatchFile = (path: string) => {
    setBatchFiles((prev) => prev.filter((f) => f.path !== path));
  };

  // Prefill the SharePoint hub URL / date from the persisted sync state the
  // first time the user opens that tab in this dialog session.
  useEffect(() => {
    if (sourceMode !== 'sharepoint' || spPrefsLoaded.current) return;
    spPrefsLoaded.current = true;
    invoke<SharePointSyncState>('get_sharepoint_sync_state_command')
      .then((state) => {
        if (state.hubUrl) setSpHubUrl(state.hubUrl);
        setSpSinceDate(defaultSinceDate(state.lastSyncDate));
      })
      .catch(() => {
        /* defaults are fine */
      });
  }, [sourceMode]);

  const spHubValid = isLikelyHttpUrl(spHubUrl) && /\.sharepoint\.(com|us)/i.test(spHubUrl);

  const handleSharePointScan = async () => {
    if (!spHubValid || spScanning) return;
    setSpScanning(true);
    setSpItems(null);
    try {
      const result = await invoke<{ strategy: string; recordings: SharePointScanItem[] }>(
        'sharepoint_scan_recordings_command',
        { hubUrl: spHubUrl.trim(), sinceIso: `${spSinceDate}T00:00:00Z` }
      );
      setSpItems(result.recordings);
      // Preselect everything not yet imported.
      setSpSelected(
        new Set(result.recordings.filter((r) => !r.already_imported).map((r) => r.file_url))
      );
      if (result.recordings.length === 0) {
        toast.info('No recordings found since that date');
      }
    } catch (e) {
      toast.error('Could not scan SharePoint recordings', { description: String(e) });
    } finally {
      setSpScanning(false);
    }
  };

  const toggleSpSelected = (fileUrl: string) => {
    setSpSelected((prev) => {
      const next = new Set(prev);
      if (next.has(fileUrl)) next.delete(fileUrl);
      else next.add(fileUrl);
      return next;
    });
  };

  const handleStartSharePointImport = () => {
    const selected = (spItems ?? []).filter((r) => spSelected.has(r.file_url));
    if (selected.length === 0) return;

    const language = isParakeetModel ? null : selectedLang === 'auto' ? null : selectedLang;
    const queue = getSharedImportQueue(backgroundJobStore, (command, args) =>
      invoke(command, args)
    );
    queue.enqueueBatch(
      // The direct file URL downloads via an authenticated GET in the backend
      // (no yt-dlp page scraping, which breaks on newer Stream UIs).
      selected.map((r) => ({ url: r.file_url, title: titleFromFileName(r.name) })),
      {
        language,
        model: selectedModel?.name || null,
        provider: selectedModel?.provider || null,
        onItemCompleted: (item) => {
          const rec = selected.find((r) => r.file_url === item.url);
          if (rec) {
            void invoke('mark_sharepoint_imported_command', { fileUrl: rec.file_url }).catch(
              () => {
                /* re-scan will just show it unchecked again */
              }
            );
          }
        },
      }
    );

    // Remember the hub and advance the sync date to today.
    void invoke('set_sharepoint_sync_prefs_command', {
      hubUrl: spHubUrl.trim(),
      lastSyncDate: new Date().toISOString().slice(0, 10),
    }).catch(() => {});

    toast.info(`Importing ${selected.length} recording${selected.length === 1 ? '' : 's'}`, {
      description:
        'They download and transcribe one at a time in the background. A sign-in window may appear if needed.',
    });

    reset();
    onOpenChange(false);
  };

  const handleStartBatch = () => {
    const language = isParakeetModel ? null : selectedLang === 'auto' ? null : selectedLang;
    const queue = getSharedImportQueue(backgroundJobStore, (command, args) =>
      invoke(command, args)
    );
    queue.enqueueBatch(
      batchFiles.map((f) => ({ path: f.path, title: titleFromFileName(f.file_name) })),
      {
        language,
        model: selectedModel?.name || null,
        provider: selectedModel?.provider || null,
      }
    );

    toast.info(`Importing ${batchFiles.length} files`, {
      description: isRemoteModel
        ? 'Up to 3 run at a time in the background.'
        : 'They run one at a time in the background.',
    });

    setBatchFiles([]);
    reset();
    onOpenChange(false);
  };

  const linkValid = isLikelyHttpUrl(linkUrl);
  const canImport =
    sourceMode === 'file'
      ? !!fileInfo || batchFiles.length > 0
      : sourceMode === 'sharepoint'
      ? spSelected.size > 0
      : linkValid;

  const handleStartImport = async () => {
    const language = isParakeetModel ? null : selectedLang === 'auto' ? null : selectedLang;
    const modelName = selectedModel?.name || null;
    const providerName = selectedModel?.provider || null;

    if (sourceMode === 'sharepoint') {
      handleStartSharePointImport();
      return;
    }

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

    if (batchFiles.length > 0) {
      handleStartBatch();
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
              {/* Source toggle: local file vs link vs SharePoint sync */}
              <div className="grid grid-cols-3 gap-1 p-1 bg-gray-100 rounded-lg text-sm font-medium">
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
                  Teams Link
                </button>
                <button
                  type="button"
                  onClick={() => setSourceMode('sharepoint')}
                  className={`flex items-center justify-center gap-2 rounded-md py-2 transition-colors ${
                    sourceMode === 'sharepoint' ? 'bg-white shadow text-gray-900' : 'text-gray-500 hover:text-gray-700'
                  }`}
                >
                  <Cloud className="h-4 w-4" />
                  SharePoint
                </button>
              </div>

              {sourceMode === 'file' && batchFiles.length > 0 && (
                <div className="bg-gray-50 rounded-lg p-4 space-y-3">
                  <div className="flex items-center justify-between">
                    <p className="font-medium text-gray-900">
                      {batchFiles.length} file{batchFiles.length === 1 ? '' : 's'} selected
                    </p>
                    <span className="text-sm text-gray-500">
                      {formatFileSize(batchFiles.reduce((sum, f) => sum + f.size_bytes, 0))}
                    </span>
                  </div>
                  <div className="max-h-44 overflow-y-auto space-y-1 pr-1">
                    {batchFiles.map((file) => (
                      <div
                        key={file.path}
                        className="flex items-center gap-2 bg-white rounded-md border border-gray-200 px-2 py-1.5"
                      >
                        <FileAudio className="h-4 w-4 text-blue-600 flex-shrink-0" />
                        <span className="flex-1 min-w-0 truncate text-sm text-gray-800">
                          {file.file_name}
                        </span>
                        <span className="text-xs text-gray-400 flex-shrink-0">
                          {formatFileSize(file.size_bytes)}
                        </span>
                        <button
                          type="button"
                          onClick={() => handleRemoveBatchFile(file.path)}
                          className="flex-shrink-0 text-gray-400 hover:text-red-600"
                          aria-label={`Remove ${file.file_name}`}
                        >
                          <X className="h-3.5 w-3.5" />
                        </button>
                      </div>
                    ))}
                  </div>
                  <div className="grid grid-cols-2 gap-2">
                    <Button variant="outline" size="sm" onClick={handleAddFiles}>
                      <Files className="h-4 w-4 mr-2" />
                      Add Files
                    </Button>
                    <Button variant="outline" size="sm" onClick={handleAddFolder}>
                      <FolderOpen className="h-4 w-4 mr-2" />
                      Add Folder
                    </Button>
                  </div>
                  <p className="text-xs text-muted-foreground">
                    Each file becomes its own meeting, titled after the file name. Imports run in
                    the background{isRemoteModel ? ', up to 3 at a time' : ', one at a time'}.
                  </p>
                </div>
              )}

              {sourceMode === 'file' && batchFiles.length === 0 && (
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
                  <div className="flex items-center justify-center gap-2 mt-3">
                    <Button variant="outline" size="sm" onClick={handleAddFiles}>
                      <Files className="h-4 w-4 mr-2" />
                      Multiple Files
                    </Button>
                    <Button variant="outline" size="sm" onClick={handleAddFolder}>
                      <FolderOpen className="h-4 w-4 mr-2" />
                      Whole Folder
                    </Button>
                  </div>
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

              {sourceMode === 'sharepoint' && (
                <div className="space-y-3">
                  <div className="space-y-1">
                    <label className="text-sm font-medium text-gray-700">Video Hub URL</label>
                    <Input
                      value={spHubUrl}
                      onChange={(e) => setSpHubUrl(e.target.value)}
                      placeholder="https://yourcompany.sharepoint.com/_layouts/15/videohub.aspx"
                    />
                  </div>
                  <div className="flex items-end gap-2">
                    <div className="space-y-1 flex-1">
                      <label className="text-sm font-medium text-gray-700">
                        Recordings since
                      </label>
                      <Input
                        type="date"
                        value={spSinceDate}
                        onChange={(e) => setSpSinceDate(e.target.value)}
                        max={new Date().toISOString().slice(0, 10)}
                      />
                    </div>
                    <Button
                      onClick={handleSharePointScan}
                      disabled={!spHubValid || spScanning || !spSinceDate}
                      variant="outline"
                    >
                      {spScanning ? (
                        <>
                          <Loader2 className="h-4 w-4 mr-2 animate-spin" />
                          Scanning…
                        </>
                      ) : (
                        <>
                          <RefreshCw className="h-4 w-4 mr-2" />
                          Scan
                        </>
                      )}
                    </Button>
                  </div>
                  <p className="text-xs text-muted-foreground">
                    Lists your meeting recordings from OneDrive/SharePoint. A sign-in window may
                    appear the first time.
                  </p>

                  {spItems && spItems.length > 0 && (
                    <div className="bg-gray-50 rounded-lg p-3 space-y-2">
                      <div className="flex items-center justify-between text-sm">
                        <span className="font-medium text-gray-900">
                          {spSelected.size} of {spItems.length} selected
                        </span>
                        <button
                          type="button"
                          className="text-blue-600 hover:underline text-xs"
                          onClick={() =>
                            setSpSelected(
                              spSelected.size === spItems.length
                                ? new Set()
                                : new Set(spItems.map((r) => r.file_url))
                            )
                          }
                        >
                          {spSelected.size === spItems.length ? 'Select none' : 'Select all'}
                        </button>
                      </div>
                      <div className="max-h-44 overflow-y-auto space-y-1 pr-1">
                        {spItems.map((rec) => (
                          <label
                            key={rec.file_url}
                            className="flex items-center gap-2 bg-white rounded-md border border-gray-200 px-2 py-1.5 cursor-pointer"
                          >
                            <input
                              type="checkbox"
                              checked={spSelected.has(rec.file_url)}
                              onChange={() => toggleSpSelected(rec.file_url)}
                              className="flex-shrink-0"
                            />
                            <span className="flex-1 min-w-0 truncate text-sm text-gray-800">
                              {titleFromFileName(rec.name)}
                            </span>
                            {rec.source === 'shared' && (
                              <span className="flex-shrink-0 text-[10px] font-medium uppercase tracking-wide text-blue-700 bg-blue-100 rounded px-1.5 py-0.5">
                                Shared
                              </span>
                            )}
                            {rec.already_imported && (
                              <span className="flex-shrink-0 text-[10px] font-medium uppercase tracking-wide text-green-700 bg-green-100 rounded px-1.5 py-0.5">
                                Imported
                              </span>
                            )}
                            <span className="text-xs text-gray-400 flex-shrink-0">
                              {rec.created ? rec.created.slice(0, 10) : ''}
                              {rec.size_bytes ? ` · ${formatFileSize(rec.size_bytes)}` : ''}
                            </span>
                          </label>
                        ))}
                      </div>
                    </div>
                  )}
                </div>
              )}

              {/* Advanced options (collapsible) — irrelevant for transcript import */}
              {(fileInfo ||
                (sourceMode === 'file' && batchFiles.length > 0) ||
                (sourceMode === 'sharepoint' && spSelected.size > 0) ||
                (sourceMode === 'link' && linkContentMode === 'audio')) && (
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
                {sourceMode === 'sharepoint' ? (
                  <>
                    <Cloud className="h-4 w-4 mr-2" />
                    Import {spSelected.size > 0 ? spSelected.size : ''} Recording
                    {spSelected.size === 1 ? '' : 's'}
                  </>
                ) : sourceMode === 'link' ? (
                  <>
                    <Link2 className="h-4 w-4 mr-2" />
                    {linkContentMode === 'transcript' ? 'Import Transcript' : 'Import from Link'}
                  </>
                ) : batchFiles.length > 0 ? (
                  <>
                    <Files className="h-4 w-4 mr-2" />
                    Import {batchFiles.length} File{batchFiles.length === 1 ? '' : 's'}
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
