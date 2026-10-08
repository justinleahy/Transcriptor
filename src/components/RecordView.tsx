import { useEffect, useRef, useState, type FormEvent } from "react";
import type { RecordingState } from "../hooks/useRecording";
import type { Meeting } from "../types";
import { formatClock, speakerLabel } from "../lib/format";
import { LevelMeter } from "./LevelMeter";

interface RecordViewProps {
  recording: RecordingState;
  onStopped: (meeting: Meeting) => void;
}

export function RecordView({ recording, onStopped }: RecordViewProps) {
  const { status, segments, levels, warning, busy, start, stop, dismissWarning } =
    recording;
  const [title, setTitle] = useState("");
  const [autoScroll, setAutoScroll] = useState(true);
  const listRef = useRef<HTMLDivElement>(null);

  // Auto-scroll the live transcript unless the user scrolled up.
  useEffect(() => {
    const el = listRef.current;
    if (!el || !autoScroll) return;
    el.scrollTop = el.scrollHeight;
  }, [segments, autoScroll]);

  function handleScroll() {
    const el = listRef.current;
    if (!el) return;
    const distanceFromBottom = el.scrollHeight - el.scrollTop - el.clientHeight;
    setAutoScroll(distanceFromBottom < 40);
  }

  async function handleStart(e?: FormEvent) {
    e?.preventDefault();
    const trimmed = title.trim();
    const meeting = await start(trimmed ? trimmed : null);
    if (meeting) {
      setTitle("");
      setAutoScroll(true);
    }
  }

  async function handleStop() {
    const meeting = await stop();
    if (meeting) onStopped(meeting);
  }

  const active = status.active;

  return (
    <section className="view record-view">
      <header className="view-header">
        <h1>Record</h1>
        <p className="muted">
          Captures your microphone and, when enabled, the other participants from
          system audio. Transcription runs live.
        </p>
      </header>

      {warning && (
        <div className="banner banner-warning" role="status">
          <span>{warning}</span>
          <button type="button" className="btn btn-ghost btn-sm" onClick={dismissWarning}>
            Dismiss
          </button>
        </div>
      )}

      <div className="record-panel">
        <div className="record-controls">
          {active ? (
            <>
              <div className="timer" aria-live="off">
                <span className="rec-dot rec-dot-lg" aria-hidden="true" />
                <span className="timer-value mono">{formatClock(status.elapsed_ms)}</span>
              </div>
              <button
                type="button"
                className="btn btn-record is-recording"
                onClick={handleStop}
                disabled={busy}
              >
                <span className="btn-record-icon stop" aria-hidden="true" />
                {busy ? "Stopping..." : "Stop"}
              </button>
              {!status.system_audio && (
                <p className="hint">
                  Only your microphone is being recorded; system audio is unavailable.
                </p>
              )}
            </>
          ) : (
            <form className="start-form" onSubmit={handleStart}>
              <input
                className="input input-lg"
                type="text"
                placeholder="Meeting title (optional)"
                value={title}
                onChange={(e) => setTitle(e.target.value)}
                disabled={busy}
                autoComplete="off"
              />
              <button type="submit" className="btn btn-record" disabled={busy}>
                <span className="btn-record-icon" aria-hidden="true" />
                {busy ? "Starting..." : "Start recording"}
              </button>
            </form>
          )}
        </div>

        <div className="meters">
          <LevelMeter label="You" level={levels.mic} muted={!active} />
          <LevelMeter
            label="Others"
            level={levels.system}
            muted={!active || !status.system_audio}
          />
        </div>
      </div>

      <div className="transcript-live card">
        <div className="card-header">
          <h2>Live transcript</h2>
          {segments.length > 0 && (
            <span className="muted small">{segments.length} segments</span>
          )}
        </div>
        <div className="transcript-scroll" ref={listRef} onScroll={handleScroll}>
          {segments.length === 0 ? (
            <div className="empty">
              {active
                ? "Listening... transcript segments will appear here as they are recognized."
                : "Start a recording to see the live transcript."}
            </div>
          ) : (
            <ol className="segment-list">
              {segments.map((seg) => (
                <li key={seg.id} className={`segment speaker-${seg.speaker}`}>
                  <span className={`chip chip-${seg.speaker}`}>
                    {speakerLabel(seg.speaker)}
                  </span>
                  <span className="segment-time mono">{formatClock(seg.start_ms)}</span>
                  <span className="segment-text">{seg.text}</span>
                </li>
              ))}
            </ol>
          )}
        </div>
        {!autoScroll && segments.length > 0 && (
          <button
            type="button"
            className="btn btn-sm jump-to-latest"
            onClick={() => {
              setAutoScroll(true);
              const el = listRef.current;
              if (el) el.scrollTop = el.scrollHeight;
            }}
          >
            Jump to latest
          </button>
        )}
      </div>
    </section>
  );
}
