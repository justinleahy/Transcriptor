import { useCallback, useEffect, useMemo, useState, type KeyboardEvent } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { api } from "../api";
import { useEvent } from "../hooks/useEvent";
import { useToast } from "../hooks/useToast";
import { formatClock, formatDate, formatDuration, speakerLabel } from "../lib/format";
import type { Meeting, MeetingDetail, Speaker, TranscriptSegment } from "../types";
import { Markdown } from "./Markdown";
import { StatusBadge } from "./StatusBadge";

interface MeetingDetailViewProps {
  id: string;
  onBack: () => void;
  onDeleted: () => void;
}

interface SpeakerGroup {
  speaker: Speaker;
  start_ms: number;
  segments: TranscriptSegment[];
}

function groupBySpeaker(segments: TranscriptSegment[]): SpeakerGroup[] {
  const groups: SpeakerGroup[] = [];
  for (const seg of segments) {
    const last = groups[groups.length - 1];
    if (last && last.speaker === seg.speaker) {
      last.segments.push(seg);
    } else {
      groups.push({ speaker: seg.speaker, start_ms: seg.start_ms, segments: [seg] });
    }
  }
  return groups;
}

function ExportChip({ meeting }: { meeting: Meeting }) {
  switch (meeting.export_status) {
    case "awaiting_approval":
      return <span className="badge badge-processing">Awaiting approval</span>;
    case "pending":
      return (
        <span className="badge badge-processing">
          <span className="spinner spinner-xs" aria-hidden="true" />
          Pending
        </span>
      );
    case "exported":
      return (
        <span className="badge badge-done">
          Exported{meeting.exported_at ? ` ${formatDate(meeting.exported_at)}` : ""}
        </span>
      );
    case "failed": {
      const text = `Failed: ${meeting.export_error ?? "unknown error"}`;
      return (
        <span className="badge badge-failed export-chip-failed" title={text}>
          {text}
        </span>
      );
    }
    default:
      return <span className="badge">Never exported</span>;
  }
}

function transcriptAsText(groups: SpeakerGroup[]): string {
  return groups
    .map(
      (g) =>
        `[${formatClock(g.start_ms)}] ${speakerLabel(g.speaker)}: ${g.segments
          .map((s) => s.text.trim())
          .join(" ")}`,
    )
    .join("\n");
}

