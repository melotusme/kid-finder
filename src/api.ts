import { invoke, convertFileSrc } from "@tauri-apps/api/core";

export type Status = {
  appVersion: string;
  extensionVersion: string;
  modelsReady: boolean;
  ffmpeg: string | null;
  processing: boolean;
  total: number;
  pending: number;
  failed: number;
  images: number;
  videos: number;
  qxcUser: string | null;
  threshold: number;
  dataDir: string;
  bridgeRunning: boolean;
  bridgePort: number;
  extensionDir: string;
  webScanned: number;
  refsSignature: string;
};
export type ExtensionInstallResult = {
  dir: string;
  browser: string | null;
  openedBrowser: boolean;
  openedFolder: boolean;
};
export type Person = { id: number; name: string; refs: { id: number; thumb: string | null }[] };
export type ImportReport = {
  added: number;
  duplicates: { name: string; existingName: string; importedAt: string }[];
  unsupported: number;
  errors: string[];
};
export type RefReport = { added: number; noFace: string[]; multiFace: string[]; errors: string[] };
export type Hit = {
  mediaId: number;
  kind: "image" | "video";
  path: string;
  origName: string;
  poster: string | null;
  time: string;
  score: number;
  faceId: number;
  bbox: [number, number, number, number];
  duration: number | null;
  segments: [number, number][];
};
export type ExportReport = { images: number; clips: number; skipped: number; errors: string[] };
export type Album = { groupId: string; groupName: string; imgCnt: number; videoCnt: number; lastUploadTime?: string };
export type SyncReport = { added: number; duplicates: number; errors: string[]; complete: boolean };
export type LoginTicket = { token: string; qrSvg: string; expiresInMs: number };
export type Progress = { stage: "import" | "analyze" | "export"; done: number; total: number; current: string };
export type ThumbUpdate = { mediaId: number; poster: string };

export const api = {
  status: () => invoke<Status>("get_status"),
  setThreshold: (value: number) => invoke<void>("set_threshold", { value }),
  downloadModels: () => invoke<void>("download_models"),
  installFfmpeg: () => invoke<string>("install_ffmpeg"),
  persons: () => invoke<Person[]>("list_persons"),
  addPerson: (name: string) => invoke<number>("add_person", { name }),
  renamePerson: (id: number, name: string) => invoke<void>("rename_person", { id, name }),
  deletePerson: (id: number) => invoke<void>("delete_person", { id }),
  addRefs: (personId: number, paths: string[]) => invoke<RefReport>("add_refs", { personId, paths }),
  deleteRef: (id: number) => invoke<void>("delete_ref", { id }),
  importPaths: (paths: string[]) => invoke<ImportReport>("import_paths", { paths }),
  startProcessing: () => invoke<void>("start_processing"),
  retryFailed: () => invoke<void>("retry_failed"),
  search: (personId: number, threshold: number) => invoke<Hit[]>("search", { personId, threshold }),
  confirmFace: (faceId: number, personId: number) => invoke<void>("confirm_face", { faceId, personId }),
  rejectFace: (faceId: number, personId: number, threshold?: number) =>
    invoke<Hit | null>("reject_face", { faceId, personId, threshold }),
  exportItems: (outDir: string, items: { mediaId: number; segments: [number, number][] }[], fade: number) =>
    invoke<ExportReport>("export", { outDir, items, fade }),
  qxcCreateLogin: () => invoke<LoginTicket>("qxc_create_login"),
  qxcCheckLogin: (token: string) => invoke<string | null>("qxc_check_login", { token }),
  qxcLogout: () => invoke<void>("qxc_logout"),
  qxcAlbums: () => invoke<Album[]>("qxc_albums"),
  qxcSync: (groupId: string) => invoke<SyncReport>("qxc_sync", { groupId }),
  installChromeExtension: () => invoke<ExtensionInstallResult>("install_chrome_extension"),
  openQxcWeb: () => invoke<void>("open_qxc_web"),
  clearWebCache: () => invoke<void>("clear_web_cache"),
};

export const src = (p: string | null | undefined) => (p ? convertFileSrc(p) : "");

export const fmtTime = (s: number) => `${Math.floor(s / 60)}:${String(Math.floor(s % 60)).padStart(2, "0")}`;

/** "1:23" / "83" / "1:23.5" -> seconds; null if unparsable. */
export const parseTime = (v: string): number | null => {
  const m = v.trim().match(/^(?:(\d+)[:：])?(\d+(?:\.\d+)?)$/);
  return m ? Number(m[1] ?? 0) * 60 + Number(m[2]) : null;
};

export type ClipSettings = { lead: number; tail: number; fade: number };
const CLIP_KEY = "clipSettings";
export const loadClipSettings = (): ClipSettings => {
  try {
    return { lead: 2, tail: 2, fade: 0.5, ...JSON.parse(localStorage.getItem(CLIP_KEY) ?? "{}") };
  } catch {
    return { lead: 2, tail: 2, fade: 0.5 };
  }
};
export const saveClipSettings = (s: ClipSettings) => localStorage.setItem(CLIP_KEY, JSON.stringify(s));

/** Apply lead-in / tail padding, clamp to the video length and merge overlaps. */
export const padSegments = (segs: [number, number][], s: ClipSettings, duration: number | null): [number, number][] => {
  const max = duration ? duration + 1 : Infinity;
  const out: [number, number][] = [];
  for (const [a, b] of [...segs].sort((x, y) => x[0] - y[0])) {
    const p: [number, number] = [Math.max(0, a - s.lead), Math.min(max, b + s.tail)];
    const last = out[out.length - 1];
    if (last && p[0] <= last[1]) last[1] = Math.max(last[1], p[1]);
    else out.push(p);
  }
  return out;
};
