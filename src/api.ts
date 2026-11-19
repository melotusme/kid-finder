import { invoke, convertFileSrc } from "@tauri-apps/api/core";

export type Status = {
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
  rejectFace: (faceId: number, personId: number) => invoke<void>("reject_face", { faceId, personId }),
  exportItems: (outDir: string, items: { mediaId: number; segments: [number, number][] }[]) =>
    invoke<ExportReport>("export", { outDir, items }),
  qxcCreateLogin: () => invoke<LoginTicket>("qxc_create_login"),
  qxcCheckLogin: (token: string) => invoke<string | null>("qxc_check_login", { token }),
  qxcLogout: () => invoke<void>("qxc_logout"),
  qxcAlbums: () => invoke<Album[]>("qxc_albums"),
  qxcSync: (groupId: string) => invoke<SyncReport>("qxc_sync", { groupId }),
};

export const src = (p: string | null | undefined) => (p ? convertFileSrc(p) : "");

export const fmtTime = (s: number) => `${Math.floor(s / 60)}:${String(Math.floor(s % 60)).padStart(2, "0")}`;
