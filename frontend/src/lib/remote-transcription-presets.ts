export interface RemoteTranscriptionModel {
    id: string;
    label: string;
    note?: string;
}

export interface RemoteTranscriptionPreset {
    id: 'openai' | 'openrouter' | 'deepinfra' | 'custom';
    label: string;
    /** Base URL saved to transcript settings; '' for custom (free text). */
    baseUrl: string;
    /** Curated batch-transcription models that outperform local whisper-large-v3-turbo. Empty for custom. */
    models: RemoteTranscriptionModel[];
}

// Curated July 2026. Batch/file models only — realtime/streaming models
// (gpt-live-transcribe, gpt-realtime-whisper, ...) are deliberately excluded,
// as are models that don't beat the local whisper-large-v3-turbo baseline.
export const REMOTE_TRANSCRIPTION_PRESETS: RemoteTranscriptionPreset[] = [
    {
        id: 'openai',
        label: 'OpenAI',
        baseUrl: 'https://api.openai.com/v1',
        models: [
            { id: 'gpt-transcribe', label: 'GPT Transcribe', note: 'Best for meeting files and long audio (new, Jul 2026)' },
            { id: 'gpt-4o-transcribe', label: 'GPT-4o Transcribe', note: 'High accuracy, ~$0.006/min' },
            { id: 'gpt-4o-mini-transcribe', label: 'GPT-4o Mini Transcribe', note: 'Cheapest OpenAI option, ~$0.003/min' },
        ],
    },
    {
        id: 'openrouter',
        label: 'OpenRouter',
        baseUrl: 'https://openrouter.ai/api/v1',
        models: [
            { id: 'openai/gpt-4o-transcribe', label: 'GPT-4o Transcribe', note: 'High accuracy' },
            { id: 'openai/gpt-4o-mini-transcribe', label: 'GPT-4o Mini Transcribe', note: 'Cost-efficient' },
            { id: 'mistralai/voxtral-mini-transcribe', label: 'Voxtral Mini Transcribe', note: 'Mistral speech model, low cost' },
        ],
    },
    {
        id: 'deepinfra',
        label: 'DeepInfra',
        baseUrl: 'https://api.deepinfra.com/v1/openai',
        models: [
            { id: 'openai/whisper-large-v3', label: 'Whisper Large v3', note: 'More accurate than the local turbo model, ~$0.0006/min' },
            { id: 'mistralai/Voxtral-Small-24B-2507', label: 'Voxtral Small 24B', note: 'Strong accuracy on noisy audio' },
        ],
    },
    {
        id: 'custom',
        label: 'Custom server',
        baseUrl: '',
        models: [],
    },
];

const CUSTOM_PRESET = REMOTE_TRANSCRIPTION_PRESETS.find((p) => p.id === 'custom')!;

function hostnameOf(url: string): string | null {
    try {
        return new URL(url.trim()).hostname.toLowerCase();
    } catch {
        return null;
    }
}

/**
 * Re-select the preset matching a previously saved base URL (by hostname, so
 * saved values like ".../v1" or a full ".../audio/transcriptions" endpoint
 * still match). Falls back to the custom preset.
 */
export function matchPresetFromBaseUrl(baseUrl?: string | null): RemoteTranscriptionPreset {
    if (!baseUrl || !baseUrl.trim()) {
        return CUSTOM_PRESET;
    }
    const host = hostnameOf(baseUrl);
    if (!host) {
        return CUSTOM_PRESET;
    }
    return (
        REMOTE_TRANSCRIPTION_PRESETS.find((p) => p.baseUrl && hostnameOf(p.baseUrl) === host) ?? CUSTOM_PRESET
    );
}

/**
 * Human-readable label for a saved remote config, e.g. "OpenAI · GPT Transcribe".
 * Returns null when the base URL + model don't match a curated preset entry.
 */
export function findPresetModelLabel(baseUrl: string | null | undefined, model: string): string | null {
    const preset = matchPresetFromBaseUrl(baseUrl);
    if (preset.id === 'custom') {
        return null;
    }
    const entry = preset.models.find((m) => m.id === model);
    return entry ? `${preset.label} · ${entry.label}` : null;
}
