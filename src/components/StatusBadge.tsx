import type { MeetingStatus } from "../types";

const LABELS: Record<MeetingStatus, string> = {
  recording: "Recording",
  processing: "Processing",
  done: "Done",
  failed: "Failed",
};

export function StatusBadge({ status }: { status: MeetingStatus }) {
  return (
    <span className={`badge badge-${status}`}>
      {status === "recording" && <span className="rec-dot" aria-hidden="true" />}
      {status === "processing" && <span className="spinner spinner-xs" aria-hidden="true" />}
      {LABELS[status]}
    </span>
  );
}
