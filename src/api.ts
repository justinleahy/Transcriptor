import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  AppEventName,
  AudioDevices,
  EventPayloads,
  Meeting,
  MeetingDetail,
  MeetingListItem,
  ModelInfo,
  RecordingStatus,
  SecretName,
  Settings,
} from "./types";

// Thin typed wrappers over every command in docs/IPC.md.
// Argument keys are snake_case exactly as the contract specifies.
export const api = {
  // Settings
  getSettings: () => invoke<Settings>("get_settings"),
  saveSettings: (settings: Settings) =>
    invoke<Settings>("save_settings", { settings }),

  // Secrets (empty string deletes)
  setSecret: (name: SecretName, value: string) =>
    invoke<void>("set_secret", { name, value }),
  hasSecret: (name: SecretName) => invoke<boolean>("has_secret", { name }),

  // Audio
  listAudioDevices: () => invoke<AudioDevices>("list_audio_devices"),

  // Recording
  startRecording: (title: string | null) =>
    invoke<Meeting>("start_recording", { title }),
  stopRecording: () => invoke<Meeting>("stop_recording"),
  recordingStatus: () => invoke<RecordingStatus>("recording_status"),

  // Meetings
  listMeetings: (query: string | null) =>
    invoke<MeetingListItem[]>("list_meetings", { query }),
  getMeeting: (id: string) => invoke<MeetingDetail>("get_meeting", { id }),
  renameMeeting: (id: string, title: string) =>
    invoke<Meeting>("rename_meeting", { id, title }),
  deleteMeeting: (id: string) => invoke<void>("delete_meeting", { id }),
  summarizeMeeting: (id: string) =>
    invoke<Meeting>("summarize_meeting", { id }),

  // Local models
  listLocalModels: () => invoke<ModelInfo[]>("list_local_models"),
  downloadModel: (name: string) =>
    invoke<ModelInfo>("download_model", { name }),
  deleteModel: (name: string) => invoke<void>("delete_model", { name }),

  // Misc
  openDataDir: () => invoke<void>("open_data_dir"),
};

/** Typed `listen` for the app's events. Returns the unlisten function. */
export function onEvent<N extends AppEventName>(
  name: N,
  handler: (payload: EventPayloads[N]) => void,
): Promise<UnlistenFn> {
  return listen<EventPayloads[N]>(name, (event) => handler(event.payload));
}

/** Normalize anything thrown by invoke into a display string. */
export function errorMessage(err: unknown): string {
  if (typeof err === "string") return err;
  if (err instanceof Error) return err.message;
  try {
    return JSON.stringify(err);
  } catch {
    return String(err);
  }
}
