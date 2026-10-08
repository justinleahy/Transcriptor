import type { ViewKind } from "../App";

interface SidebarProps {
  current: ViewKind;
  recording: boolean;
  elapsedLabel: string;
  onNavigate: (view: ViewKind) => void;
}

const ITEMS: { kind: ViewKind; label: string }[] = [
  { kind: "record", label: "Record" },
  { kind: "library", label: "Library" },
  { kind: "settings", label: "Settings" },
];

export function Sidebar({ current, recording, elapsedLabel, onNavigate }: SidebarProps) {
  return (
    <nav className="sidebar" aria-label="Main">
      <div className="sidebar-brand">
        <span className="brand-mark" aria-hidden="true" />
        <span className="brand-name">Transcriptor</span>
      </div>
      <ul className="nav-list">
        {ITEMS.map((item) => {
          const active =
            current === item.kind || (item.kind === "library" && current === "meeting");
          return (
            <li key={item.kind}>
              <button
                type="button"
                className={`nav-item${active ? " is-active" : ""}`}
                onClick={() => onNavigate(item.kind)}
                aria-current={active ? "page" : undefined}
              >
                <span className="nav-label">{item.label}</span>
                {item.kind === "record" && recording && (
                  <span className="nav-recording">
                    <span className="rec-dot" aria-hidden="true" />
                    <span className="mono">{elapsedLabel}</span>
                  </span>
                )}
              </button>
            </li>
          );
        })}
      </ul>
      <div className="sidebar-footer">
        {recording ? (
          <span className="status-pill status-recording">
            <span className="rec-dot" aria-hidden="true" /> Recording
          </span>
        ) : (
          <span className="status-pill">Idle</span>
        )}
      </div>
    </nav>
  );
}
