import { useCallback, useEffect, useState } from "react";
import { api } from "../api";
import { useDebouncedValue } from "../hooks/useDebouncedValue";
import { useEvent } from "../hooks/useEvent";
import { useToast } from "../hooks/useToast";
import { formatDateShort, formatDuration } from "../lib/format";
import { renderSnippet } from "../lib/snippet";
import type { MeetingListItem } from "../types";
import { StatusBadge } from "./StatusBadge";

interface LibraryViewProps {
  onOpen: (id: string) => void;
}

export function LibraryView({ onOpen }: LibraryViewProps) {
  const toast = useToast();
  const [query, setQuery] = useState("");
  const debouncedQuery = useDebouncedValue(query, 250);
  const [items, setItems] = useState<MeetingListItem[] | null>(null);
  const [loading, setLoading] = useState(false);

  const load = useCallback(
    async (q: string) => {
      setLoading(true);
      try {
        const trimmed = q.trim();
        const result = await api.listMeetings(trimmed ? trimmed : null);
        setItems(result);
      } catch (err) {
        toast.error(err);
        setItems((prev) => prev ?? []);
      } finally {
        setLoading(false);
      }
    },
    [toast],
  );

  useEffect(() => {
    void load(debouncedQuery);
  }, [debouncedQuery, load]);

  // Refresh when a meeting changes status (e.g. transcription finished).
  useEvent("meeting:updated", () => {
    void load(debouncedQuery);
  });
  useEvent("recording:status", (s) => {
    if (!s.active) void load(debouncedQuery);
  });

  const searching = debouncedQuery.trim().length > 0;

  return (
    <section className="view library-view">
      <header className="view-header row-between">
        <div>
          <h1>Library</h1>
          <p className="muted">All recorded meetings, newest first.</p>
        </div>
      </header>

      <div className="search-row">
        <input
          className="input input-search"
          type="search"
          placeholder="Search titles and transcripts"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          aria-label="Search meetings"
        />
        {loading && <span className="spinner" aria-label="Loading" />}
      </div>

      {items === null ? (
        <div className="empty">Loading...</div>
      ) : items.length === 0 ? (
        <div className="empty empty-lg">
          {searching ? (
            <>
              <strong>No matches</strong>
              <span className="muted">Nothing in your meetings matches "{debouncedQuery}".</span>
            </>
          ) : (
            <>
              <strong>No meetings yet</strong>
              <span className="muted">
                Start a recording from the Record tab and it will show up here.
              </span>
            </>
          )}
        </div>
      ) : (
        <ul className="meeting-list">
          {items.map(({ meeting, segment_count, snippet }) => (
            <li key={meeting.id}>
              <button
                type="button"
                className="meeting-row"
                onClick={() => onOpen(meeting.id)}
              >
                <div className="meeting-row-main">
                  <span className="meeting-title">
                    {meeting.title || "Untitled meeting"}
                  </span>
                  <StatusBadge status={meeting.status} />
                </div>
                <div className="meeting-row-meta muted small">
                  <span>{formatDateShort(meeting.started_at)}</span>
                  <span aria-hidden="true">·</span>
                  <span>{formatDuration(meeting.duration_ms)}</span>
                  <span aria-hidden="true">·</span>
                  <span>
                    {segment_count} {segment_count === 1 ? "segment" : "segments"}
                  </span>
                </div>
                {snippet && <p className="meeting-snippet">{renderSnippet(snippet)}</p>}
              </button>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
