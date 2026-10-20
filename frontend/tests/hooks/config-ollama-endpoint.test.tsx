import { afterAll, describe, expect, mock, test } from 'bun:test';
import { act, create } from 'react-test-renderer';

const originalCore = { ...await import('@tauri-apps/api/core') };
const originalEvent = { ...await import('@tauri-apps/api/event') };
const originalAnalytics = { ...await import('../../src/lib/analytics') };
afterAll(() => {
  mock.module('@tauri-apps/api/core', () => originalCore);
  mock.module('@tauri-apps/api/event', () => originalEvent);
  mock.module('../../src/lib/analytics', () => originalAnalytics);
});

if (typeof globalThis.localStorage === 'undefined') {
  const store = new Map<string, string>();
  (globalThis as any).localStorage = {
    getItem: (k: string) => store.get(k) ?? null,
    setItem: (k: string, v: string) => { store.set(k, v); },
    removeItem: (k: string) => { store.delete(k); },
  };
}

mock.module('../../src/lib/analytics', () => ({ default: new Proxy({}, { get: () => () => {} }) }));
mock.module('@tauri-apps/api/event', () => ({ ...originalEvent, listen: async () => () => {} }));

let storedProvider = 'custom-openai';
mock.module('@tauri-apps/api/core', () => ({
  ...originalCore,
  invoke: async (command: string) => {
    switch (command) {
      case 'api_get_model_config':
        return { provider: storedProvider, model: 'm', whisperModel: 'large-v3', apiKey: null, ollamaEndpoint: 'http://gpu-box:11434' };
      case 'api_get_custom_openai_config':
        return { endpoint: 'http://llm.local/v1', model: 'm', apiKey: null, maxTokens: null, temperature: null, topP: null };
      case 'api_get_copilot_cli_config':
        return { binaryPath: null, model: 'auto', githubToken: null };
      case 'get_ollama_models':
        return [];
      default:
        return null;
    }
  },
}));

const { ConfigProvider, useConfig } = await import('../../src/contexts/ConfigContext');

let context!: ReturnType<typeof useConfig>;
function Probe() {
  context = useConfig();
  return null;
}
const flush = () => act(async () => { await new Promise((resolve) => setTimeout(resolve, 0)); });

// ModelSettingsModal saves modelConfig.ollamaEndpoint for every non-Ollama provider,
// so whatever the context loads here is what gets written back.
describe('ConfigContext model config load', () => {
  for (const provider of ['custom-openai', 'copilot-cli']) {
    test(`keeps the stored Ollama endpoint when the active provider is ${provider}`, async () => {
      storedProvider = provider;
      let renderer: ReturnType<typeof create> | undefined;
      await act(async () => { renderer = create(<ConfigProvider><Probe /></ConfigProvider>); });
      await flush();
      expect(context.modelConfig.provider).toBe(provider);
      expect(context.modelConfig.ollamaEndpoint).toBe('http://gpu-box:11434');
      await act(async () => { renderer!.unmount(); });
    });
  }
});
