import { useCallback, useMemo, useRef, useState, type ReactNode } from "react";
import { errorMessage } from "../api";
import {
  ToastContext,
  type Toast,
  type ToastApi,
  type ToastKind,
} from "../hooks/useToast";

const AUTO_DISMISS_MS: Record<ToastKind, number> = {
  error: 10000,
  info: 4000,
  success: 3000,
};

export function ToastProvider({ children }: { children: ReactNode }) {
  const [toasts, setToasts] = useState<Toast[]>([]);
  const nextId = useRef(1);

  const dismiss = useCallback((id: number) => {
    setToasts((prev) => prev.filter((t) => t.id !== id));
  }, []);

  const push = useCallback(
    (message: string, kind: ToastKind = "info") => {
      const id = nextId.current++;
      setToasts((prev) => [...prev, { id, kind, message }]);
      window.setTimeout(() => dismiss(id), AUTO_DISMISS_MS[kind]);
    },
    [dismiss],
  );

  const error = useCallback(
    (err: unknown) => push(errorMessage(err), "error"),
    [push],
  );

  // Stable identity: only depends on stable callbacks, never on `toasts`.
  const value = useMemo<ToastApi>(() => ({ push, error, dismiss }), [push, error, dismiss]);

  return (
    <ToastContext.Provider value={value}>
      {children}
      <ToastViewport toasts={toasts} onDismiss={dismiss} />
    </ToastContext.Provider>
  );
}

function ToastViewport({
  toasts,
  onDismiss,
}: {
  toasts: Toast[];
  onDismiss: (id: number) => void;
}) {
  if (toasts.length === 0) return null;
  return (
    <div className="toast-viewport" role="region" aria-label="Notifications">
      {toasts.map((t) => (
        <div key={t.id} className={`toast toast-${t.kind}`} role="alert">
          <span className="toast-message">{t.message}</span>
          <button
            type="button"
            className="toast-close"
            aria-label="Dismiss"
            onClick={() => onDismiss(t.id)}
          >
            &times;
          </button>
        </div>
      ))}
    </div>
  );
}
