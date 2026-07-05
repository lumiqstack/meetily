import { useState, useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from './ui/select';
import { Input } from './ui/input';
import { Button } from './ui/button';
import { Label } from './ui/label';
import { Switch } from './ui/switch';
import { Eye, EyeOff, Lock, Unlock } from 'lucide-react';
import { ModelManager } from './WhisperModelManager';
import { ParakeetModelManager } from './ParakeetModelManager';
import { useRecordingState } from '@/contexts/RecordingStateContext';


export interface TranscriptModelProps {
    provider: 'localWhisper' | 'parakeet' | 'deepgram' | 'elevenLabs' | 'groq' | 'openai' | 'openaiCompatible';
    model: string;
    apiKey?: string | null;
    baseUrl?: string | null;
    realtimeTranscriptionEnabled: boolean;
}

export interface TranscriptSettingsProps {
    transcriptModelConfig: TranscriptModelProps;
    setTranscriptModelConfig: (config: TranscriptModelProps) => void;
    onModelSelect?: () => void;
}

export function TranscriptSettings({ transcriptModelConfig, setTranscriptModelConfig, onModelSelect }: TranscriptSettingsProps) {
    const { isRecording } = useRecordingState();
    const [apiKey, setApiKey] = useState<string | null>(transcriptModelConfig.apiKey || null);
    const [showApiKey, setShowApiKey] = useState<boolean>(false);
    const [isApiKeyLocked, setIsApiKeyLocked] = useState<boolean>(true);
    const [isLockButtonVibrating, setIsLockButtonVibrating] = useState<boolean>(false);
    const [uiProvider, setUiProvider] = useState<TranscriptModelProps['provider']>(transcriptModelConfig.provider);
    const [remoteBaseUrl, setRemoteBaseUrl] = useState<string>(transcriptModelConfig.baseUrl || '');
    const [remoteModel, setRemoteModel] = useState<string>(
        transcriptModelConfig.provider === 'openaiCompatible' ? transcriptModelConfig.model : ''
    );
    const [remoteSaveStatus, setRemoteSaveStatus] = useState<'idle' | 'saving' | 'saved' | 'error'>('idle');

    // Sync uiProvider when backend config changes (e.g., after model selection or initial load)
    useEffect(() => {
        setUiProvider(transcriptModelConfig.provider);
    }, [transcriptModelConfig.provider]);

    // Sync remote provider fields when backend config loads
    useEffect(() => {
        if (transcriptModelConfig.baseUrl) {
            setRemoteBaseUrl(transcriptModelConfig.baseUrl);
        }
        if (transcriptModelConfig.provider === 'openaiCompatible' && transcriptModelConfig.model) {
            setRemoteModel(transcriptModelConfig.model);
        }
    }, [transcriptModelConfig.baseUrl, transcriptModelConfig.provider, transcriptModelConfig.model]);

    useEffect(() => {
        if (transcriptModelConfig.provider === 'localWhisper' || transcriptModelConfig.provider === 'parakeet') {
            setApiKey(null);
        }
    }, [transcriptModelConfig.provider]);

    const fetchApiKey = async (provider: string) => {
        try {

            const data = await invoke('api_get_transcript_api_key', { provider }) as string;

            setApiKey(data || '');
        } catch (err) {
            console.error('Error fetching API key:', err);
            setApiKey(null);
        }
    };
    const modelOptions = {
        localWhisper: [], // Model selection handled by ModelManager component
        parakeet: [], // Model selection handled by ParakeetModelManager component
        deepgram: ['nova-2-phonecall'],
        elevenLabs: ['eleven_multilingual_v2'],
        groq: ['llama-3.3-70b-versatile'],
        openai: ['gpt-4o'],
        openaiCompatible: [], // Model entered as free text in the remote settings panel
    };
    const requiresApiKey = transcriptModelConfig.provider === 'deepgram' || transcriptModelConfig.provider === 'elevenLabs' || transcriptModelConfig.provider === 'openai' || transcriptModelConfig.provider === 'groq';

    const handleInputClick = () => {
        if (isApiKeyLocked) {
            setIsLockButtonVibrating(true);
            setTimeout(() => setIsLockButtonVibrating(false), 500);
        }
    };

    const handleWhisperModelSelect = (modelName: string) => {
        // Always update config when model is selected, regardless of current provider
        // This ensures the model is set when user switches back
        setTranscriptModelConfig({
            ...transcriptModelConfig,
            provider: 'localWhisper', // Ensure provider is set correctly
            model: modelName
        });
        // Close modal after selection
        if (onModelSelect) {
            onModelSelect();
        }
    };

    const handleParakeetModelSelect = (modelName: string) => {
        // Always update config when model is selected, regardless of current provider
        // This ensures the model is set when user switches back
        setTranscriptModelConfig({
            ...transcriptModelConfig,
            provider: 'parakeet', // Ensure provider is set correctly
            model: modelName
        });
        // Close modal after selection
        if (onModelSelect) {
            onModelSelect();
        }
    };

    const handleSaveRemoteConfig = async () => {
        const baseUrl = remoteBaseUrl.trim();
        const model = remoteModel.trim();
        if (!baseUrl || !model) {
            setRemoteSaveStatus('error');
            return;
        }

        setRemoteSaveStatus('saving');
        const updatedConfig: TranscriptModelProps = {
            ...transcriptModelConfig,
            provider: 'openaiCompatible',
            model,
            baseUrl,
            apiKey: apiKey || null,
        };

        try {
            await invoke('api_save_transcript_config', {
                provider: 'openaiCompatible',
                model,
                realtimeTranscriptionEnabled: transcriptModelConfig.realtimeTranscriptionEnabled ?? false,
                apiKey: apiKey || null,
                baseUrl,
            });
            setTranscriptModelConfig(updatedConfig);
            setRemoteSaveStatus('saved');
            setTimeout(() => setRemoteSaveStatus('idle'), 2000);
        } catch (err) {
            console.error('Failed to save remote transcription config:', err);
            setRemoteSaveStatus('error');
        }
    };

    const handleRealtimeToggle = async (checked: boolean) => {
        const updatedConfig = {
            ...transcriptModelConfig,
            realtimeTranscriptionEnabled: checked,
        };
        setTranscriptModelConfig(updatedConfig);

        try {
            await invoke('api_save_transcript_config', {
                provider: updatedConfig.provider,
                model: updatedConfig.model,
                realtimeTranscriptionEnabled: checked,
                apiKey: updatedConfig.apiKey ?? null,
                baseUrl: updatedConfig.baseUrl ?? null,
            });
        } catch (err) {
            console.error('Failed to save realtime transcription setting:', err);
            setTranscriptModelConfig({
                ...updatedConfig,
                realtimeTranscriptionEnabled: !checked,
            });
        }
    };

    return (
        <div>
            <div>
                {/* <div className="flex justify-between items-center mb-4">
                    <h3 className="text-lg font-semibold text-gray-900">Transcript Settings</h3>
                </div> */}
                <div className="space-y-4 pb-6">
                    <div className="flex items-center justify-between gap-4 rounded-md border border-gray-200 bg-white px-4 py-3">
                        <div className="space-y-1">
                            <Label className="text-sm font-medium text-gray-900">
                                Realtime transcription
                            </Label>
                            <p className="text-xs text-gray-500">
                                Generate transcript while recording. Uses more CPU. When off, audio is still recorded and can be transcribed after the meeting.
                            </p>
                        </div>
                        <Switch
                            checked={!!transcriptModelConfig.realtimeTranscriptionEnabled}
                            onCheckedChange={handleRealtimeToggle}
                            disabled={isRecording}
                            aria-label="Toggle realtime transcription"
                        />
                    </div>

                    <div>
                        <Label className="block text-sm font-medium text-gray-700 mb-1">
                            Transcript Model
                        </Label>
                        <div className="flex space-x-2 mx-1">
                            <Select
                                value={uiProvider}
                                onValueChange={(value) => {
                                    const provider = value as TranscriptModelProps['provider'];
                                    setUiProvider(provider);
                                    if (provider !== 'localWhisper' && provider !== 'parakeet') {
                                        fetchApiKey(provider);
                                    }
                                }}
                            >
                                <SelectTrigger className='focus:ring-1 focus:ring-blue-500 focus:border-blue-500'>
                                    <SelectValue placeholder="Select provider" />
                                </SelectTrigger>
                                <SelectContent>
                                    <SelectItem value="parakeet">⚡ Parakeet (Recommended - Real-time / Accurate)</SelectItem>
                                    <SelectItem value="localWhisper">🏠 Local Whisper (High Accuracy)</SelectItem>
                                    <SelectItem value="openaiCompatible">🌐 Remote (OpenAI-compatible)</SelectItem>
                                    {/* <SelectItem value="deepgram">☁️ Deepgram (Backup)</SelectItem>
                                    <SelectItem value="elevenLabs">☁️ ElevenLabs</SelectItem>
                                    <SelectItem value="groq">☁️ Groq</SelectItem>
                                    <SelectItem value="openai">☁️ OpenAI</SelectItem> */}
                                </SelectContent>
                            </Select>

                            {uiProvider !== 'localWhisper' && uiProvider !== 'parakeet' && uiProvider !== 'openaiCompatible' && (
                                <Select
                                    value={transcriptModelConfig.model}
                                    onValueChange={(value) => {
                                        const model = value as TranscriptModelProps['model'];
                                        setTranscriptModelConfig({ ...transcriptModelConfig, provider: uiProvider, model });
                                    }}
                                >
                                    <SelectTrigger className='focus:ring-1 focus:ring-blue-500 focus:border-blue-500'>
                                        <SelectValue placeholder="Select model" />
                                    </SelectTrigger>
                                    <SelectContent>
                                        {modelOptions[uiProvider].map((model) => (
                                            <SelectItem key={model} value={model}>{model}</SelectItem>
                                        ))}
                                    </SelectContent>
                                </Select>
                            )}

                        </div>
                    </div>

                    {uiProvider === 'localWhisper' && (
                        <div className="mt-6">
                            <ModelManager
                                selectedModel={transcriptModelConfig.provider === 'localWhisper' ? transcriptModelConfig.model : undefined}
                                onModelSelect={handleWhisperModelSelect}
                                autoSave={true}
                            />
                        </div>
                    )}

                    {uiProvider === 'parakeet' && (
                        <div className="mt-6">
                            <ParakeetModelManager
                                selectedModel={transcriptModelConfig.provider === 'parakeet' ? transcriptModelConfig.model : undefined}
                                onModelSelect={handleParakeetModelSelect}
                                autoSave={true}
                            />
                        </div>
                    )}

                    {uiProvider === 'openaiCompatible' && (
                        <div className="mt-6 space-y-4 rounded-md border border-gray-200 bg-white px-4 py-4">
                            <div>
                                <Label className="block text-sm font-medium text-gray-700 mb-1">
                                    Server Base URL
                                </Label>
                                <Input
                                    type="text"
                                    className="focus:ring-1 focus:ring-blue-500 focus:border-blue-500"
                                    value={remoteBaseUrl}
                                    onChange={(e) => setRemoteBaseUrl(e.target.value)}
                                    placeholder="http://127.0.0.1:8000/v1"
                                />
                                <p className="text-xs text-gray-500 mt-1">
                                    Any server exposing the OpenAI audio transcriptions API (oMLX, LiteLLM, vLLM, OpenAI, ...).
                                    Audio is sent to {'{base}'}/audio/transcriptions.
                                </p>
                            </div>

                            <div>
                                <Label className="block text-sm font-medium text-gray-700 mb-1">
                                    Model
                                </Label>
                                <Input
                                    type="text"
                                    className="focus:ring-1 focus:ring-blue-500 focus:border-blue-500"
                                    value={remoteModel}
                                    onChange={(e) => setRemoteModel(e.target.value)}
                                    placeholder="whisper-1"
                                />
                            </div>

                            <div>
                                <Label className="block text-sm font-medium text-gray-700 mb-1">
                                    API Key (optional)
                                </Label>
                                <div className="relative">
                                    <Input
                                        type={showApiKey ? "text" : "password"}
                                        className="pr-12 focus:ring-1 focus:ring-blue-500 focus:border-blue-500"
                                        value={apiKey || ''}
                                        onChange={(e) => setApiKey(e.target.value)}
                                        placeholder="Enter API key if the server requires one"
                                    />
                                    <div className="absolute inset-y-0 right-0 pr-1 flex items-center">
                                        <Button
                                            type="button"
                                            variant="ghost"
                                            size="icon"
                                            onClick={() => setShowApiKey(!showApiKey)}
                                        >
                                            {showApiKey ? <EyeOff className="h-4 w-4" /> : <Eye className="h-4 w-4" />}
                                        </Button>
                                    </div>
                                </div>
                            </div>

                            <div className="flex items-center gap-3">
                                <Button
                                    type="button"
                                    onClick={handleSaveRemoteConfig}
                                    disabled={isRecording || remoteSaveStatus === 'saving' || !remoteBaseUrl.trim() || !remoteModel.trim()}
                                    className="bg-blue-600 hover:bg-blue-700 text-white"
                                >
                                    {remoteSaveStatus === 'saving' ? 'Saving...' : 'Save Remote Settings'}
                                </Button>
                                {remoteSaveStatus === 'saved' && (
                                    <span className="text-sm text-green-600">Saved</span>
                                )}
                                {remoteSaveStatus === 'error' && (
                                    <span className="text-sm text-red-600">
                                        {!remoteBaseUrl.trim() || !remoteModel.trim()
                                            ? 'Base URL and model are required'
                                            : 'Failed to save settings'}
                                    </span>
                                )}
                            </div>
                        </div>
                    )}


                    {requiresApiKey && (
                        <div>
                            <Label className="block text-sm font-medium text-gray-700 mb-1">
                                API Key
                            </Label>
                            <div className="relative mx-1">
                                <Input
                                    type={showApiKey ? "text" : "password"}
                                    className={`pr-24 focus:ring-1 focus:ring-blue-500 focus:border-blue-500 ${isApiKeyLocked ? 'bg-gray-100 cursor-not-allowed' : ''
                                        }`}
                                    value={apiKey || ''}
                                    onChange={(e) => setApiKey(e.target.value)}
                                    disabled={isApiKeyLocked}
                                    onClick={handleInputClick}
                                    placeholder="Enter your API key"
                                />
                                {isApiKeyLocked && (
                                    <div
                                        onClick={handleInputClick}
                                        className="absolute inset-0 flex items-center justify-center bg-gray-100 bg-opacity-50 rounded-md cursor-not-allowed"
                                    />
                                )}
                                <div className="absolute inset-y-0 right-0 pr-1 flex items-center">
                                    <Button
                                        type="button"
                                        variant="ghost"
                                        size="icon"
                                        onClick={() => setIsApiKeyLocked(!isApiKeyLocked)}
                                        className={`transition-colors duration-200 ${isLockButtonVibrating ? 'animate-vibrate text-red-500' : ''
                                            }`}
                                        title={isApiKeyLocked ? "Unlock to edit" : "Lock to prevent editing"}
                                    >
                                        {isApiKeyLocked ? <Lock className="h-4 w-4" /> : <Unlock className="h-4 w-4" />}
                                    </Button>
                                    <Button
                                        type="button"
                                        variant="ghost"
                                        size="icon"
                                        onClick={() => setShowApiKey(!showApiKey)}
                                    >
                                        {showApiKey ? <EyeOff className="h-4 w-4" /> : <Eye className="h-4 w-4" />}
                                    </Button>
                                </div>
                            </div>
                        </div>
                    )}
                </div>
            </div>
        </div >
    )
}








