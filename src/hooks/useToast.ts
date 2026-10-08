import { createContext, useContext } from "react";

export type ToastKind = "error" | "info" | "success";

export interface Toast {
  id: number;
  kind: ToastKind;
  message: string;
}

/**
 * Stable toast API. Deliberately excludes the toast list so that the context
 * value never changes identity; components can safely list `toast` in hook
 * dependency arrays without re-running effects whenever a toast is shown.
 */
export interface ToastApi {
  push: (message: string, kind?: ToastKind) => void;
  error: (err: unknown) => void;
  dismiss: (id: number) => void;
}

export const ToastContext = createContext<ToastApi | null>(null);

export function useToast(): ToastApi {
  const ctx = useContext(ToastContext);
  if (!ctx) throw new Error("useToast must be used inside <ToastProvider>");
  return ctx;
}
