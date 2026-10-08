interface LevelMeterProps {
  label: string;
  level: number; // 0..1 RMS
  muted?: boolean;
}

/** Compact horizontal level meter. RMS is boosted so quiet speech is visible. */
export function LevelMeter({ label, level, muted = false }: LevelMeterProps) {
  const clamped = Math.min(1, Math.max(0, level));
  // Perceptual-ish curve: sqrt lifts low RMS values into a visible range.
  const pct = muted ? 0 : Math.round(Math.sqrt(clamped) * 100);
  const hot = pct > 85;
  return (
    <div className={`meter${muted ? " is-muted" : ""}`}>
      <span className="meter-label">{label}</span>
      <div
        className="meter-track"
        role="meter"
        aria-label={`${label} level`}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={pct}
      >
        <div
          className={`meter-fill${hot ? " is-hot" : ""}`}
          style={{ width: `${pct}%` }}
        />
      </div>
    </div>
  );
}
