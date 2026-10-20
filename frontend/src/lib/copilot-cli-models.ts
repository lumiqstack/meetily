/**
 * GitHub Copilot CLI model catalog — the single list both settings surfaces
 * use. Keep RETIRED_COPILOT_CLI_MODELS identical to the Rust list in
 * src-tauri/src/summary/copilot_cli.rs (a Rust test compares them).
 *
 * Offered IDs are the models GitHub's supported-models reference lists for
 * Copilot CLI (docs.github.com/en/copilot/reference/ai-models/supported-models),
 * in the CLI's slug form (lower case, hyphens, version dots kept, as in the
 * `gpt-6-luna` the CLI reports for `auto`). Availability is plan-dependent:
 * on a plan limited to `auto`, the CLI rejects every explicit ID. `auto` is
 * documented by the CLI and works on every plan. Last refreshed 2026-10-20.
 */
export const COPILOT_CLI_MODELS: string[] = [
  'auto',
  'claude-haiku-4.5',
  'claude-haiku-5.5',
  'claude-opus-4.8',
  'claude-opus-5',
  'claude-opus-5.5',
  'claude-fable-5',
  'claude-fable-5.1',
  'claude-sonnet-5',
  'claude-sonnet-5.5',
  'gemini-3.8-flash',
  'gemini-4-argon',
  'mai-code-1.1-flash',
  'gpt-5.3-codex',
  'gpt-5.6-luna',
  'gpt-5.6-sol',
  'gpt-5.6-terra',
  'gpt-6-astra',
  'gpt-6-luna',
  'gpt-6-sol',
  'grok-4.5',
  'grok-4.6',
  'grok-4.7',
  'kimi-k3',
];

/**
 * Retired per GitHub's model retirement history, so never offered, never used
 * as an alias target, and rejected when saved. Covers retirements since the
 * provider shipped (July 2026) plus earlier IDs this catalog once offered.
 * Claude Sonnet 4.6 stays here although annual Pro/Pro+ subscribers keep it.
 */
export const RETIRED_COPILOT_CLI_MODELS: string[] = [
  // Previously retired
  'claude-sonnet-4.5',
  'claude-sonnet-4',
  'gpt-5',
  'gpt-4.1',
  'gemini-2.5-pro',
  'gemini-3.1-pro',
  // Retired July 31, 2026
  'gemini-3-flash',
  // Retired September 1, 2026
  'claude-opus-4.5',
  'claude-opus-4.6',
  'claude-sonnet-4.6',
  'raptor-mini',
  // Retired September 10, 2026
  'mai-code-1-flash',
  // Retired October 2, 2026
  'gemini-3.5-flash',
  'gemini-3.6-flash',
  'kimi-k2.7-code',
  'claude-opus-4.7',
  // Retired October 19, 2026
  'gpt-5-mini',
  'gpt-5.4',
  'gpt-5.4-mini',
  'gpt-5.5',
  'gemini-3.7-flash',
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
