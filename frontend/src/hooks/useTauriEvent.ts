import { useEffect, useRef } from 'react';
import { listen } from '@tauri-apps/api/event';

/** Subscribes to a Rust-emitted event for the component's lifetime; the latest `handler` always runs. */
export function useTauriEvent<T>(name: string, handler: (payload: T) => void) {
  const handlerRef = useRef(handler);
  handlerRef.current = handler;

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let disposed = false;
    listen<T>(name, (event) => handlerRef.current(event.payload))
      .then(fn => {
        // listen() resolves after the effect may already have been cleaned up.
        if (disposed) fn();
        else unlisten = fn;
      })
      .catch(error => console.warn(`Could not listen for ${name}:`, error));
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [name]);
}
