import { invoke, convertFileSrc } from "@tauri-apps/api/core";

export type Status = {
  appVersion: string;
  extensionVersion: string;
  modelsReady: boolean;
  ocrModelsReady: boolean;
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
  storageRoot: string;
  wechatWatchDir: string | null;
  wechatWatchAutoclean: boolean;
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

export type Person = {
  id: number;
  name: string;
  aliases: string;
  keywords: string[];
  refs: { id: number; thumb: string | null }[];
};

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
  faceId: number | null;
  bbox: [number, number, number, number];
  ocrId: number | null;
  matchSource: "face" | "ocr" | "both";
  ocrKeyword: string | null;
  ocrText: string | null;
  ocrBbox: [number, number, number, number] | null;
  duration: number | null;
  segments: [number, number][];
};

export type ExportReport = { images: number; clips: number; skipped: number; errors: string[] };
export type Album = { groupId: string; groupName: string; imgCnt: number; videoCnt: number; lastUploadTime?: string };
export type SyncReport = { added: number; duplicates: number; errors: string[]; complete: boolean };
export type LoginTicket = { token: string; qrSvg: string; expiresInMs: number };
export type Progress = { stage: "import" | "analyze" | "export"; done: number; total: number; current: string };
export type ThumbUpdate = { mediaId: number; poster: string };

export type WechatSource = {
  accountId: string;
  generation: string;
  rootDir: string;
  videoDir: string | null;
  fileDir: string | null;
  legacyImageDir: string | null;
  attachDir: string | null;
  videoCount: number;
  fileMediaCount: number;
  legacyDatCount: number;
  v2DatCount: number;
  v2FullDatCount: number;
};

export type StorageStats = {
  storageRoot: string;
  defaultRoot: string;
  isCustom: boolean;
  libraryBytes: number;
  thumbsBytes: number;
  freeBytes: number | null;
  purgedCount: number;
  externalCount: number;
};

export type MigrateReport = {
  oldRoot: string;
  newRoot: string;
  copiedFiles: number;
  copiedBytes: number;
  updatedMediaRows: number;
  updatedRefRows: number;
  removedOldFiles: number;
};

export type PurgeReport = {
  purgedCount: number;
  freedBytes: number;
  skippedExternal: number;
  skippedKept: number;
  errors: string[];
};

export const api = {
  status: () => invoke<Status>("get_status"),
  setThreshold: (value: number) => invoke<void>("set_threshold", { value }),
  downloadModels: () => invoke<void>("download_models"),
  installFfmpeg: () => invoke<string>("install_ffmpeg"),
  defaultAliases: (name: string) => invoke<string>("default_aliases", { name }),
  persons: () => invoke<Person[]>("list_persons"),
  addPerson: (name: string, aliases?: string) => invoke<number>("add_person", { name, aliases }),
  renamePerson: (id: number, name: string) => invoke<void>("rename_person", { id, name }),
  updatePersonProfile: (id: number, name: string, aliases: string) =>
    invoke<void>("update_person_profile", { id, name, aliases }),
  deletePerson: (id: number) => invoke<void>("delete_person", { id }),
  addRefs: (personId: number, paths: string[]) => invoke<RefReport>("add_refs", { personId, paths }),
  deleteRef: (id: number) => invoke<void>("delete_ref", { id }),
  importPaths: (paths: string[]) => invoke<ImportReport>("import_paths", { paths }),
  discoverWechat: () => invoke<WechatSource[]>("discover_wechat"),
  importWechatVideos: () => invoke<ImportReport>("import_wechat_videos"),
  setWechatWatch: (dir?: string | null, autoclean?: boolean) =>
    invoke<void>("set_wechat_watch", { dir: dir === undefined ? undefined : (dir ?? ""), autoclean }),
  startProcessing: () => invoke<void>("start_processing"),
  retryFailed: () => invoke<void>("retry_failed"),
  search: (personId: number, threshold: number) => invoke<Hit[]>("search", { personId, threshold }),
  confirmFace: (faceId: number, personId: number) => invoke<void>("confirm_face", { faceId, personId }),
  rejectFace: (faceId: number, personId: number, threshold?: number) =>
    invoke<Hit | null>("reject_face", { faceId, personId, threshold }),
  rejectHit: (personId: number, faceId: number | null, ocrId: number | null, threshold?: number) =>
    invoke<Hit | null>("reject_hit", { personId, faceId, ocrId, threshold }),
  exportItems: (outDir: string, items: { mediaId: number; segments: [number, number][] }[], fade: number) =>
    invoke<ExportReport>("export", { outDir, items, fade }),
  prepareVideoPreview: (mediaId: number) => invoke<string>("prepare_video_preview", { mediaId }),
  storageStats: () => invoke<StorageStats>("get_storage_stats"),
  migrateStorage: (newRoot: string) => invoke<MigrateReport>("migrate_storage", { newRoot }),
  purgeUnmatched: () => invoke<PurgeReport>("purge_unmatched"),
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

export const videoStreamUrl = (
  bridgePort: number,
  mediaId: number,
  preview = false,
  cacheBust?: number,
): string => {
  const params = new URLSearchParams();
  if (preview) params.set("preview", "1");
  if (cacheBust !== undefined) params.set("t", String(cacheBust));
  const qs = params.toString();
  return `http://127.0.0.1:${bridgePort}/api/video/${mediaId}${qs ? `?${qs}` : ""}`;
};

export const fmtTime = (s: number) => `${Math.floor(s / 60)}:${String(Math.floor(s % 60)).padStart(2, "0")}`;

export const fmtBytes = (bytes: number | null | undefined): string => {
  if (bytes === null || bytes === undefined) return "未知";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GB`;
};

const GENERIC_TERMS = [
  "宝宝", "小朋友", "同学", "宝贝", "孩子", "儿子", "女儿",
  "大宝", "二宝", "小宝", "大班", "中班", "小班", "托班", "学前班", "幼儿园", "未命名", "测试",
];

/** Pure client-side mirror of `ocr::default_aliases_for_name` for instant prefill while typing. */
export const deriveDefaultAliases = (name: string): string => {
  const trimmed = name.trim();
  const chars = Array.from(trimmed);
  if (chars.length !== 3 && chars.length !== 4) return "";
  const isCjk = chars.every((ch) => {
    const cp = ch.codePointAt(0) ?? 0;
    return (
      (cp >= 0x4e00 && cp <= 0x9fff) ||
      (cp >= 0x3400 && cp <= 0x4dbf) ||
      (cp >= 0x20000 && cp <= 0x323af) ||
      (cp >= 0xf900 && cp <= 0xfaff)
    );
  });
  if (!isCjk) return "";
  if (trimmed.endsWith("班") || GENERIC_TERMS.some((g) => trimmed.includes(g))) return "";
  return chars.slice(-2).join("").replace(/袆/g, "祎");
};

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
