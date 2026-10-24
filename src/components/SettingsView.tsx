import { useCallback, useEffect, useState, type FormEvent } from "react";
import { api } from "../api";
import { useEvent } from "../hooks/useEvent";
import { useToast } from "../hooks/useToast";
import { formatBytes, formatSizeMb } from "../lib/format";
import type {
  AudioDevices,
  ModelInfo,
  ModelProgress,
  SecondBrainStatus,
  SecretName,
  Settings,
} from "../types";

interface SecretState {
  saved: boolean | null; // null = unknown/loading
  draft: string;
  busy: boolean;
}

const SYSTEM_DEFAULT = "__system_default__";
const AUTOMATIC_LOOPBACK = "__automatic_loopback__";

export function SettingsView() {
  const toast = useToast();
  const [settings, setSettings] = useState<Settings | null>(null);
  const [dirty, setDirty] = useState(false);
  const [saving, setSaving] = useState(false);
  const [models, setModels] = useState<ModelInfo[]>([]);
  const [devices, setDevices] = useState<AudioDevices | null>(null);
  const [progress, setProgress] = useState<Record<string, ModelProgress>>({});
  const [downloading, setDownloading] = useState<Set<string>>(new Set());
  const [secrets, setSecrets] = useState<Record<SecretName, SecretState>>({
    openai_api_key: { saved: null, draft: "", busy: false },
    anthropic_api_key: { saved: null, draft: "", busy: false },
    secondbrain_api_key: { saved: null, draft: "", busy: false },
  });
  // SecondBrain tags are edited as comma-separated text.
  const [tagsDraft, setTagsDraft] = useState("");
  const [testing, setTesting] = useState(false);
  const [sbStatus, setSbStatus] = useState<SecondBrainStatus | null>(null);

  const refreshModels = useCallback(async () => {
    try {
      setModels(await api.listLocalModels());
    } catch (err) {
      toast.error(err);
    }
  }, [toast]);

  const refreshSecret = useCallback(async (name: SecretName) => {
    try {
      const saved = await api.hasSecret(name);
      setSecrets((prev) => ({ ...prev, [name]: { ...prev[name], saved } }));
    } catch (err) {
      console.warn(`has_secret(${name}) failed`, err);
    }
  }, []);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const s = await api.getSettings();
        if (!cancelled) {
          setSettings(s);
          setTagsDraft(s.secondbrain_tags.join(", "));
        }
      } catch (err) {
        toast.error(err);
      }
      try {
        const d = await api.listAudioDevices();
        if (!cancelled) setDevices(d);
      } catch (err) {
        toast.error(err);
      }
      await refreshModels();
      await Promise.all([
        refreshSecret("openai_api_key"),
        refreshSecret("anthropic_api_key"),
        refreshSecret("secondbrain_api_key"),
      ]);
    })();
    return () => {
      cancelled = true;
    };
  }, [toast, refreshModels, refreshSecret]);

  useEvent("model:progress", (p) => {
    setProgress((prev) => ({ ...prev, [p.name]: p }));
  });

  function update<K extends keyof Settings>(key: K, value: Settings[K]) {
    setSettings((prev) => (prev ? { ...prev, [key]: value } : prev));
    setDirty(true);
  }

  async function handleSave(e: FormEvent) {
    e.preventDefault();
    if (!settings) return;
    setSaving(true);
    try {
      const saved = await api.saveSettings({
        ...settings,
        chunk_seconds: clampChunk(settings.chunk_seconds),
        language: settings.language.trim() || "auto",
        cloud_base_url: settings.cloud_base_url.trim(),
        cloud_model: settings.cloud_model.trim(),
        claude_model: settings.claude_model.trim(),
        secondbrain_url: settings.secondbrain_url.trim(),
        secondbrain_tags: parseTags(tagsDraft),
        secondbrain_identity: settings.secondbrain_identity.trim(),
      });
      setSettings(saved);
      setTagsDraft(saved.secondbrain_tags.join(", "));
      setDirty(false);
      setSbStatus(null);
      // Changing the cloud or SecondBrain endpoint's origin clears that stored
      // key on the backend; re-check so the form shows it needs re-entering.
      await Promise.all([refreshSecret("openai_api_key"), refreshSecret("secondbrain_api_key")]);
      // Persist any API keys typed into the form.
      for (const name of ["openai_api_key", "anthropic_api_key", "secondbrain_api_key"] as SecretName[]) {
        if (secrets[name].draft.trim()) {
          await saveSecret(name);
        }
      }
      toast.push("Settings saved", "success");
    } catch (err) {
      toast.error(err);
    } finally {
      setSaving(false);
    }
  }

  async function saveSecret(name: SecretName) {
    const value = secrets[name].draft.trim();
    if (!value) return;
    setSecrets((prev) => ({ ...prev, [name]: { ...prev[name], busy: true } }));
    try {
      await api.setSecret(name, value);
      setSecrets((prev) => ({ ...prev, [name]: { saved: true, draft: "", busy: false } }));
    } catch (err) {
      toast.error(err);
      setSecrets((prev) => ({ ...prev, [name]: { ...prev[name], busy: false } }));
    }
  }

  async function removeSecret(name: SecretName) {
    setSecrets((prev) => ({ ...prev, [name]: { ...prev[name], busy: true } }));
    try {
      await api.setSecret(name, "");
      setSecrets((prev) => ({ ...prev, [name]: { saved: false, draft: "", busy: false } }));
    } catch (err) {
      toast.error(err);
      setSecrets((prev) => ({ ...prev, [name]: { ...prev[name], busy: false } }));
    }
  }

  async function handleDownload(name: string) {
    setDownloading((prev) => new Set(prev).add(name));
    setProgress((prev) => ({ ...prev, [name]: { name, downloaded: 0, total: 0 } }));
    try {
      await api.downloadModel(name);
      toast.push(`Downloaded ${name}`, "success");
    } catch (err) {
      toast.error(err);
    } finally {
      setDownloading((prev) => {
        const next = new Set(prev);
        next.delete(name);
        return next;
      });
      setProgress((prev) => {
        const next = { ...prev };
        delete next[name];
        return next;
      });
      await refreshModels();
    }
  }

  async function handleDeleteModel(name: string) {
    try {
      await api.deleteModel(name);
      await refreshModels();
    } catch (err) {
      toast.error(err);
    }
  }

  async function handleTestSecondBrain() {
    setTesting(true);
    setSbStatus(null);
    try {
      setSbStatus(await api.secondbrainStatus());
    } catch (err) {
      toast.error(err);
    } finally {
      setTesting(false);
    }
  }

  async function handleOpenDataDir() {
    try {
      await api.openDataDir();
    } catch (err) {
      toast.error(err);
    }
  }

  if (!settings) {
    return (
      <section className="view settings-view">
        <header className="view-header">
          <h1>Settings</h1>
        </header>
        <div className="empty">Loading...</div>
      </section>
    );
  }

  // Keep a previously chosen device selectable even if it's currently unplugged.
  const inputOptions = [...(devices?.inputs ?? [])];
  if (settings.input_device && !inputOptions.includes(settings.input_device)) {
    inputOptions.push(settings.input_device);
  }
  // "Others" source choices: monitor/loopback-looking inputs first, then the rest.
  const monitorOptions = [...(devices?.monitors ?? [])];
  const otherSourceOptions = (devices?.inputs ?? []).filter((n) => !monitorOptions.includes(n));
  const savedSystemSource = settings.system_audio_device;
  if (savedSystemSource && !monitorOptions.includes(savedSystemSource) && !otherSourceOptions.includes(savedSystemSource)) {
    otherSourceOptions.push(savedSystemSource);
  }

  return (
    <section className="view settings-view">
      <form onSubmit={handleSave}>
        <header className="view-header row-between">
          <div>
            <h1>Settings</h1>
            <p className="muted">Transcription, API keys, audio capture and SecondBrain export.</p>
          </div>
          <div className="row-gap">
            <button type="button" className="btn btn-sm" onClick={handleOpenDataDir}>
              Open data folder
            </button>
            <button type="submit" className="btn btn-primary" disabled={saving || (!dirty && !hasDrafts(secrets))}>
              {saving ? "Saving..." : "Save"}
            </button>
          </div>
        </header>

        {/* Transcription */}
        <fieldset className="card settings-section">
          <legend>Transcription</legend>
          <div className="radio-row">
            <label className={`radio-card${settings.transcription_mode === "local" ? " is-selected" : ""}`}>
              <input
                type="radio"
                name="transcription_mode"
                value="local"
                checked={settings.transcription_mode === "local"}
                onChange={() => update("transcription_mode", "local")}
              />
              <span className="radio-title">Local</span>
              <span className="radio-desc muted small">
                Whisper runs on this machine. Private, works offline.
              </span>
            </label>
            <label className={`radio-card${settings.transcription_mode === "cloud" ? " is-selected" : ""}`}>
              <input
                type="radio"
                name="transcription_mode"
                value="cloud"
                checked={settings.transcription_mode === "cloud"}
                onChange={() => update("transcription_mode", "cloud")}
              />
              <span className="radio-title">Cloud</span>
              <span className="radio-desc muted small">
                OpenAI-compatible transcription API. Faster on slow machines.
              </span>
            </label>
          </div>

          {settings.transcription_mode === "local" && (
            <div className="subsection">
              <h3>Local model</h3>
              {models.length === 0 ? (
                <p className="muted small">No models reported by the backend.</p>
              ) : (
                <ul className="model-list">
                  {models.map((m) => {
                    const isDownloading = downloading.has(m.name);
                    const p = progress[m.name];
                    const pct =
                      p && p.total > 0 ? Math.min(100, Math.round((p.downloaded / p.total) * 100)) : null;
                    const selected = settings.local_model === m.name;
                    return (
                      <li key={m.name} className={`model-row${selected ? " is-selected" : ""}`}>
                        <label className="model-pick">
                          <input
                            type="radio"
                            name="local_model"
                            value={m.name}
                            checked={selected}
                            onChange={() => update("local_model", m.name)}
                            disabled={!m.downloaded && !isDownloading}
                          />
                          <span className="model-name mono">{m.name}</span>
                          <span className="muted small">{formatSizeMb(m.size_mb)}</span>
                        </label>
                        <div className="model-actions">
                          {isDownloading ? (
                            <div className="progress-wrap">
                              <div className="progress" role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={pct ?? undefined}>
                                <div
                                  className={`progress-fill${pct === null ? " is-indeterminate" : ""}`}
                                  style={pct === null ? undefined : { width: `${pct}%` }}
                                />
                              </div>
                              <span className="muted small mono">
                                {p && p.total > 0
                                  ? `${formatBytes(p.downloaded)} / ${formatBytes(p.total)}`
                                  : "Starting..."}
                              </span>
                            </div>
                          ) : m.downloaded ? (
                            <>
                              <span className="badge badge-done">Downloaded</span>
                              <button
                                type="button"
                                className="btn btn-ghost btn-sm text-danger"
                                onClick={() => handleDeleteModel(m.name)}
                                disabled={selected}
                                title={selected ? "Select another model before deleting" : undefined}
                              >
                                Delete
                              </button>
                            </>
                          ) : (
                            <button
                              type="button"
                              className="btn btn-sm"
                              onClick={() => handleDownload(m.name)}
                            >
                              Download
                            </button>
                          )}
                        </div>
                      </li>
                    );
                  })}
                </ul>
              )}
              {!models.some((m) => m.name === settings.local_model && m.downloaded) && (
                <p className="hint">
                  The selected model <span className="mono">{settings.local_model}</span> isn't
                  downloaded yet. Download it before recording in Local mode.
                </p>
              )}
            </div>
          )}

          {settings.transcription_mode === "cloud" && (
            <div className="subsection">
              <h3>Cloud API</h3>
              <div className="field-grid">
                <label className="field">
                  <span className="field-label">Base URL</span>
                  <input
                    className="input"
                    type="url"
                    value={settings.cloud_base_url}
                    onChange={(e) => update("cloud_base_url", e.target.value)}
                    placeholder="https://api.openai.com/v1"
                  />
                </label>
                <label className="field">
                  <span className="field-label">Model</span>
                  <input
                    className="input"
                    type="text"
                    value={settings.cloud_model}
                    onChange={(e) => update("cloud_model", e.target.value)}
                    placeholder="whisper-1"
                  />
                </label>
                <SecretField
                  label="API key"
                  state={secrets.openai_api_key}
                  onChange={(draft) =>
                    setSecrets((prev) => ({
                      ...prev,
                      openai_api_key: { ...prev.openai_api_key, draft },
                    }))
                  }
                  onSave={() => saveSecret("openai_api_key")}
                  onRemove={() => removeSecret("openai_api_key")}
                />
              </div>
            </div>
          )}
        </fieldset>

        {/* Claude */}
        <fieldset className="card settings-section">
          <legend>Claude</legend>
          <p className="muted small">Used for meeting summaries.</p>
          <div className="field-grid">
            <SecretField
              label="Anthropic API key"
              state={secrets.anthropic_api_key}
              onChange={(draft) =>
                setSecrets((prev) => ({
                  ...prev,
                  anthropic_api_key: { ...prev.anthropic_api_key, draft },
                }))
              }
              onSave={() => saveSecret("anthropic_api_key")}
              onRemove={() => removeSecret("anthropic_api_key")}
            />
            <label className="field">
              <span className="field-label">Model</span>
              <input
                className="input"
                type="text"
                value={settings.claude_model}
                onChange={(e) => update("claude_model", e.target.value)}
                placeholder="claude-opus-5-5"
              />
            </label>
          </div>
        </fieldset>

        {/* SecondBrain */}
        <fieldset className="card settings-section">
          <legend>SecondBrain</legend>
          <p className="muted small">
            Send finished meetings to your SecondBrain knowledge base as searchable transcript
            documents.
          </p>
          <div className="field-grid">
            <label className="field field-check">
              <input
                type="checkbox"
                checked={settings.secondbrain_enabled}
                onChange={(e) => update("secondbrain_enabled", e.target.checked)}
              />
              <span>Export meetings to SecondBrain</span>
            </label>
            <label className="field">
              <span className="field-label">API URL</span>
              <input
                className="input"
                type="url"
                value={settings.secondbrain_url}
                onChange={(e) => update("secondbrain_url", e.target.value)}
                placeholder="http://127.0.0.1:7171/v1"
              />
            </label>
            <label className="field field-check">
              <input
                type="checkbox"
                checked={settings.secondbrain_allow_private_http}
                onChange={(e) => update("secondbrain_allow_private_http", e.target.checked)}
              />
              <span>
                Allow plain HTTP on my private network
                <span className="muted small block">
                  Unencrypted: the API key and meetings travel in clear text. Only for addresses like
                  192.168.x.x, 10.x.x.x or *.lan behind your VPN or home network.
                </span>
              </span>
            </label>
            <SecretField
              label="API key"
              state={secrets.secondbrain_api_key}
              onChange={(draft) =>
                setSecrets((prev) => ({
                  ...prev,
                  secondbrain_api_key: { ...prev.secondbrain_api_key, draft },
                }))
              }
              onSave={() => saveSecret("secondbrain_api_key")}
              onRemove={() => removeSecret("secondbrain_api_key")}
            />
            <label className="field field-check">
              <input
                type="checkbox"
                checked={settings.secondbrain_auto_export}
                onChange={(e) => update("secondbrain_auto_export", e.target.checked)}
              />
              <span>
                Export automatically when transcription finishes
                <span className="muted small block">
                  Also re-exports when a summary is generated.
                </span>
              </span>
            </label>
            <label className="field field-check">
              <input
                type="checkbox"
                checked={settings.secondbrain_wait_for_index}
                onChange={(e) => update("secondbrain_wait_for_index", e.target.checked)}
              />
              <span>
                Wait for indexing
                <span className="muted small block">
                  Waits up to 30 seconds. Indexing may continue after the export is accepted.
                </span>
              </span>
            </label>
            <label className="field">
              <span className="field-label">Your identity (email or name)</span>
              <input
                className="input"
                type="text"
                value={settings.secondbrain_identity}
                onChange={(e) => update("secondbrain_identity", e.target.value)}
                placeholder="you@example.com"
                autoComplete="email"
              />
              <span className="muted small">
                Used as the speaker name and participant for your side. Leave empty to label it
                "You" with no participant.
              </span>
            </label>
            <label className="field">
              <span className="field-label">Tags</span>
              <input
                className="input"
                type="text"
                value={tagsDraft}
                onChange={(e) => {
                  setTagsDraft(e.target.value);
                  setDirty(true);
                }}
                placeholder="meeting, transcriptor"
              />
              <span className="muted small">Comma-separated; added to every exported meeting.</span>
            </label>
            <div className="field">
              <div className="row-gap">
                <button
                  type="button"
                  className="btn btn-sm"
                  onClick={handleTestSecondBrain}
                  disabled={testing || dirty || !!secrets.secondbrain_api_key.draft.trim()}
                  title={dirty ? "Save settings first" : undefined}
                >
                  {testing && <span className="spinner spinner-xs" aria-hidden="true" />}
                  {testing ? "Testing..." : "Test connection"}
                </button>
                {sbStatus && sbStatus.reachable === true && sbStatus.key_ok === true && (
                  <span className="badge badge-done">Connected</span>
                )}
                {sbStatus && sbStatus.reachable === true && sbStatus.key_ok === false && (
                  <span className="badge badge-failed">Key rejected</span>
                )}
                {sbStatus && sbStatus.reachable === true && sbStatus.key_ok === null && (
                  <span className="badge badge-processing">Reachable</span>
                )}
                {sbStatus && sbStatus.reachable === false && (
                  <span className="badge badge-failed">Not connected</span>
                )}
              </div>
              {(dirty || !!secrets.secondbrain_api_key.draft.trim()) && (
                <span className="muted small">Save settings to test the connection.</span>
              )}
              {sbStatus && (
                <span
                  className={`small${
                    sbStatus.reachable === false || sbStatus.key_ok === false ? " text-danger" : " muted"
                  }`}
                >
                  {sbStatus.message}
                </span>
              )}
            </div>
          </div>
        </fieldset>

        {/* Audio */}
        <fieldset className="card settings-section">
          <legend>Audio</legend>
          <div className="field-grid">
            <label className="field">
              <span className="field-label">Input device</span>
              <select
                className="input"
                value={settings.input_device ?? SYSTEM_DEFAULT}
                onChange={(e) =>
                  update("input_device", e.target.value === SYSTEM_DEFAULT ? null : e.target.value)
                }
              >
                <option value={SYSTEM_DEFAULT}>
                  System default{devices?.default_input ? ` (${devices.default_input})` : ""}
                </option>
                {inputOptions.map((name) => (
                  <option key={name} value={name}>
                    {name}
                  </option>
                ))}
              </select>
            </label>
            <label className="field field-check">
              <input
                type="checkbox"
                checked={settings.capture_system_audio}
                onChange={(e) => update("capture_system_audio", e.target.checked)}
              />
              <span>
                Capture system audio (other participants)
                {devices?.default_output && (
                  <span className="muted small block">Output: {devices.default_output}</span>
                )}
              </span>
            </label>
            <label className="field">
              <span className="field-label">Other participants (system audio) source</span>
              <select
                className="input"
                value={settings.system_audio_device ?? AUTOMATIC_LOOPBACK}
                disabled={!settings.capture_system_audio}
                onChange={(e) =>
                  update(
                    "system_audio_device",
                    e.target.value === AUTOMATIC_LOOPBACK ? null : e.target.value,
                  )
                }
              >
                <option value={AUTOMATIC_LOOPBACK}>Automatic (system loopback)</option>
                {monitorOptions.length > 0 && (
                  <optgroup label="Monitor / loopback sources">
                    {monitorOptions.map((name) => (
                      <option key={name} value={name}>
                        {name}
                      </option>
                    ))}
                  </optgroup>
                )}
                {otherSourceOptions.length > 0 && (
                  <optgroup label="Other input devices">
                    {otherSourceOptions.map((name) => (
                      <option key={name} value={name}>
                        {name}
                      </option>
                    ))}
                  </optgroup>
                )}
              </select>
              <span className="muted small">
                Automatic records the default output. On Linux, or with a virtual device such as
                BlackHole or Stereo Mix, pick the monitor/loopback source here.
              </span>
            </label>
            <label className="field">
              <span className="field-label">Language</span>
              <input
                className="input"
                type="text"
                value={settings.language}
                onChange={(e) => update("language", e.target.value)}
                placeholder="auto"
              />
              <span className="muted small">"auto" or an ISO code such as "en".</span>
            </label>
            <label className="field">
              <span className="field-label">Chunk seconds</span>
              <input
                className="input input-narrow"
                type="number"
                min={2}
                max={60}
                step={1}
                value={settings.chunk_seconds}
                onChange={(e) => update("chunk_seconds", Number(e.target.value))}
              />
              <span className="muted small">
                Live transcription chunk length. Shorter is more responsive; longer is more accurate.
              </span>
            </label>
          </div>
        </fieldset>
      </form>
    </section>
  );
}

