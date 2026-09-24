export const COPILOT_CLI_MODELS: string[] = [
  'auto',
  'claude-sonnet-5',
  'claude-fable-5.1',
  'claude-fable-5',
  'claude-opus-5',
  'claude-opus-4.8',
  'claude-opus-4.8-fast',
  'claude-opus-4.7',
  'claude-sonnet-4.6',
  'claude-haiku-4.5',
  'gpt-5.6-sol',
  'gpt-5.6-terra',
  'gpt-5.6-luna',
  'gpt-5.5',
  'gemini-3.7-flash',
  'gemini-3.6-flash',
  'gemini-3.5-flash',
  'grok-4.5',
  'kimi-k3',
  'kimi-k2.7-code',
];

const LEGACY_COPILOT_MODEL_ALIASES: Record<string, string> = {
  'claude-sonnet-4.5': 'claude-sonnet-5',
  'claude-sonnet-4': 'claude-sonnet-4.6',
  'gpt-5': 'gpt-5.5',
  'gpt-5-mini': 'auto',
  'gpt-4.1': 'auto',
  'gemini-2.5-pro': 'auto',
};

export function normalizeCopilotCliModel(model: string | null | undefined): string {
  const normalizedModel = model?.trim() || 'auto';
  return LEGACY_COPILOT_MODEL_ALIASES[normalizedModel] || normalizedModel;
}