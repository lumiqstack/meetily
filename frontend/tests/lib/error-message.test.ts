import { describe, expect, test } from 'bun:test';
import { errorMessage } from '../../src/lib/utils';

describe('errorMessage', () => {
  test('returns the string a Tauri invoke rejects with', () => {
    expect(errorMessage('Meeting not found: abc', 'Unknown error')).toBe('Meeting not found: abc');
  });

  test('returns .message for an Error', () => {
    expect(errorMessage(new Error('boom'), 'Unknown error')).toBe('boom');
  });

  test('falls back for anything else', () => {
    expect(errorMessage(undefined, 'Retry failed')).toBe('Retry failed');
    expect(errorMessage({ code: 1 })).toBe('Unknown error');
  });
});
