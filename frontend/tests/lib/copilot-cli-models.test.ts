import { describe, expect, test } from 'bun:test';
import {
  COPILOT_CLI_MODELS,
  RETIRED_COPILOT_CLI_MODELS,
  copilotCliModelProblem,
  isRetiredCopilotCliModel,
  resolveSavedCopilotCliModel,
} from '../../src/lib/copilot-cli-models';

const OCTOBER_EXCLUSIONS = [
  'gemini-3.5-flash', 'gemini-3.6-flash', 'kimi-k2.7-code', 'claude-opus-4.7',
  'gpt-5-mini', 'gpt-5.4', 'gpt-5.4-mini', 'gpt-5.5', 'gemini-3.7-flash', 'grok-4.5',
];

describe('Copilot CLI model catalog', () => {
  test('no October-retiring or retired model is offered', () => {
    for (const id of [...OCTOBER_EXCLUSIONS, 'claude-sonnet-4.5', 'claude-sonnet-4', 'gpt-5', 'gpt-4.1', 'gemini-2.5-pro']) {
      expect(COPILOT_CLI_MODELS).not.toContain(id);
      expect(RETIRED_COPILOT_CLI_MODELS).toContain(id);
    }
  });

  test('catalog always offers auto and has no duplicates', () => {
    expect(COPILOT_CLI_MODELS[0]).toBe('auto');
    expect(COPILOT_CLI_MODELS).toEqual([
      'auto',
      'claude-haiku-4.5',
      'gemini-3.8-flash',
      'claude-opus-5.5',
    ]);
    expect(new Set(COPILOT_CLI_MODELS).size).toBe(COPILOT_CLI_MODELS.length);
  });

  test('a saved retired selection is kept and flagged, never remapped', () => {
    expect(resolveSavedCopilotCliModel('gpt-5-mini')).toBe('gpt-5-mini');
    expect(isRetiredCopilotCliModel(' GPT-5-MINI ')).toBe(true);
    expect(copilotCliModelProblem('gpt-5-mini')).toContain('retired');
  });

  test('empty selects auto; valid and unknown custom IDs are kept', () => {
    expect(resolveSavedCopilotCliModel('')).toBe('auto');
    expect(resolveSavedCopilotCliModel(null)).toBe('auto');
    expect(resolveSavedCopilotCliModel('claude-haiku-4.5')).toBe('claude-haiku-4.5');
    expect(resolveSavedCopilotCliModel('my-org-model')).toBe('my-org-model');
    expect(copilotCliModelProblem('my-org-model')).toBeNull();
    expect(copilotCliModelProblem('auto')).toBeNull();
  });
});
