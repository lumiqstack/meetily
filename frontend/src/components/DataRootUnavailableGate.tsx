'use client';

import { useEffect, useState, type ReactNode } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { HardDrive } from 'lucide-react';

type Status = { kind: 'checking' } | { kind: 'available' } | { kind: 'unavailable'; path: string };

/**
 * Blocks the app while the configured data root (e.g. an external or network
 * drive) is not reachable. The app is then running from a fallback folder that
 * holds none of the user's meetings, so anything done there would be lost.
 */
export function DataRootUnavailableGate({ children }: { children: ReactNode }) {
  const [status, setStatus] = useState<Status>({ kind: 'checking' });

  useEffect(() => {
    invoke<string | null>('get_unavailable_data_root')
      .then((path) => setStatus(path ? { kind: 'unavailable', path } : { kind: 'available' }))
      .catch((error) => {
        console.error('[DataRootUnavailableGate] Failed to check the data root:', error);
        setStatus({ kind: 'available' });
      });
  }, []);

  if (status.kind === 'checking') return null;
  if (status.kind === 'available') return <>{children}</>;

  return (
    <div className="flex h-screen w-full items-center justify-center bg-gray-50 p-6">
      <div role="alert" className="max-w-md rounded-lg border border-gray-200 bg-white p-6 shadow-sm">
        <div className="mb-3 flex items-center gap-2 text-gray-900">
          <HardDrive className="h-5 w-5" />
          <h1 className="text-lg font-semibold">Meetings folder not available</h1>
        </div>
        <p className="text-sm text-gray-700">
          Your meetings folder <span className="break-all font-mono">{status.path}</span> is not
          available. Connect the drive and restart Meetily.
        </p>
      </div>
    </div>
  );
}
