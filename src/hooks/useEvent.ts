import { useEffect, useRef } from "react";
import type { UnlistenFn } from "@tauri-apps/api/event";
import { onEvent } from "../api";
import type { AppEventName, EventPayloads } from "../types";

/**
 * Subscribe to a backend event for the lifetime of the component.
 * The handler is kept in a ref so callers don't need to memoize it, and the
 * Tauri unlisten function is always called on unmount (including the case
 * where the component unmounts before `listen` resolves).
 */
export function useEvent<N extends AppEventName>(
  name: N,
  handler: (payload: EventPayloads[N]) => void,
): void {
  const handlerRef = useRef(handler);
  handlerRef.current = handler;

  useEffect(() => {
    let unlisten: UnlistenFn | null = null;
    let cancelled = false;
    onEvent(name, (payload) => handlerRef.current(payload))
      .then((fn) => {
        if (cancelled) fn();
        else unlisten = fn;
      })
      .catch((err) => {
        console.error(`Failed to listen for ${name}`, err);
      });
    return () => {
      cancelled = true;
      if (unlisten) unlisten();
    };
  }, [name]);
}