export function MeetingDetailView({ id, onBack, onDeleted }: MeetingDetailViewProps) {
  const toast = useToast();
  const [detail, setDetail] = useState<MeetingDetail | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [titleDraft, setTitleDraft] = useState("");
  const [summarizing, setSummarizing] = useState(false);
  const [confirmingDelete, setConfirmingDelete] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [copied, setCopied] = useState(false);
  const [exporting, setExporting] = useState(false);
  // null while loading; the export card is hidden for users who don't use SecondBrain.
  const [secondbrainEnabled, setSecondbrainEnabled] = useState<boolean | null>(null);

  useEffect(() => {
    let cancelled = false;
    api
      .getSettings()
      .then((s) => {
        if (!cancelled) setSecondbrainEnabled(s.secondbrain_enabled);
      })
      .catch(() => {
        if (!cancelled) setSecondbrainEnabled(false);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const load = useCallback(async () => {
    try {
      const d = await api.getMeeting(id);
      setDetail(d);
      setTitleDraft(d.meeting.title);
      setLoadError(null);
    } catch (err) {
      setLoadError(typeof err === "string" ? err : "Failed to load meeting");
      toast.error(err);
    }
  }, [id, toast]);

  useEffect(() => {
    setDetail(null);
    setConfirmingDelete(false);
    void load();
  }, [load]);

  // Status/summary changes from the backend (e.g. transcription finished).
  useEvent("meeting:updated", (meeting) => {
    if (meeting.id !== id) return;
    // Refetch so we also pick up segments written after stop.
    void load();
  });

  // Live segments while this meeting is still recording.
  useEvent("transcript:segment", (seg) => {
    if (seg.meeting_id !== id) return;
    setDetail((prev) => {
      if (!prev) return prev;
      if (prev.segments.some((s) => s.id === seg.id)) return prev;
      const segments = [...prev.segments, seg].sort((a, b) => a.start_ms - b.start_ms);
      return { ...prev, segments };
    });
  });

  const groups = useMemo(() => groupBySpeaker(detail?.segments ?? []), [detail]);

  async function commitTitle() {
    if (!detail) return;
    const next = titleDraft.trim();
    if (!next || next === detail.meeting.title) {
      setTitleDraft(detail.meeting.title);
      return;
    }
    try {
      const meeting = await api.renameMeeting(id, next);
      setDetail((prev) => (prev ? { ...prev, meeting } : prev));
      setTitleDraft(meeting.title);
    } catch (err) {
      toast.error(err);
      setTitleDraft(detail.meeting.title);
    }
  }

  function handleTitleKey(e: KeyboardEvent<HTMLInputElement>) {
    if (e.key === "Enter") {
      e.currentTarget.blur();
    } else if (e.key === "Escape") {
      setTitleDraft(detail?.meeting.title ?? "");
      e.currentTarget.blur();
    }
  }

  async function handleSummarize() {
    if (summarizing) return;
    setSummarizing(true);
    try {
      const meeting = await api.summarizeMeeting(id);
      setDetail((prev) => (prev ? { ...prev, meeting } : prev));
      toast.push("Summary ready", "success");
    } catch (err) {
      toast.error(err);
    } finally {
      setSummarizing(false);
    }
  }

  async function handleExport() {
    if (exporting) return;
    setExporting(true);
    try {
      const meeting = await api.exportMeeting(id);
      setDetail((prev) => (prev ? { ...prev, meeting } : prev));
      toast.push(
        meeting.export_status === "awaiting_approval"
          ? "Update awaiting approval in SecondBrain"
          : "Exported to SecondBrain",
        meeting.export_status === "awaiting_approval" ? "info" : "success",
      );
    } catch (err) {
      toast.error(err);
      // The failure is recorded on the meeting; refetch to show it.
      void load();
    } finally {
      setExporting(false);
    }
  }

  async function handleCopy() {
    try {
      await navigator.clipboard.writeText(transcriptAsText(groups));
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1500);
    } catch (err) {
      toast.error(err);
    }
  }

  async function handleDelete() {
    setDeleting(true);
    try {
      await api.deleteMeeting(id);
      toast.push("Meeting deleted", "info");
      onDeleted();
    } catch (err) {
      toast.error(err);
      setDeleting(false);
      setConfirmingDelete(false);
    }
  }

  if (loadError && !detail) {
    return (
      <section className="view detail-view">
        <button type="button" className="btn btn-ghost btn-sm back-link" onClick={onBack}>
          &larr; Library
        </button>
        <div className="empty empty-lg">
          <strong>Couldn't load this meeting</strong>
          <span className="muted">{loadError}</span>
          <button type="button" className="btn" onClick={() => void load()}>
            Retry
          </button>
        </div>
      </section>
    );
  }

  if (!detail) {
    return (
      <section className="view detail-view">
        <button type="button" className="btn btn-ghost btn-sm back-link" onClick={onBack}>
          &larr; Library
        </button>
        <div className="empty">Loading...</div>
      </section>
    );
  }

  const { meeting, segments } = detail;
  const isRecording = meeting.status === "recording";
  const canSummarize = segments.length > 0 && !isRecording && !summarizing;
  const isProcessing = meeting.status === "processing";
  const exportPending = exporting || meeting.export_status === "pending";
  const canExport =
    secondbrainEnabled === true &&
    segments.length > 0 &&
    !isRecording &&
    !isProcessing &&
    !exportPending;
  const showExport = secondbrainEnabled === true || meeting.export_status !== null;

  return (
    <section className="view detail-view">
      <button type="button" className="btn btn-ghost btn-sm back-link" onClick={onBack}>
        &larr; Library
      </button>

      <header className="detail-header">
        <input
          className="title-input"
          type="text"
          value={titleDraft}
          onChange={(e) => setTitleDraft(e.target.value)}
          onBlur={() => void commitTitle()}
          onKeyDown={handleTitleKey}
          aria-label="Meeting title"
          placeholder="Untitled meeting"
        />
        <div className="detail-meta muted small">
          <span>{formatDate(meeting.started_at)}</span>
          <span aria-hidden="true">·</span>
          <span>{formatDuration(meeting.duration_ms)}</span>
          <span aria-hidden="true">·</span>
          <StatusBadge status={meeting.status} />
          <span aria-hidden="true">·</span>
          <span className="capitalize">{meeting.transcription_mode} transcription</span>
        </div>
      </header>

      {meeting.audio_path && (
        <div className="audio-row">
          <audio controls preload="metadata" src={convertFileSrc(meeting.audio_path)} />
        </div>
      )}

      {showExport && (
        <div className="card export-card">
          <div className="card-header">
            <div className="row-gap export-status">
              <h2>SecondBrain</h2>
              <ExportChip meeting={meeting} />
            </div>
            <button
              type="button"
              className="btn btn-sm"
              onClick={handleExport}
              disabled={!canExport}
              title={
                secondbrainEnabled !== true
                  ? "Enable SecondBrain export in Settings"
                  : isRecording
                    ? "Stop the recording first"
                    : isProcessing
                      ? "Wait for transcription to finish"
                      : segments.length === 0
                        ? "No transcript to export"
                        : undefined
              }
            >
              {exportPending && <span className="spinner spinner-xs" aria-hidden="true" />}
              {exportPending
                ? "Exporting..."
                : meeting.export_status === "awaiting_approval"
                  ? "Check approval"
                : meeting.export_document_id
                  ? "Re-export"
                  : "Export to SecondBrain"}
            </button>
          </div>
          {meeting.export_status === "awaiting_approval" && (
            <p className="muted small">{meeting.export_error}</p>
          )}
          {(meeting.export_document_id || meeting.export_summary_document_id) && (
            <p className="muted small export-docs">
              {meeting.export_document_id && (
                <span>
                  Transcript <code>{meeting.export_document_id}</code>
                </span>
              )}
              {meeting.export_document_id && meeting.export_summary_document_id && " · "}
              {meeting.export_summary_document_id && (
                <span>
                  Notes <code>{meeting.export_summary_document_id}</code>
                </span>
              )}
              {meeting.summary && !meeting.export_summary_document_id && " · Notes not exported yet"}
            </p>
          )}
        </div>
      )}

      <div className="card summary-card">
        <div className="card-header">
          <h2>Summary</h2>
          <div className="row-gap">
            {meeting.summarized_at && (
              <span className="muted small">Summarized {formatDate(meeting.summarized_at)}</span>
            )}
            <button
              type="button"
              className="btn btn-primary btn-sm"
              onClick={handleSummarize}
              disabled={!canSummarize}
              title={
                segments.length === 0
                  ? "No transcript to summarize yet"
                  : isRecording
                    ? "Stop the recording first"
                    : undefined
              }
            >
              {summarizing && <span className="spinner spinner-xs spinner-inverse" aria-hidden="true" />}
              {summarizing
                ? "Summarizing..."
                : meeting.summary
                  ? "Re-summarize with Claude"
                  : "Summarize with Claude"}
            </button>
          </div>
        </div>
        {meeting.summary ? (
          <Markdown source={meeting.summary} />
        ) : (
          <p className="muted small">
            {summarizing
              ? "Claude is reading the transcript. This can take up to a minute."
              : "No summary yet. Generate one with Claude once the transcript is complete."}
          </p>
        )}
      </div>

      <div className="card transcript-card">
        <div className="card-header">
          <h2>
            Transcript{" "}
            <span className="muted small normal">
              {segments.length} {segments.length === 1 ? "segment" : "segments"}
            </span>
          </h2>
          <button
            type="button"
            className="btn btn-sm"
            onClick={handleCopy}
            disabled={segments.length === 0}
          >
            {copied ? "Copied" : "Copy transcript"}
          </button>
        </div>
        {segments.length === 0 ? (
          <p className="muted small">
            {isRecording
              ? "Recording in progress; segments will appear as they are transcribed."
              : meeting.status === "processing"
                ? "Transcription is still finishing up."
                : "No transcript segments."}
          </p>
        ) : (
          <div className="transcript-groups">
            {groups.map((g, idx) => (
              <div key={`${g.speaker}-${g.start_ms}-${idx}`} className={`group speaker-${g.speaker}`}>
                <div className="group-head">
                  <span className={`chip chip-${g.speaker}`}>{speakerLabel(g.speaker)}</span>
                  <span className="segment-time mono muted">{formatClock(g.start_ms)}</span>
                </div>
                <p className="group-text">
                  {g.segments.map((s) => (
                    <span key={s.id} className="group-seg" title={formatClock(s.start_ms)}>
                      {s.text.trim()}{" "}
                    </span>
                  ))}
                </p>
              </div>
            ))}
          </div>
        )}
      </div>

      <div className="danger-zone">
        {confirmingDelete ? (
          <div className="confirm-row">
            <span>Delete this meeting and its audio file? This cannot be undone.</span>
            <button
              type="button"
              className="btn btn-danger btn-sm"
              onClick={handleDelete}
              disabled={deleting}
            >
              {deleting ? "Deleting..." : "Yes, delete"}
            </button>
            <button
              type="button"
              className="btn btn-ghost btn-sm"
              onClick={() => setConfirmingDelete(false)}
              disabled={deleting}
            >
              Cancel
            </button>
          </div>
        ) : (
          <button
            type="button"
            className="btn btn-ghost btn-sm text-danger"
            onClick={() => setConfirmingDelete(true)}
            disabled={isRecording}
            title={isRecording ? "Stop the recording first" : undefined}
          >
            Delete meeting
          </button>
        )}
      </div>
    </section>
  );
}
