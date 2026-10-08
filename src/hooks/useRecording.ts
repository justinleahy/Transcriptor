import { useCallback, useEffect, useRef, useState } from "react";
import { api, errorMessage } from "../api";
import { useEvent } from "./useEvent";
import type {
  Levels,
  Meeting,
  RecordingStatus,
  TranscriptSegment,
} from "../types";

const IDLE_STATUS: RecordingStatus = {
  active: false,
  meeting_id: null,
  elapsed_ms: 0,
  system_audio: false,
};

const ZERO_LEVELS: Levels = { mic: 0, system: 0 };

export interface RecordingState {
  status: RecordingStatus;
  segments: TranscriptSegment[];
  levels: Levels;
  warning: string | null;
  busy: boolean;
  start: (title: string | null) => Promise<Meeting | null>;
  stop: () => Promise<Meeting | null>;
  dismissWarning: () => void;
}

/**
 * Owns the recording lifecycle: subscribes to recording/transcript/level
 * events, tracks live segments for the active meeting, and exposes start/stop.
 * Mount once near the app root so state survives view switches.
 */
export function useRecording(onError: (message: string) => void): RecordingState {
  const [status, setStatus] = useState<RecordingStatus>(IDLE_STATUS);
  const [segments, setSegments] = useState<TranscriptSegment[]>([]);
  const [levels, setLevels] = useState<Levels>(ZERO_LEVELS);
  const [warning, setWarning] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // Meeting id the live transcript belongs to (kept in a ref so event
  // handlers always see the latest value without re-subscribing).
  const activeMeetingRef = useRef<string | null>(null);

  const beginMeeting = useCallback((meetingId: string | null) => {
    activeMeetingRef.current = meetingId;
    setSegments([]);
    setWarning(null);
  }, []);

  // Sync with the backend on mount (the app may open while a recording runs).
  useEffect(() => {
    let cancelled = false;
    api
      .recordingStatus()
      .then((s) => {
        if (cancelled) return;
        if (s.active && s.meeting_id) activeMeetingRef.current = s.meeting_id;
        setStatus(s);
      })
      .catch((err) => {
        console.warn("recording_status failed", err);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  useEvent("recording:status", (s) => {
    if (s.active && s.meeting_id && s.meeting_id !== activeMeetingRef.current) {
      beginMeeting(s.meeting_id);
    }
    setStatus(s);
    if (!s.active) setLevels(ZERO_LEVELS);
  });

  useEvent("transcript:segment", (seg) => {
    const current = activeMeetingRef.current;
    if (current && seg.meeting_id !== current) return;
    setSegments((prev) => {
      if (prev.some((p) => p.id === seg.id)) {
        return prev.map((p) => (p.id === seg.id ? seg : p));
      }
      const next = [...prev, seg];
      next.sort((a, b) => a.start_ms - b.start_ms);
      return next;
    });
  });

  useEvent("audio:levels", (l) => setLevels(l));

  useEvent("recording:warning", ({ message }) => setWarning(message));

  const start = useCallback(
    async (title: string | null): Promise<Meeting | null> => {
      setBusy(true);
      try {
        const meeting = await api.startRecording(title);
        beginMeeting(meeting.id);
        setStatus({
          active: true,
          meeting_id: meeting.id,
          elapsed_ms: 0,
          system_audio: true,
        });
        return meeting;
      } catch (err) {
        onError(errorMessage(err));
        return null;
      } finally {
        setBusy(false);
      }
    },
    [onError, beginMeeting],
  );

  const stop = useCallback(async (): Promise<Meeting | null> => {
    setBusy(true);
    try {
      const meeting = await api.stopRecording();
      setStatus((prev) => ({ ...prev, active: false }));
      setLevels(ZERO_LEVELS);
      return meeting;
    } catch (err) {
      onError(errorMessage(err));
      return null;
    } finally {
      setBusy(false);
    }
  }, [onError]);

  const dismissWarning = useCallback(() => setWarning(null), []);

  return { status, segments, levels, warning, busy, start, stop, dismissWarning };
}
