import { afterAll, describe, expect, mock, test } from 'bun:test';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';

// H8-F4: open_system_settings takes a required `preference_pane` argument
// (src-tauri/src/utils.rs). Calling it without one always rejects, so the
// "Open Settings" buttons fell straight into the alert fallback.
const originalCore = { ...await import('@tauri-apps/api/core') };
const originalOnboarding = { ...await import('../../src/contexts/OnboardingContext') };
afterAll(() => {
  mock.module('@tauri-apps/api/core', () => originalCore);
  mock.module('../../src/contexts/OnboardingContext', () => originalOnboarding);
});

const invoke = mock(async (_command: string, _args?: Record<string, unknown>): Promise<unknown> => undefined);
mock.module('@tauri-apps/api/core', () => ({ ...originalCore, invoke }));
mock.module('../../src/contexts/OnboardingContext', () => ({
  ...originalOnboarding,
  useOnboarding: () => ({
    setPermissionStatus: () => {},
    setPermissionsSkipped: () => {},
    completeOnboarding: async () => {},
    permissions: { microphone: 'denied', systemAudio: 'denied' },
  }),
}));
const { PermissionsStep } = await import('../../src/components/onboarding/steps/PermissionsStep');

async function clickOpenSettings(index: number) {
  let renderer: ReactTestRenderer | undefined;
  await act(async () => {
    renderer = create(<PermissionsStep />);
  });
  const buttons = renderer!.root.findAll(
    (node) => node.type === 'button' && JSON.stringify(node.props.children ?? '').includes('Open Settings'),
  );
  expect(buttons).toHaveLength(2);
  await act(async () => {
    await buttons[index].props.onClick();
  });
  await act(async () => renderer!.unmount());
}

describe('permissions step Open Settings buttons', () => {
  test('microphone button opens the Microphone privacy pane', async () => {
    invoke.mockClear();
    await clickOpenSettings(0);
    expect(invoke.mock.calls).toEqual([['open_system_settings', { preferencePane: 'Privacy_Microphone' }]]);
  });

  test('system audio button opens the Screen & System Audio Recording privacy pane', async () => {
    invoke.mockClear();
    await clickOpenSettings(1);
    expect(invoke.mock.calls).toEqual([['open_system_settings', { preferencePane: 'Privacy_ScreenCapture' }]]);
  });
});