function clampChunk(n: number): number {
  if (!Number.isFinite(n)) return 8;
  return Math.min(60, Math.max(2, Math.round(n)));
}

function parseTags(text: string): string[] {
  return text
    .split(",")
    .map((t) => t.trim())
    .filter((t) => t.length > 0);
}

function hasDrafts(secrets: Record<SecretName, SecretState>): boolean {
  return Object.values(secrets).some((s) => s.draft.trim().length > 0);
}

interface SecretFieldProps {
  label: string;
  state: SecretState;
  onChange: (draft: string) => void;
  onSave: () => void;
  onRemove: () => void;
}

function SecretField({ label, state, onChange, onSave, onRemove }: SecretFieldProps) {
  return (
    <div className="field">
      <span className="field-label">
        {label}{" "}
        {state.saved === true && <span className="badge badge-done">Key saved</span>}
        {state.saved === false && <span className="badge">Not set</span>}
      </span>
      <div className="secret-row">
        <input
          className="input"
          type="password"
          autoComplete="off"
          value={state.draft}
          onChange={(e) => onChange(e.target.value)}
          placeholder={state.saved ? "Enter a new key to replace" : "Paste API key"}
          disabled={state.busy}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              onSave();
            }
          }}
        />
        <button
          type="button"
          className="btn btn-sm"
          onClick={onSave}
          disabled={state.busy || !state.draft.trim()}
        >
          Save key
        </button>
        {state.saved && (
          <button
            type="button"
            className="btn btn-ghost btn-sm text-danger"
            onClick={onRemove}
            disabled={state.busy}
          >
            Remove
          </button>
        )}
      </div>
    </div>
  );
}
