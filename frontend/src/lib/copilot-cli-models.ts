/**
 * GitHub Copilot CLI model catalog — the single list both settings surfaces
 * use. Keep RETIRED_COPILOT_CLI_MODELS identical to the Rust list in
 * src-tauri/src/summary/copilot_cli.rs (a Rust test compares them).
 *
 * Only identifiers known to be accepted are offered. Replacement families
 * named in GitHub's retirement notices (Sonnet 5, GPT-5.6 Sol/Luna, Gemini
 * 3.8 Flash, Kimi K3, Claude Opus 5.5, Grok 4.6) are deliberately absent
 * until their exact `--model` IDs are verified against an installed CLI and
 * account; 'auto' works on every plan meanwhile.
 */
export const COPILOT_CLI_MODELS: string[] = ['auto', 'claude-haiku-4.5'];

/**
 * Retired, or retiring during October 2026, so never offered, never used as
 * an alias target, and rejected when saved. Includes scheduled retirements
 * before their effective date (October 19 group) by user requirement.
 */
export const RETIRED_COPILOT_CLI_MODELS: string[] = [
  // Previously retired
  'claude-sonnet-4.5',
  'claude-sonnet-4',
  'gpt-5',
  'gpt-4.1',
  'gemini-2.5-pro',
  'gemini-3.1-pro',
  // Retired October 2, 2026
  'gemini-3.5-flash',
  'gemini-3.6-flash',
  'kimi-k2.7-code',
  'claude-opus-4.7',
  // Retiring October 19, 2026
  'gpt-5-mini',
  'gpt-5.4',
  'gpt-5.4-mini',
  'gpt-5.5',
  'gemini-3.7-flash',
  'grok-4.5',
];

const RETIRED = new Set(RETIRED_COPILOT_CLI_MODELS.map(id => id.toLowerCase()));

export function isRetiredCopilotCliModel(model: string | null | undefined): boolean {
  return RETIRED.has((model ?? '').trim().toLowerCase());
}

/**
 * The model to use for a saved Copilot selection. Empty means 'auto'. A
 * retired ID is returned unchanged (never silently remapped) so the settings
 * UI can flag it and require a replacement; other unknown IDs are kept as the
 * user's custom choice without claiming they are available.
 */
export function resolveSavedCopilotCliModel(model: string | null | undefined): string {
  const trimmed = (model ?? '').trim();
  return trimmed || 'auto';
}

/** Validation message for a selection that must be replaced, or null. */
export function copilotCliModelProblem(model: string | null | undefined): string | null {
  if (!isRetiredCopilotCliModel(model)) return null;
  return `GitHub has retired the Copilot model "${(model ?? '').trim()}". Choose another model (Auto works on every plan).`;
}
