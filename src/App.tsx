import { useCallback, useEffect, useRef, useState } from "react";
import "./App.css";
import { ToastProvider } from "./components/Toast";
import { Sidebar } from "./components/Sidebar";
import { RecordView } from "./components/RecordView";
import { LibraryView } from "./components/LibraryView";
import { MeetingDetailView } from "./components/MeetingDetailView";
import { SettingsView } from "./components/SettingsView";
import { useRecording } from "./hooks/useRecording";
import { useToast } from "./hooks/useToast";
import { formatClock } from "./lib/format";

export type ViewKind = "record" | "library" | "meeting" | "settings";

type View =
  | { kind: "record" }
  | { kind: "library" }
  | { kind: "meeting"; id: string }
  | { kind: "settings" };

export default function App() {
  return (
    <ToastProvider>
      <Shell />
    </ToastProvider>
  );
}

function Shell() {
  const toast = useToast();
  const onError = useCallback((message: string) => toast.push(message, "error"), [toast]);
  const recording = useRecording(onError);
  const [view, setView] = useState<View>({ kind: "library" });

  // If a recording is already running when the app opens, land on Record.
  const autoNavigated = useRef(false);
  useEffect(() => {
    if (recording.status.active && !autoNavigated.current) {
      autoNavigated.current = true;
      setView({ kind: "record" });
    }
  }, [recording.status.active]);

  const navigate = useCallback((kind: ViewKind) => {
    if (kind === "meeting") return; // meetings are opened by id from the library
    setView({ kind });
  }, []);

  const openMeeting = useCallback((id: string) => setView({ kind: "meeting", id }), []);
  const goLibrary = useCallback(() => setView({ kind: "library" }), []);

  return (
    <div className="app">
      <Sidebar
        current={view.kind}
        recording={recording.status.active}
        elapsedLabel={formatClock(recording.status.elapsed_ms)}
        onNavigate={navigate}
      />
      <main className="main">
        {view.kind === "record" && (
          <RecordView recording={recording} onStopped={(m) => openMeeting(m.id)} />
        )}
        {view.kind === "library" && <LibraryView onOpen={openMeeting} />}
        {view.kind === "meeting" && (
          <MeetingDetailView
            key={view.id}
            id={view.id}
            onBack={goLibrary}
            onDeleted={goLibrary}
          />
        )}
        {view.kind === "settings" && <SettingsView />}
      </main>
    </div>
  );
}
