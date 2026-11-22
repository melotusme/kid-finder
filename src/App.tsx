import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";
import { openPath, revealItemInDir } from "@tauri-apps/plugin-opener";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import Guide, { WechatImportGuide } from "./Guide";
import {
  api,
  src,
  videoStreamUrl,
  fmtTime,
  fmtBytes,
  deriveDefaultAliases,
  parseTime,
  padSegments,
  loadClipSettings,
  saveClipSettings,
  type ClipSettings,
  type Status,
  type Person,
  type Hit,
  type Progress,
  type ThumbUpdate,
  type ImportReport,
  type Album,
  type LoginTicket,
  type ExtensionInstallResult,
  type WechatSource,
  type StorageStats,
} from "./api";

type Tab = "kids" | "import" | "photos" | "videos" | "settings" | "guide";
type PhotoFilter = "all" | "face" | "ocr";
const MEDIA_EXT = ["jpg", "jpeg", "png", "webp", "bmp", "gif", "mp4", "mov", "m4v", "avi", "mkv", "3gp", "webm", "dat"];

export default function App() {
  const [tab, setTab] = useState<Tab>("kids");
  const [status, setStatus] = useState<Status | null>(null);
  const [persons, setPersons] = useState<Person[]>([]);
  const [selectedPersonId, setSelectedPersonId] = useState<number | null>(null);
  const [draftThreshold, setDraftThreshold] = useState<number>(0.4);
  const [committedThreshold, setCommittedThreshold] = useState<number>(0.4);
  const thresholdSynced = useRef(false);
  const thresholdTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const [progress, setProgress] = useState<Progress | null>(null);
  const [dl, setDl] = useState<{ label: string; done: number; total: number | null } | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  const [update, setUpdate] = useState<Update | null>(null);
  const [extGuide, setExtGuide] = useState<ExtensionInstallResult | null>(null);

  const refresh = useCallback(async () => {
    const [s, p] = await Promise.all([api.status(), api.persons()]);
    setStatus(s);
    setPersons(p);
    if (!thresholdSynced.current) {
      setDraftThreshold(s.threshold);
      setCommittedThreshold(s.threshold);
      thresholdSynced.current = true;
    }
  }, []);
  const notify = useCallback((m: string) => {
    setToast(m);
    setTimeout(() => setToast((t) => (t === m ? null : t)), 6000);
  }, []);

  const handleThresholdInput = useCallback((v: number) => {
    setDraftThreshold(v);
    if (thresholdTimer.current) clearTimeout(thresholdTimer.current);
    thresholdTimer.current = setTimeout(() => {
      thresholdTimer.current = null;
      setCommittedThreshold(v);
    }, 160);
  }, []);

  const handleThresholdCommit = useCallback((v: number) => {
    if (thresholdTimer.current) {
      clearTimeout(thresholdTimer.current);
      thresholdTimer.current = null;
    }
    setDraftThreshold(v);
    setCommittedThreshold(v);
    api.setThreshold(v).catch(() => {});
  }, []);

  useEffect(() => {
    return () => {
      if (thresholdTimer.current) clearTimeout(thresholdTimer.current);
    };
  }, []);

  useEffect(() => {
    if (selectedPersonId === null || !persons.some((p) => p.id === selectedPersonId)) {
      const next = persons[0]?.id ?? null;
      if (selectedPersonId !== next) setSelectedPersonId(next);
    }
  }, [persons, selectedPersonId]);

  const activePid =
    selectedPersonId !== null && persons.some((p) => p.id === selectedPersonId)
      ? selectedPersonId
      : (persons[0]?.id ?? null);

  const triggerInstallExtension = useCallback(async () => {
    try {
      const res = await api.installChromeExtension();
      navigator.clipboard?.writeText(res.dir).catch(() => {});
      setExtGuide(res);
      refresh();
    } catch (e) {
      notify("准备 Chrome 扩展失败：" + String(e));
    }
  }, [refresh, notify]);

  useEffect(() => {
    refresh();
    const uns = [
      listen<Progress>("progress", (e) => setProgress(e.payload.done >= e.payload.total ? null : e.payload)),
      listen<{ label: string; done: number; total: number | null }>("download-progress", (e) =>
        setDl(e.payload.total !== null && e.payload.done >= e.payload.total ? null : e.payload),
      ),
      listen<string | null>("process-done", (e) => {
        setProgress(null);
        if (e.payload) notify("分析出错：" + e.payload);
        refresh();
      }),
      listen<void>("persons-changed", () => refresh()),
      listen<void>("ocr-updated", () => refresh()),
      listen<void>("task-state", () => refresh()),
      listen<number>("watch-imported", (e) => {
        notify(`已从微信中转夹自动入库 ${e.payload} 个新文件`);
        refresh();
      }),
    ];
    check().then((u) => u && setUpdate(u)).catch(() => {});
    return () => uns.forEach((u) => u.then((f) => f()));
  }, [refresh, notify]);

  useEffect(() => {
    if (status && !status.modelsReady) setTab("settings");
  }, [status?.modelsReady]);

  return (
    <div className="app">
      <nav>
        <div className="brand">找娃助手</div>
        {(
          [
            ["kids", "① 录入宝宝"],
            ["import", "② 导入照片视频"],
            ["photos", "③ 宝宝照片"],
            ["videos", "④ 宝宝视频"],
            ["settings", "设置"],
            ["guide", "使用说明"],
          ] as [Tab, string][]
        ).map(([k, label]) => (
          <button key={k} className={tab === k ? "active" : ""} onClick={() => setTab(k)}>
            {label}
          </button>
        ))}
        <div className="spacer" />
        <button className="ext-nav-btn" onClick={triggerInstallExtension} title="安装 Chrome 扩展，在群相册网页直接人脸+作品署名识别勾选">
          安装 Chrome 扩展
        </button>
        {status && (
          <div className="stat">
            库内 {status.images} 张照片 · {status.videos} 个视频
            <div>
              已扫记录 {status.analyzed}
              {status.purged > 0 ? ` · 指纹 ${status.purged}` : ""}
            </div>
            {status.pending > 0 && <div>待识别 {status.pending}</div>}
            {status.failed > 0 && (
              <div>
                失败 {status.failed} <a onClick={() => api.retryFailed().then(refresh)}>重试</a>
              </div>
            )}
          </div>
        )}
      </nav>
      <main>
        {update && (
          <div className="banner">
            发现新版本 {update.version}
            <button
              onClick={async () => {
                notify("正在下载更新…");
                await update.downloadAndInstall();
                await relaunch();
              }}
            >
              立即更新
            </button>
            <button className="ghost" onClick={() => setUpdate(null)}>
              稍后
            </button>
          </div>
        )}
        {progress && progress.stage !== "analyze" && (
          <div className="progress">
            <span>{{ import: "导入/扫描中", analyze: "识别分析中", export: "处理中" }[progress.stage]}</span>
            <progress value={progress.done} max={Math.max(progress.total, 1)} />
            <span className="progress-meta">
              {progress.done}/{progress.total} {progress.current}
            </span>
          </div>
        )}
        {status && (status.analyzeState !== "idle" || status.pending > 0 || (progress && progress.stage === "analyze")) && (
          <div className="progress">
            <span className={`task-pill ${status.analyzeState}`}>
              {
                {
                  running: "识别中",
                  paused: "已暂停",
                  stopped: "已停止",
                  idle: "待识别",
                }[status.analyzeState]
              }
            </span>
            {progress && progress.stage === "analyze" && (
              <>
                <progress value={progress.done} max={Math.max(progress.total, 1)} />
                <span className="progress-meta">
                  {progress.done}/{progress.total} {progress.current}
                </span>
              </>
            )}
            {(!progress || progress.stage !== "analyze") && (
              <span className="progress-meta">
                已扫记录 {status.analyzed} · 待识别 {status.pending}
                {status.failed > 0 ? ` · 失败 ${status.failed}` : ""}
              </span>
            )}
            <div className="spacer" />
            {status.analyzeState === "running" && (
              <>
                <button className="ghost small" onClick={() => api.pauseProcessing().then(refresh)}>
                  暂停
                </button>
                <button className="danger small" onClick={() => api.stopProcessing().then(refresh)}>
                  停止
                </button>
              </>
            )}
            {status.analyzeState === "paused" && (
              <>
                <button className="small" onClick={() => api.resumeProcessing().then(refresh)}>
                  继续
                </button>
                <button className="danger small" onClick={() => api.stopProcessing().then(refresh)}>
                  停止
                </button>
                <button
                  className="ghost small"
                  onClick={() =>
                    confirm("确定停止当前队列并从头重新识别库内所有未清理照片/视频？（已录入的宝宝参考照不受影响）") &&
                    api.restartProcessing("all").then(refresh).catch((e) => notify(String(e)))
                  }
                >
                  重新开始
                </button>
              </>
            )}
            {(status.analyzeState === "stopped" || (status.analyzeState === "idle" && status.pending > 0)) && (
              <>
                <button className="small" onClick={() => api.resumeProcessing().then(refresh)}>
                  {status.analyzeState === "stopped" ? "继续剩余" : "开始识别"}
                </button>
                <button
                  className="ghost small"
                  onClick={() =>
                    confirm("从头重新识别库内所有未清理照片/视频？（正常只需点「继续剩余」，已扫过的不会重复扫）") &&
                    api.restartProcessing("all").then(refresh).catch((e) => notify(String(e)))
                  }
                >
                  重新开始
                </button>
              </>
            )}
          </div>
        )}
        {dl && (
          <div className="progress">
            <span>下载 {dl.label}</span>
            <progress value={dl.done} max={dl.total ?? undefined} />
            <span>
              {(dl.done / 1048576).toFixed(1)}
              {dl.total ? ` / ${(dl.total / 1048576).toFixed(1)}` : ""} MB
            </span>
          </div>
        )}
        {tab === "kids" && <Kids persons={persons} refresh={refresh} notify={notify} />}
        {tab === "import" && status && (
          <Import status={status} persons={persons} onInstallExt={triggerInstallExtension} refresh={refresh} notify={notify} />
        )}
        {tab === "photos" && status && (
          <PhotosView
            persons={persons}
            status={status}
            pid={activePid}
            onSelectPid={setSelectedPersonId}
            draftThreshold={draftThreshold}
            committedThreshold={committedThreshold}
            onInputThreshold={handleThresholdInput}
            onCommitThreshold={handleThresholdCommit}
            onGoKids={() => setTab("kids")}
            onGoSettings={() => setTab("settings")}
            refresh={refresh}
            notify={notify}
          />
        )}
        {tab === "videos" && status && (
          <VideosView
            persons={persons}
            status={status}
            pid={activePid}
            onSelectPid={setSelectedPersonId}
            draftThreshold={draftThreshold}
            committedThreshold={committedThreshold}
            onInputThreshold={handleThresholdInput}
            onCommitThreshold={handleThresholdCommit}
            onGoKids={() => setTab("kids")}
            refresh={refresh}
            notify={notify}
          />
        )}
        {tab === "settings" && status && (
          <Settings status={status} persons={persons} onInstallExt={triggerInstallExtension} refresh={refresh} notify={notify} />
        )}
        {tab === "guide" && <Guide onNavigate={(t) => setTab(t as Tab)} />}
      </main>
      {extGuide && (
        <ExtensionInstallModal
          result={extGuide}
          extensionVersion={status?.extensionVersion}
          persons={persons}
          onClose={() => setExtGuide(null)}
          onCopy={() => {
            navigator.clipboard?.writeText(extGuide.dir).then(() => notify("已复制扩展目录路径"));
          }}
          onOpenFolder={() => openPath(extGuide.dir).catch(() => {})}
          onOpenQxc={() => api.openQxcWeb().catch((e) => notify(String(e)))}
        />
      )}
      {toast && (
        <div className="toast" onClick={() => setToast(null)}>
          {toast}
        </div>
      )}
    </div>
  );
}

type Common = { refresh: () => Promise<void>; notify: (m: string) => void };

function PersonAliasesEditor({
  person,
  refresh,
  notify,
}: Common & { person: Person }) {
  const [aliases, setAliases] = useState(person.aliases);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    setAliases(person.aliases);
  }, [person.aliases]);

  const dirty = aliases.trim() !== person.aliases.trim();

  const save = async () => {
    if (!dirty || saving) return;
    setSaving(true);
    try {
      await api.updatePersonProfile(person.id, person.name, aliases);
      notify(`已更新「${person.name}」的作品署名匹配词`);
      await refresh();
    } catch (e) {
      notify(String(e));
    }
    setSaving(false);
  };

  return (
    <div className="ocr-profile-box">
      <div className="row small">
        <span className="muted">作品署名 / 小名：</span>
        <input
          style={{ flex: 1, minWidth: 240 }}
          placeholder="逗号分隔，用于识别画作、手工、奖状上的名字（如：梓淏, 小淏）"
          value={aliases}
          onChange={(e) => setAliases(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") save();
          }}
        />
        {dirty && (
          <button className="small" disabled={saving} onClick={save}>
            {saving ? "保存中…" : "保存署名"}
          </button>
        )}
      </div>
      <div className="row small" style={{ marginTop: 6 }}>
        <span className="muted">🏷️ 署名匹配词：</span>
        {person.keywords.length > 0 ? (
          person.keywords.map((kw) => (
            <span className="kw-chip" key={kw}>
              {kw}
            </span>
          ))
        ) : (
          <span className="muted">暂无（填写至少 2 个字的姓名或小名后自动识别无露脸作品照；单字姓氏会自动忽略防误判）</span>
        )}
      </div>
    </div>
  );
}

function Kids({ persons, refresh, notify }: Common & { persons: Person[] }) {
  const [name, setName] = useState("");
  const [aliases, setAliases] = useState("");
  const [aliasesTouched, setAliasesTouched] = useState(false);
  const [busy, setBusy] = useState<number | null>(null);

  const handleNameChange = (val: string) => {
    setName(val);
    if (!aliasesTouched) {
      setAliases(deriveDefaultAliases(val));
    }
  };

  const handleAddPerson = async () => {
    if (!name.trim()) return;
    try {
      await api.addPerson(name.trim(), aliases.trim());
      setName("");
      setAliases("");
      setAliasesTouched(false);
      await refresh();
    } catch (e) {
      notify(String(e));
    }
  };

  const addRefs = async (p: Person) => {
    const files = await open({ multiple: true, filters: [{ name: "照片", extensions: ["jpg", "jpeg", "png", "webp", "bmp"] }] });
    if (!files || files.length === 0) return;
    setBusy(p.id);
    try {
      const r = await api.addRefs(p.id, files as string[]);
      const msg = [`已录入 ${r.added} 张`];
      if (r.duplicates.length) msg.push(`跳过 ${r.duplicates.length} 张重复照`);
      if (r.noFace.length) msg.push(`${r.noFace.length} 张没检测到人脸`);
      if (r.multiFace.length) msg.push(`${r.multiFace.length} 张有多张脸（已取最大的那张，建议用单人照）`);
      if (r.errors.length) msg.push(`失败：${r.errors.join("；")}`);
      notify(msg.join("，"));
    } catch (e) {
      notify(String(e));
    }
    setBusy(null);
    refresh();
  };

  return (
    <section>
      <h2>录入宝宝</h2>
      <p className="hint">
        为每个孩子添加 3~10 张清晰的正脸照（不同角度、时期更稳），并填写孩子的<b>真实姓名 / 小名</b>——遇到老师拍的<b>画作、手工、奖状、姓名贴</b>等未露脸照片时，也能通过作品署名自动帮你找出来！所有识别均在本机离线完成。
      </p>
      <div className="card">
        <div className="row">
          <input
            placeholder="宝宝姓名（如：韩梓淏）"
            value={name}
            onChange={(e) => handleNameChange(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && handleAddPerson()}
          />
          <input
            style={{ flex: 1, minWidth: 260 }}
            placeholder="作品署名 / 小名（逗号分隔，用于识别画作、手工、奖状上的名字）"
            value={aliases}
            onChange={(e) => {
              setAliasesTouched(true);
              setAliases(e.target.value);
            }}
            onKeyDown={(e) => e.key === "Enter" && handleAddPerson()}
          />
          <button disabled={!name.trim()} onClick={handleAddPerson}>
            添加宝宝
          </button>
        </div>
        <div className="muted small" style={{ marginTop: 6 }}>
          提示：输入三字或四字中文姓名时会自动预填双字名（如「韩梓淏」→「梓淏」），方便识别老师只写名不写姓的作品贴纸；如有同班重名可随时删改。
        </div>
      </div>
      {persons.map((p) => (
        <div className="card" key={p.id}>
          <div className="row">
            <h3>{p.name}</h3>
            <span className="muted">{p.refs.length} 张参考照</span>
            <div className="spacer" />
            <button disabled={busy === p.id} onClick={() => addRefs(p)}>
              {busy === p.id ? "识别中…" : "添加参考照"}
            </button>
            {p.refs.length > 1 && (
              <button
                className="ghost small"
                disabled={busy === p.id}
                title="自动清理同一张或几乎完全相同的重复参考照（保留最早一张）"
                onClick={async () => {
                  try {
                    const removed = await api.dedupPersonRefs(p.id);
                    notify(removed > 0 ? `已清理「${p.name}」的 ${removed} 张重复参考照` : `「${p.name}」的参考照没有重复`);
                    await refresh();
                  } catch (e) {
                    notify(String(e));
                  }
                }}
              >
                清理重复照
              </button>
            )}
            <button
              className="ghost"
              onClick={() => {
                const n = prompt("新名字", p.name);
                if (n?.trim()) api.renamePerson(p.id, n).then(refresh).catch((e) => notify(String(e)));
              }}
            >
              改名
            </button>
            <button
              className="danger"
              onClick={() => confirm(`删除「${p.name}」及其参考照？`) && api.deletePerson(p.id).then(refresh)}
            >
              删除
            </button>
          </div>
          <PersonAliasesEditor person={p} refresh={refresh} notify={notify} />
          <div className="thumbs">
            {p.refs.map((r) => (
              <div className="ref" key={r.id}>
                {r.thumb ? <img src={src(r.thumb)} /> : <div className="noimg">视频</div>}
                <button className="x" title="删除" onClick={() => api.deleteRef(r.id).then(refresh)}>
                  ×
                </button>
              </div>
            ))}
          </div>
        </div>
      ))}
    </section>
  );
}

function ImportResult({ r }: { r: ImportReport }) {
  return (
    <div className="card">
      <div>
        {r.stopped && <span className="task-pill stopped" style={{ marginRight: 6 }}>已停止</span>}
        新导入 <b>{r.added}</b> 个
        {r.duplicates.length > 0 && (
          <>
            ，<b className="warn">{r.duplicates.length}</b> 个之前已记录/缓存过（已自动跳过）
          </>
        )}
        {r.unsupported > 0 && <>，{r.unsupported} 个不是支持的照片/视频</>}
      </div>
      {r.duplicates.length > 0 && (
        <details open={r.duplicates.length <= 10}>
          <summary>已存在的文件</summary>
          <ul className="small">
            {r.duplicates.map((d, i) => (
              <li key={i}>
                {d.name} — 已于 {d.importedAt} 导入{d.existingName !== d.name && <>（当时的文件名：{d.existingName}）</>}
              </li>
            ))}
          </ul>
        </details>
      )}
      {r.errors.length > 0 && (
        <ul className="small err">
          {r.errors.map((e, i) => (
            <li key={i}>{e}</li>
          ))}
        </ul>
      )}
    </div>
  );
}

function WechatAutoCard({
  status,
  onReport,
  refresh,
  notify,
}: Common & { status: Status; onReport: (r: ImportReport) => void }) {
  const [sources, setSources] = useState<WechatSource[]>([]);
  const [scanningWx, setScanningWx] = useState(false);

  useEffect(() => {
    api.discoverWechat().then(setSources).catch(() => {});
  }, []);

  const totalWxVideos = sources.reduce((s, x) => s + x.videoCount, 0);
  const totalWxFiles = sources.reduce((s, x) => s + x.fileMediaCount, 0);

  const pickWatchDir = async () => {
    const dir = await open({ directory: true, title: "选择「微信导出中转」文件夹（将每 4 秒自动入库新保存的照片/视频）" });
    if (!dir) return;
    try {
      await api.setWechatWatch(dir as string, status.wechatWatchAutoclean);
      notify("已开启微信导出中转夹自动监听");
      await refresh();
    } catch (e) {
      notify(String(e));
    }
  };

  return (
    <div className="card">
      <div className="row">
        <h3>微信班级群自动导入（中转夹监听 + 本机已看视频零拷贝）</h3>
        <span className="ok small">100% 零封号只读模式</span>
      </div>
      <div className="wx-auto-grid">
        <div className="wx-subcard">
          <div className="row">
            <b>📂 微信导出中转文件夹（每 4 秒自动扫描）</b>
            <div className="spacer" />
            <button className="small" onClick={pickWatchDir}>
              {status.wechatWatchDir ? "更换中转夹" : "设置中转文件夹"}
            </button>
            {status.wechatWatchDir && (
              <>
                <button className="ghost small" onClick={() => openPath(status.wechatWatchDir!).catch(() => {})}>
                  打开
                </button>
                <button
                  className="ghost small"
                  onClick={() =>
                    api
                      .setWechatWatch("", status.wechatWatchAutoclean)
                      .then(() => {
                        notify("已停止监听中转文件夹");
                        refresh();
                      })
                      .catch((e) => notify(String(e)))
                  }
                >
                  清除
                </button>
              </>
            )}
          </div>
          <div className="muted small" style={{ marginTop: 4 }}>
            {status.wechatWatchDir ? (
              <>
                正在监听：<code>{status.wechatWatchDir}</code>
              </>
            ) : (
              "设置后，你在电脑微信聊天记录里点底部「保存」存入该文件夹的照片与视频会自动后台入库分析。"
            )}
          </div>
          <label className="row small" style={{ marginTop: 6, cursor: "pointer" }}>
            <input
              type="checkbox"
              checked={status.wechatWatchAutoclean}
              onChange={(e) =>
                api
                  .setWechatWatch(undefined, e.target.checked)
                  .then(refresh)
                  .catch((err) => notify(String(err)))
              }
            />
            <span>入库后自动清理中转夹原文件（省空间，已记录 SHA-256 指纹防重复导入）</span>
          </label>
        </div>

        <div className="wx-subcard">
          <div className="row">
            <b>🎬 本机微信已缓存视频（零拷贝原位识别）</b>
            {status.wechatScanState !== "idle" && (
              <span className={`task-pill ${status.wechatScanState}`}>
                {{ running: "扫描中", paused: "已暂停", stopped: "已停止", idle: "" }[status.wechatScanState]}
              </span>
            )}
            <div className="spacer" />
            {scanningWx || status.wechatScanState === "running" || status.wechatScanState === "paused" ? (
              <>
                {status.wechatScanState === "paused" ? (
                  <button className="small" onClick={() => api.resumeWechatScan().then(refresh)}>
                    继续
                  </button>
                ) : (
                  <button className="ghost small" onClick={() => api.pauseWechatScan().then(refresh)}>
                    暂停
                  </button>
                )}
                <button className="danger small" onClick={() => api.stopWechatScan().then(refresh)}>
                  停止
                </button>
              </>
            ) : (
              <>
                <button
                  className="ghost small"
                  onClick={async () => {
                    setScanningWx(true);
                    try {
                      const rep = await api.importWechatVideos(false);
                      onReport(rep);
                      notify(
                        (rep.stopped ? "已停止微信缓存扫描：" : "微信缓存扫描完成：") +
                          `新增索引 ${rep.added} 个` +
                          (rep.duplicates.length ? `，跳过已记录缓存 ${rep.duplicates.length} 个` : ""),
                      );
                      await refresh();
                      api.discoverWechat().then(setSources).catch(() => {});
                    } catch (e) {
                      notify(String(e));
                    }
                    setScanningWx(false);
                  }}
                >
                  {status.wechatScanState === "stopped" ? "继续扫描新增" : "扫描微信已缓存视频"}
                </button>
                <button
                  className="ghost small"
                  title="跳过路径快缓存，从头按 SHA-256 核对全部本机微信视频（已入库的仍不会重复保存）"
                  onClick={async () => {
                    setScanningWx(true);
                    try {
                      const rep = await api.importWechatVideos(true);
                      onReport(rep);
                      notify(
                        (rep.stopped ? "已停止重新核对：" : "重新核对完成：") +
                          `新增索引 ${rep.added} 个` +
                          (rep.duplicates.length ? `，已有记录 ${rep.duplicates.length} 个` : ""),
                      );
                      await refresh();
                      api.discoverWechat().then(setSources).catch(() => {});
                    } catch (e) {
                      notify(String(e));
                    }
                    setScanningWx(false);
                  }}
                >
                  重新核对
                </button>
              </>
            )}
          </div>
          <div className="muted small" style={{ marginTop: 4 }}>
            {sources.length > 0 ? (
              <>
                已发现本机微信目录 {sources.length} 个（{sources.map((s) => `微信 ${s.generation}`).join("、")}），检测到明文视频{" "}
                <b>{totalWxVideos}</b> 个{totalWxFiles > 0 ? `、文件附件 ${totalWxFiles} 个` : ""}；本机数据库已永久记录微信媒体{" "}
                <b>{status.wechatCached}</b> 个（已扫过的自动秒级跳过，不重复入库）。
              </>
            ) : (
              "未在默认位置检测到已下载的电脑微信明文视频（在微信设置开启「自动下载在其他设备查看过的内容」后，手机看过的群视频会自动同步到电脑）。"
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

function Import({
  status,
  persons,
  onInstallExt,
  refresh,
  notify,
}: Common & { status: Status; persons: Person[]; onInstallExt: () => void }) {
  const [report, setReport] = useState<ImportReport | null>(null);
  const [hover, setHover] = useState(false);
  const busy = useRef(false);

  const doImport = useCallback(
    async (paths: string[]) => {
      if (!paths.length || busy.current) return;
      busy.current = true;
      try {
        setReport(await api.importPaths(paths));
      } catch (e) {
        notify(String(e));
      }
      busy.current = false;
      refresh();
    },
    [refresh, notify],
  );

  useEffect(() => {
    const un = getCurrentWebview().onDragDropEvent((e) => {
      const t = e.payload.type;
      if (t === "enter" || t === "over") setHover(true);
      else if (t === "leave") setHover(false);
      else if (t === "drop") {
        setHover(false);
        doImport(e.payload.paths);
      }
    });
    return () => {
      un.then((f) => f());
    };
  }, [doImport]);

  return (
    <section>
      <h2>导入照片和视频</h2>
      <p className="hint">
        把微信里保存下来的照片/视频（或整个文件夹）拖进来，照片（含宝宝正脸与署名作品）与视频会自动分类归档到「③ 宝宝照片」与「④ 宝宝视频」。同一个文件导入过会自动跳过，不会重复导入。
        {!status.ffmpeg && <b className="warn"> 处理视频需要 ffmpeg，请到「④ 宝宝视频」或「设置」一键安装。</b>}
      </p>
      <div className={"drop" + (hover ? " hover" : "")}>
        <div>拖放本地照片/视频文件或文件夹到这里</div>
        <div className="row center">
          <button
            onClick={async () => {
              const f = await open({ multiple: true, filters: [{ name: "照片/视频", extensions: MEDIA_EXT }] });
              if (f) doImport(f as string[]);
            }}
          >
            选择文件
          </button>
          <button
            onClick={async () => {
              const d = await open({ directory: true, multiple: true });
              if (d) doImport(d as string[]);
            }}
          >
            选择文件夹
          </button>
        </div>
      </div>
      {report && <ImportResult r={report} />}
      <WechatImportGuide />
      <WechatAutoCard status={status} onReport={setReport} refresh={refresh} notify={notify} />
      <ChromeExtensionCard status={status} persons={persons} onInstallExt={onInstallExt} refresh={refresh} notify={notify} />
      <Qxc status={status} refresh={refresh} notify={notify} />
    </section>
  );
}

function Qxc({ status, refresh, notify }: Common & { status: Status }) {
  const [ticket, setTicket] = useState<LoginTicket | null>(null);
  const [albums, setAlbums] = useState<Album[] | null>(null);
  const [syncing, setSyncing] = useState<string | null>(null);
  const [syncInfo, setSyncInfo] = useState<string>("");

  useEffect(() => {
    if (!ticket) return;
    const started = Date.now();
    const timer = setInterval(async () => {
      if (Date.now() - started > ticket.expiresInMs) {
        setTicket(await api.qxcCreateLogin().catch(() => null));
        return;
      }
      try {
        const name = await api.qxcCheckLogin(ticket.token);
        if (name !== null) {
          setTicket(null);
          refresh();
        }
      } catch (e) {
        notify(String(e));
      }
    }, 2000);
    return () => clearInterval(timer);
  }, [ticket, refresh, notify]);

  useEffect(() => {
    if (status.qxcUser !== null) api.qxcAlbums().then(setAlbums).catch((e) => notify(String(e)));
    else setAlbums(null);
  }, [status.qxcUser, notify]);

  useEffect(() => {
    const un = listen<{ groupId?: string; state?: string; page: number; added: number; duplicates: number }>(
      "sync-progress",
      (e) =>
        setSyncInfo(
          `${e.payload.state === "paused" ? "【已暂停】" : ""}第 ${e.payload.page + 1} 页 · 新增 ${e.payload.added} · 已记录跳过 ${e.payload.duplicates}`,
        ),
    );
    return () => {
      un.then((f) => f());
    };
  }, []);

  return (
    <div className="card">
      <div className="row">
        <h3>从「群相册」小程序同步</h3>
        <div className="spacer" />
        {status.qxcUser !== null ? (
          <>
            <span className="muted">已登录：{status.qxcUser || "微信用户"}</span>
            <button className="ghost" onClick={() => api.qxcLogout().then(refresh)}>
              退出
            </button>
          </>
        ) : (
          !ticket && (
            <button onClick={() => api.qxcCreateLogin().then(setTicket).catch((e) => notify(String(e)))}>扫码登录</button>
          )
        )}
      </div>
      <p className="hint">如果老师用的是「群相册」小程序，可以直接同步，已同步过的照片会自动跳过。登录 24 小时后需要重新扫码。</p>
      {ticket && (
        <div className="qr">
          <div dangerouslySetInnerHTML={{ __html: ticket.qrSvg }} />
          <div>请用微信扫码，并在手机上确认登录</div>
        </div>
      )}
      {albums?.map((a) => {
        const activeThis = syncing === a.groupId || status.qxcActiveGroup === a.groupId;
        const isRunningThis = activeThis && status.qxcState === "running";
        const isPausedThis = activeThis && status.qxcState === "paused";
        const isStoppedThis = activeThis && status.qxcState === "stopped";
        const busyAny = syncing !== null || status.qxcState === "running" || status.qxcState === "paused";
        const runSync = async (restart: boolean) => {
          setSyncing(a.groupId);
          setSyncInfo("");
          try {
            const r = await api.qxcSync(a.groupId, restart);
            notify(
              (r.stopped ? "已停止群相册同步：" : restart ? "重新核对同步完成：" : "同步完成：") +
                `新增 ${r.added} 个，${r.duplicates} 个之前已记录（已跳过）` +
                (r.errors.length ? `，${r.errors.length} 个失败` : ""),
            );
          } catch (e) {
            notify(String(e));
          }
          setSyncing(null);
          refresh();
        };
        return (
          <div className="row album" key={a.groupId}>
            <span>{a.groupName}</span>
            <span className="muted">
              {a.imgCnt} 张 · {a.videoCnt} 个视频
            </span>
            {isRunningThis && <span className="task-pill running">同步中</span>}
            {isPausedThis && <span className="task-pill paused">已暂停</span>}
            {isStoppedThis && <span className="task-pill stopped">已停止</span>}
            <div className="spacer" />
            {activeThis && syncInfo && <span className="muted small">{syncInfo}</span>}
            {isRunningThis || isPausedThis ? (
              <>
                {isPausedThis ? (
                  <button className="small" onClick={() => api.resumeQxcSync().then(refresh)}>
                    继续
                  </button>
                ) : (
                  <button className="ghost small" onClick={() => api.pauseQxcSync().then(refresh)}>
                    暂停
                  </button>
                )}
                <button className="danger small" onClick={() => api.stopQxcSync().then(refresh)}>
                  停止
                </button>
              </>
            ) : (
              <>
                <button className="small" disabled={busyAny} onClick={() => runSync(false)}>
                  {isStoppedThis ? "继续同步" : "增量同步"}
                </button>
                <button
                  className="ghost small"
                  disabled={busyAny}
                  title="从第 1 页重新核对整本相册（之前同步过的仍按记录跳过，不会重复下载）"
                  onClick={() => runSync(true)}
                >
                  重新开始
                </button>
              </>
            )}
          </div>
        );
      })}
    </div>
  );
}

type MediaViewProps = Common & {
  persons: Person[];
  status: Status;
  pid: number | null;
  onSelectPid: (id: number) => void;
  draftThreshold: number;
  committedThreshold: number;
  onInputThreshold: (v: number) => void;
  onCommitThreshold: (v: number) => void;
  onGoKids: () => void;
  onGoSettings?: () => void;
};

/** Deterministic `(time DESC, mediaId DESC)` comparator matching `media::search` in Rust. */
function compareHits(a: Hit, b: Hit): number {
  if (a.time !== b.time) return b.time < a.time ? -1 : 1;
  return b.mediaId - a.mediaId;
}

function insertHitSorted(list: Hit[], item: Hit): Hit[] {
  const without = list.filter((x) => x.mediaId !== item.mediaId);
  const idx = without.findIndex((x) => compareHits(item, x) < 0);
  if (idx === -1) return [...without, item];
  return [...without.slice(0, idx), item, ...without.slice(idx)];
}

function usePersonHits({
  kind,
  persons,
  pid,
  committedThreshold,
  pending,
  notify,
}: {
  kind: "image" | "video";
  persons: Person[];
  pid: number | null;
  committedThreshold: number;
  pending: number;
  notify: (m: string) => void;
}) {
  const [hits, setHits] = useState<Hit[]>([]);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [loading, setLoading] = useState(false);
  const reqSeq = useRef(0);
  const hitsRef = useRef<Hit[]>(hits);
  hitsRef.current = hits;
  const selectedRef = useRef<Set<number>>(selected);
  selectedRef.current = selected;
  const unselectedByPid = useRef<Map<number, Set<number>>>(new Map());

  const getUnselected = useCallback((personId: number) => {
    let set = unselectedByPid.current.get(personId);
    if (!set) {
      set = new Set<number>();
      unselectedByPid.current.set(personId, set);
    }
    return set;
  }, []);

  const currentPerson = persons.find((p) => p.id === pid) ?? null;
  const refsKey = currentPerson?.refs.map((r) => r.id).join(",") ?? "";
  const keywordsKey = currentPerson?.keywords.join(",") ?? "";
  const hasRefs = (currentPerson?.refs.length ?? 0) > 0;
  const hasKeywords = (currentPerson?.keywords.length ?? 0) > 0;
  const canSearch = hasRefs || (kind === "image" && hasKeywords);

  const load = useCallback(async () => {
    const seq = ++reqSeq.current;
    if (pid === null || !currentPerson || !canSearch) {
      setHits([]);
      setSelected(new Set());
      setLoading(false);
      return;
    }
    setLoading(true);
    try {
      const all = await api.search(pid, committedThreshold);
      if (seq !== reqSeq.current) return;
      const filtered = all.filter((h) => h.kind === kind);
      const unsel = getUnselected(pid);
      setHits(filtered);
      setSelected(new Set(filtered.filter((x) => !unsel.has(x.mediaId)).map((x) => x.mediaId)));
    } finally {
      if (seq === reqSeq.current) setLoading(false);
    }
  }, [pid, currentPerson?.id, canSearch, refsKey, keywordsKey, committedThreshold, kind, getUnselected]);

  useEffect(() => {
    load().catch((e) => notify(String(e)));
  }, [load, pending, notify]);

  // Patch backfilled photo thumbnails in-place AND refresh on background OCR backfill completion.
  useEffect(() => {
    if (kind !== "image") return;
    const unThumbs = listen<ThumbUpdate[]>("thumbs-updated", (e) => {
      const updates = e.payload;
      if (!updates || updates.length === 0) return;
      const posterById = new Map<number, string>();
      for (const u of updates) posterById.set(u.mediaId, u.poster);
      setHits((prev) => {
        let changed = false;
        const next = prev.map((h) => {
          const p = posterById.get(h.mediaId);
          if (p && h.poster !== p) {
            changed = true;
            return { ...h, poster: p };
          }
          return h;
        });
        return changed ? next : prev;
      });
    });
    const unOcr = listen<void>("ocr-updated", () => {
      load().catch(() => {});
    });
    return () => {
      unThumbs.then((f) => f());
      unOcr.then((f) => f());
    };
  }, [kind, load]);

  const toggle = useCallback(
    (id: number) => {
      setSelected((s) => {
        const n = new Set(s);
        const unsel = pid !== null ? getUnselected(pid) : null;
        if (n.has(id)) {
          n.delete(id);
          unsel?.add(id);
        } else {
          n.add(id);
          unsel?.delete(id);
        }
        return n;
      });
    },
    [pid, getUnselected],
  );

  const toggleSelectVisible = useCallback(
    (visibleHits?: Hit[]) => {
      const target = visibleHits ?? hitsRef.current;
      const unsel = pid !== null ? getUnselected(pid) : null;
      const anySelected = target.some((h) => selectedRef.current.has(h.mediaId));
      setSelected((prev) => {
        const next = new Set(prev);
        if (anySelected) {
          for (const h of target) {
            next.delete(h.mediaId);
            unsel?.add(h.mediaId);
          }
        } else {
          for (const h of target) {
            next.add(h.mediaId);
            unsel?.delete(h.mediaId);
          }
        }
        return next;
      });
    },
    [pid, getUnselected],
  );

  // Optimistic surgical face + OCR rejection: removes/updates ONLY `h.mediaId` in-place.
  const rejectHit = useCallback(
    (h: Hit) => {
      if (pid === null) return;
      const wasSelected = selectedRef.current.has(h.mediaId);
      if (wasSelected) {
        setSelected((prev) => {
          if (!prev.has(h.mediaId)) return prev;
          const n = new Set(prev);
          n.delete(h.mediaId);
          return n;
        });
      }
      setHits((prev) => prev.filter((x) => x.mediaId !== h.mediaId));

      api
        .rejectHit(pid, h.faceId, h.ocrId, committedThreshold)
        .then((replacement) => {
          if (replacement && replacement.kind === kind) {
            setHits((prev) => insertHitSorted(prev, replacement));
            if (wasSelected) {
              setSelected((prev) => {
                const n = new Set(prev);
                n.add(replacement.mediaId);
                return n;
              });
            }
          }
        })
        .catch((e) => {
          setHits((prev) => insertHitSorted(prev, h));
          if (wasSelected) {
            setSelected((prev) => {
              const n = new Set(prev);
              n.add(h.mediaId);
              return n;
            });
          }
          notify(String(e));
        });
    },
    [pid, committedThreshold, kind, notify],
  );

  return {
    hits,
    selected,
    loading,
    currentPerson,
    hasRefs,
    hasKeywords,
    canSearch,
    load,
    toggle,
    toggleSelectAll: toggleSelectVisible,
    rejectHit,
  };
}

const MemoPhotoTile = memo(function PhotoTile({
  hit,
  isSelected,
  onPreview,
  onToggle,
  onConfirm,
  onReject,
}: {
  hit: Hit;
  isSelected: boolean;
  onPreview: (h: Hit) => void;
  onToggle: (mediaId: number) => void;
  onConfirm: (h: Hit) => void;
  onReject: (h: Hit) => void;
}) {
  const hasFace = hit.faceId !== null;
  const hasOcr = hit.matchSource === "ocr" || hit.matchSource === "both";
  return (
    <div className={"tile" + (isSelected ? " sel" : "")}>
      <div className="imgwrap" onClick={() => onPreview(hit)}>
        <img src={src(hit.poster ?? hit.path)} loading="lazy" decoding="async" />
        {hasFace && (
          <div
            className="box"
            style={{
              left: `${hit.bbox[0] * 100}%`,
              top: `${hit.bbox[1] * 100}%`,
              width: `${(hit.bbox[2] - hit.bbox[0]) * 100}%`,
              height: `${(hit.bbox[3] - hit.bbox[1]) * 100}%`,
            }}
          />
        )}
        {hit.ocrBbox && (
          <div
            className="box ocr-box"
            style={{
              left: `${hit.ocrBbox[0] * 100}%`,
              top: `${hit.ocrBbox[1] * 100}%`,
              width: `${(hit.ocrBbox[2] - hit.ocrBbox[0]) * 100}%`,
              height: `${(hit.ocrBbox[3] - hit.ocrBbox[1]) * 100}%`,
            }}
          />
        )}
        {hasOcr && (
          <div className="ocr-badge" title={hit.ocrText ? `识别文字：${hit.ocrText}` : undefined}>
            🏷️ 署名: {hit.ocrKeyword ?? "命中"}
          </div>
        )}
      </div>
      <div className="row tilebar">
        <input type="checkbox" checked={isSelected} onChange={() => onToggle(hit.mediaId)} />
        <span className="muted small">
          {hit.time.slice(0, 10)} · {hasFace ? `${(hit.score * 100).toFixed(0)}%` : "作品署名"}
        </span>
        <div className="spacer" />
        {hit.faceId !== null && (
          <button className="icon" title="是我家宝宝（加入人脸参考并重算）" onClick={() => onConfirm(hit)}>
            ✓
          </button>
        )}
        <button className="icon" title="不是我家宝宝 / 署名认错了" onClick={() => onReject(hit)}>
          ✕
        </button>
      </div>
    </div>
  );
});

const MemoVideoTile = memo(function VideoTile({
  hit,
  clip,
  customSegs,
  isSelected,
  onPreview,
  onToggle,
  onConfirm,
  onReject,
}: {
  hit: Hit;
  clip: ClipSettings;
  customSegs: [number, number][] | undefined;
  isSelected: boolean;
  onPreview: (hit: Hit, initialRange: [number, number] | null) => void;
  onToggle: (mediaId: number) => void;
  onConfirm: (h: Hit) => void;
  onReject: (h: Hit) => void;
}) {
  const segs = useMemo(
    () => customSegs ?? padSegments(hit.segments, clip, hit.duration),
    [customSegs, hit.segments, clip, hit.duration],
  );
  const clipSum = segs.reduce((s, [a, b]) => s + b - a, 0);
  const isEdited = customSegs !== undefined;
  return (
    <div className={"tile video-card" + (isSelected ? " sel" : "")}>
      <div className="vidwrap" onClick={() => onPreview(hit, null)}>
        {hit.poster ? <img src={src(hit.poster)} loading="lazy" decoding="async" /> : <div className="noposter">▶ 视频预览</div>}
        <div className="badge">
          ▶ {segs.length} 段 · 共 {fmtTime(clipSum)}
          {isEdited && " · 已手动调整"}
        </div>
        {hit.duration !== null && <div className="dur-badge">总长 {fmtTime(hit.duration)}</div>}
      </div>
      {segs.length > 0 && (
        <div className="seg-chips">
          {segs.map(([a, b], idx) => (
            <button
              key={idx}
              className="seg-chip"
              title="点击试看并微调该片段"
              onClick={() => onPreview(hit, [a, b])}
            >
              {fmtTime(a)}–{fmtTime(b)}
            </button>
          ))}
        </div>
      )}
      <div className="row tilebar">
        <input type="checkbox" checked={isSelected} onChange={() => onToggle(hit.mediaId)} />
        <span className="muted small">
          {hit.time.slice(0, 10)} · {(hit.score * 100).toFixed(0)}%
        </span>
        <div className="spacer" />
        {hit.faceId !== null && (
          <button className="icon" title="是我家宝宝（加入参考并重算）" onClick={() => onConfirm(hit)}>
            ✓
          </button>
        )}
        <button className="icon" title="不是我家宝宝" onClick={() => onReject(hit)}>
          ✕
        </button>
      </div>
    </div>
  );
});

function PhotosView({
  persons,
  status,
  pid,
  onSelectPid,
  draftThreshold,
  committedThreshold,
  onInputThreshold,
  onCommitThreshold,
  onGoKids,
  onGoSettings,
  refresh,
  notify,
}: MediaViewProps) {
  const {
    hits,
    selected,
    loading,
    currentPerson,
    hasRefs,
    hasKeywords,
    canSearch,
    load,
    toggle,
    toggleSelectAll,
    rejectHit,
  } = usePersonHits({
    kind: "image",
    persons,
    pid,
    committedThreshold,
    pending: status.pending,
    notify,
  });
  const [filter, setFilter] = useState<PhotoFilter>("all");
  const [preview, setPreview] = useState<Hit | null>(null);

  useEffect(() => {
    setPreview((prev) => (prev ? hits.find((x) => x.mediaId === prev.mediaId) ?? null : null));
  }, [hits]);

  const faceCount = useMemo(
    () => hits.filter((h) => h.matchSource === "face" || h.matchSource === "both").length,
    [hits],
  );
  const ocrCount = useMemo(
    () => hits.filter((h) => h.matchSource === "ocr" || h.matchSource === "both").length,
    [hits],
  );

  const visibleHits = useMemo(() => {
    if (filter === "face") return hits.filter((h) => h.matchSource === "face" || h.matchSource === "both");
    if (filter === "ocr") return hits.filter((h) => h.matchSource === "ocr" || h.matchSource === "both");
    return hits;
  }, [hits, filter]);

  const visibleSelectedCount = useMemo(
    () => visibleHits.filter((h) => selected.has(h.mediaId)).length,
    [visibleHits, selected],
  );

  const handleConfirm = useCallback(
    (h: Hit) => {
      if (pid === null || h.faceId === null) return;
      api
        .confirmFace(h.faceId, pid)
        .then(() => {
          notify("已加入参考，已重新计算相似度");
          return refresh();
        })
        .catch((e) => notify(String(e)));
    },
    [pid, refresh, notify],
  );

  if (!persons.length) {
    return (
      <section>
        <div className="card empty-card">
          <p className="hint">请先在「① 录入宝宝」里添加孩子（正脸参考照 + 姓名/小名）。</p>
          <button onClick={onGoKids}>去「① 录入宝宝」添加</button>
        </div>
      </section>
    );
  }

  const doExport = async () => {
    const dir = await open({ directory: true, title: "导出照片到哪个文件夹？" });
    if (!dir) return;
    const items = visibleHits
      .filter((h) => selected.has(h.mediaId))
      .map((h) => ({ mediaId: h.mediaId, segments: [] as [number, number][] }));
    try {
      const r = await api.exportItems(dir as string, items, 0);
      notify(
        `已导出 ${r.images} 张照片` +
          (r.skipped ? `，${r.skipped} 张已存在跳过` : "") +
          (r.errors.length ? `，失败：${r.errors.join("；")}` : ""),
      );
      openPath(dir as string).catch(() => {});
    } catch (e) {
      notify(String(e));
    }
  };

  return (
    <section>
      <div className="row toolbar">
        <select value={pid ?? ""} onChange={(e) => onSelectPid(Number(e.target.value))}>
          {persons.map((p) => (
            <option key={p.id} value={p.id}>
              {p.name}
            </option>
          ))}
        </select>
        <div className="filter-pills">
          <button
            type="button"
            className={"pill" + (filter === "all" ? " active" : "")}
            onClick={() => setFilter("all")}
          >
            全部 ({hits.length})
          </button>
          <button
            type="button"
            className={"pill" + (filter === "face" ? " active" : "")}
            onClick={() => setFilter("face")}
          >
            😊 人脸照 ({faceCount})
          </button>
          <button
            type="button"
            className={"pill ocr-pill" + (filter === "ocr" ? " active" : "")}
            onClick={() => setFilter("ocr")}
          >
            🏷️ 作品署名 ({ocrCount})
          </button>
        </div>
        <label className="muted" title="只调整人脸匹配严格程度，不影响已命中的作品署名照">
          严格程度 {draftThreshold.toFixed(2)}
          <input
            type="range"
            min={0.25}
            max={0.65}
            step={0.01}
            value={draftThreshold}
            onChange={(e) => onInputThreshold(Number(e.target.value))}
            onPointerUp={(e) => onCommitThreshold(Number((e.target as HTMLInputElement).value))}
            onKeyUp={(e) => onCommitThreshold(Number((e.target as HTMLInputElement).value))}
          />
        </label>
        <button
          className="ghost"
          disabled={loading}
          onClick={() =>
            refresh()
              .then(() => load())
              .then(() => notify(canSearch ? "已重新计算匹配结果" : "请先在「① 录入宝宝」添加参考照或姓名"))
              .catch((e) => notify(String(e)))
          }
        >
          {loading ? "计算中…" : "重新计算"}
        </button>
        <div className="spacer" />
        <span className="muted">
          当前显示 {visibleHits.length} 张 · 已选 {visibleSelectedCount}
        </span>
        <button className="ghost" disabled={!visibleHits.length} onClick={() => toggleSelectAll(visibleHits)}>
          {visibleSelectedCount ? "全不选" : "全选"}
        </button>
        <button disabled={!visibleSelectedCount} onClick={doExport}>
          导出选中照片
        </button>
      </div>

      {hasKeywords && !status.ocrModelsReady && (
        <div className="ffmpeg-banner">
          <span>
            <b>尚未下载作品识字模型（约 20MB）：</b>下载后即可自动识别画作、手工、奖状上的「{currentPerson?.keywords.join(" / ")}」名字贴！
          </span>
          <div className="spacer" />
          {onGoSettings && <button onClick={onGoSettings}>前往「设置」一键下载</button>}
        </div>
      )}

      {!canSearch ? (
        <div className="card empty-card">
          <p className="hint">「{currentPerson?.name ?? "该宝宝"}」还没有参考照或有效署名关键词，请先到「① 录入宝宝」补充正脸照或姓名。</p>
          <button onClick={onGoKids}>去「① 录入宝宝」补充</button>
        </div>
      ) : (
        <>
          {!hasRefs && (
            <div className="banner">
              <span>
                当前尚未给「{currentPerson?.name}」添加正脸参考照，正在按署名关键词（<b>{currentPerson?.keywords.join(" · ")}</b>）查找作品照；补充正脸照可同时找出日常人像照。
              </span>
              <div className="spacer" />
              <button className="ghost small" onClick={onGoKids}>
                补充正脸照
              </button>
            </div>
          )}
          <p className="hint">
            认错了点 ✕ 排除，人脸照认对了点 ✓ 加入参考照；拖动「严格程度」滑块只调节人脸相似度，不会漏掉已命中的 🏷️ 作品署名照。
          </p>
          {visibleHits.length === 0 ? (
            <div className="card empty-card">
              <p className="hint" style={{ margin: 0 }}>
                {status.images === 0
                  ? "库内还没有导入照片，请先到「② 导入照片视频」导入。"
                  : filter === "ocr"
                    ? "暂未找到包含宝宝姓名/小名的作品署名照（可在「① 录入宝宝」补充老师常用的署名或小名）。"
                    : "当前筛选条件下没有找到匹配照片，可适当调低严格程度或补充更多角度的参考照。"}
              </p>
            </div>
          ) : (
            <div className="grid">
              {visibleHits.map((h) => (
                <MemoPhotoTile
                  key={h.mediaId}
                  hit={h}
                  isSelected={selected.has(h.mediaId)}
                  onPreview={setPreview}
                  onToggle={toggle}
                  onConfirm={handleConfirm}
                  onReject={rejectHit}
                />
              ))}
            </div>
          )}
        </>
      )}
      {preview && (
        <div className="modal" onClick={() => setPreview(null)}>
          <div onClick={(e) => e.stopPropagation()}>
            <img src={src(preview.path)} />
            <div className="row">
              <span>
                {preview.origName}
                {preview.faceId !== null && (
                  <span className="muted small"> · 人脸相似度 {(preview.score * 100).toFixed(0)}%</span>
                )}
                {preview.ocrText && (
                  <span className="ocr-preview-tag">🏷️ 识别署名：「{preview.ocrText}」</span>
                )}
              </span>
              <div className="spacer" />
              <button className="ghost" onClick={() => revealItemInDir(preview.path)}>
                在文件夹中显示
              </button>
              <button onClick={() => setPreview(null)}>关闭</button>
            </div>
          </div>
        </div>
      )}
    </section>
  );
}

function VideosView({
  persons,
  status,
  pid,
  onSelectPid,
  draftThreshold,
  committedThreshold,
  onInputThreshold,
  onCommitThreshold,
  onGoKids,
  refresh,
  notify,
}: MediaViewProps) {
  const { hits, selected, loading, currentPerson, hasRefs, load, toggle, toggleSelectAll, rejectHit } = usePersonHits({
    kind: "video",
    persons,
    pid,
    committedThreshold,
    pending: status.pending,
    notify,
  });
  const [clip, setClip] = useState<ClipSettings>(loadClipSettings);
  const [edits, setEdits] = useState<Record<string, [number, number][]>>({});
  const [preview, setPreview] = useState<{ hit: Hit; initialRange: [number, number] | null } | null>(null);
  const [installingFfmpeg, setInstallingFfmpeg] = useState(false);

  const setClipS = (c: ClipSettings) => {
    setClip(c);
    saveClipSettings(c);
  };
  const editKey = (mediaId: number) => `${pid ?? "none"}:${mediaId}`;
  const segsOf = (h: Hit) => edits[editKey(h.mediaId)] ?? padSegments(h.segments, clip, h.duration);

  const handlePreviewVideo = useCallback((hit: Hit, initialRange: [number, number] | null) => {
    setPreview({ hit, initialRange });
  }, []);

  const handleConfirm = useCallback(
    (h: Hit) => {
      if (pid === null || h.faceId === null) return;
      api
        .confirmFace(h.faceId, pid)
        .then(() => {
          notify("已加入参考，已重新计算相似度");
          return refresh();
        })
        .catch((e) => notify(String(e)));
    },
    [pid, refresh, notify],
  );

  useEffect(() => {
    setPreview((prev) => {
      if (!prev) return null;
      const updated = hits.find((x) => x.mediaId === prev.hit.mediaId);
      return updated ? { hit: updated, initialRange: prev.initialRange } : null;
    });
  }, [hits]);

  if (!persons.length) {
    return (
      <section>
        <div className="card empty-card">
          <p className="hint">请先在「① 录入宝宝」里添加孩子和清晰的正脸参考照。</p>
          <button onClick={onGoKids}>去「① 录入宝宝」添加</button>
        </div>
      </section>
    );
  }

  const totalClips = hits.reduce((sum, h) => sum + segsOf(h).length, 0);

  const doExport = async () => {
    const dir = await open({ directory: true, title: "裁剪导出视频片段到哪个文件夹？" });
    if (!dir) return;
    const items = hits.filter((h) => selected.has(h.mediaId)).map((h) => ({ mediaId: h.mediaId, segments: segsOf(h) }));
    try {
      const r = await api.exportItems(dir as string, items, clip.fade);
      notify(
        `已导出 ${r.clips} 段视频片段` +
          (r.images ? `、${r.images} 个完整视频` : "") +
          (r.skipped ? `，${r.skipped} 个已存在跳过` : "") +
          (r.errors.length ? `，失败：${r.errors.join("；")}` : ""),
      );
      openPath(dir as string).catch(() => {});
    } catch (e) {
      notify(String(e));
    }
  };

  return (
    <section>
      {!status.ffmpeg && (
        <div className="ffmpeg-banner">
          <span>
            <b>尚未安装 ffmpeg：</b>分析视频抽帧与裁剪导出宝宝片段需要 ffmpeg。
          </span>
          <div className="spacer" />
          <button
            disabled={installingFfmpeg}
            onClick={async () => {
              setInstallingFfmpeg(true);
              try {
                await api.installFfmpeg();
                notify("ffmpeg 安装完成");
                await refresh();
              } catch (e) {
                notify(String(e));
              }
              setInstallingFfmpeg(false);
            }}
          >
            {installingFfmpeg ? "安装中…" : "一键安装 ffmpeg"}
          </button>
        </div>
      )}
      <div className="row card cliprow">
        <b>视频片段裁剪</b>
        <label>
          宝宝出现前提前
          <input type="number" min={0} max={30} step={0.5} value={clip.lead} onChange={(e) => setClipS({ ...clip, lead: Math.max(0, Number(e.target.value)) })} />
          秒开始
        </label>
        <label>
          离开后多留
          <input type="number" min={0} max={30} step={0.5} value={clip.tail} onChange={(e) => setClipS({ ...clip, tail: Math.max(0, Number(e.target.value)) })} />
          秒
        </label>
        <label>
          淡入淡出
          <input type="number" min={0} max={3} step={0.1} value={clip.fade} onChange={(e) => setClipS({ ...clip, fade: Math.max(0, Number(e.target.value)) })} />
          秒（0 为直接切）
        </label>
      </div>
      <div className="row toolbar">
        <select value={pid ?? ""} onChange={(e) => onSelectPid(Number(e.target.value))}>
          {persons.map((p) => (
            <option key={p.id} value={p.id}>
              {p.name}
            </option>
          ))}
        </select>
        <label className="muted" title="越高越严格：漏掉的多，认错的少">
          严格程度 {draftThreshold.toFixed(2)}
          <input
            type="range"
            min={0.25}
            max={0.65}
            step={0.01}
            value={draftThreshold}
            onChange={(e) => onInputThreshold(Number(e.target.value))}
            onPointerUp={(e) => onCommitThreshold(Number((e.target as HTMLInputElement).value))}
            onKeyUp={(e) => onCommitThreshold(Number((e.target as HTMLInputElement).value))}
          />
        </label>
        <button
          className="ghost"
          disabled={loading}
          onClick={() =>
            refresh()
              .then(() => load())
              .then(() => notify(hasRefs ? "已重新计算相似度" : "请先在「① 录入宝宝」添加参考照"))
              .catch((e) => notify(String(e)))
          }
        >
          {loading ? "计算中…" : "重新计算"}
        </button>
        <div className="spacer" />
        <span className="muted">
          找到 {hits.length} 个视频（共 {totalClips} 段片段）· 已选 {selected.size}
        </span>
        <button className="ghost" disabled={!hits.length} onClick={() => toggleSelectAll()}>
          {selected.size ? "全不选" : "全选"}
        </button>
        <button disabled={!selected.size} onClick={doExport}>
          裁剪导出选中片段
        </button>
      </div>
      {!hasRefs ? (
        <div className="card empty-card">
          <p className="hint">「{currentPerson?.name ?? "该宝宝"}」还没有参考照，请先到「① 录入宝宝」添加至少一张清晰的正脸照片。</p>
          <button onClick={onGoKids}>去「① 录入宝宝」添加参考照</button>
        </div>
      ) : (
        <>
          <p className="hint">视频只导出宝宝出现的片段，点击时间胶囊可直接试看对应片段，点开视频可逐段微调起止时间。</p>
          {hits.length === 0 ? (
            <div className="card empty-card">
              <p className="hint" style={{ margin: 0 }}>
                {status.videos === 0
                  ? "库内还没有导入视频，请先到「② 导入照片视频」导入。"
                  : "当前严格程度下没有找到匹配视频片段，可适当调低严格程度或补充更多角度的参考照。"}
              </p>
            </div>
          ) : (
            <div className="video-grid">
              {hits.map((h) => (
                <MemoVideoTile
                  key={h.mediaId}
                  hit={h}
                  clip={clip}
                  customSegs={edits[editKey(h.mediaId)]}
                  isSelected={selected.has(h.mediaId)}
                  onPreview={handlePreviewVideo}
                  onToggle={toggle}
                  onConfirm={handleConfirm}
                  onReject={rejectHit}
                />
              ))}
            </div>
          )}
        </>
      )}
      {preview && (
        <div className="modal" onClick={() => setPreview(null)}>
          <div onClick={(e) => e.stopPropagation()}>
            <VideoPreview
              hit={preview.hit}
              bridgePort={status.bridgePort}
              bridgeRunning={status.bridgeRunning}
              segs={segsOf(preview.hit)}
              initialRange={preview.initialRange}
              edited={editKey(preview.hit.mediaId) in edits}
              onChange={(s) =>
                setEdits((e) => {
                  const n = { ...e };
                  const k = editKey(preview.hit.mediaId);
                  if (s === null) delete n[k];
                  else n[k] = s;
                  return n;
                })
              }
            />
            <div className="row">
              <span>{preview.hit.origName}</span>
              <div className="spacer" />
              <button className="ghost" onClick={() => revealItemInDir(preview.hit.path)}>
                在文件夹中显示
              </button>
              <button onClick={() => setPreview(null)}>关闭</button>
            </div>
          </div>
        </div>
      )}
    </section>
  );
}

function VideoPreview({
  hit,
  bridgePort,
  bridgeRunning,
  segs,
  initialRange,
  edited,
  onChange,
}: {
  hit: Hit;
  bridgePort: number;
  bridgeRunning: boolean;
  segs: [number, number][];
  initialRange?: [number, number] | null;
  edited: boolean;
  onChange: (s: [number, number][] | null) => void;
}) {
  const ref = useRef<HTMLVideoElement>(null);
  const stopAt = useRef<number | null>(null);
  const activeMediaIdRef = useRef<number>(hit.mediaId);
  activeMediaIdRef.current = hit.mediaId;
  const fallbackTriedRef = useRef<number | null>(null);
  const metadataLoadedRef = useRef(false);
  const pendingSeekRef = useRef<{ time: number; stop: number | null; autoPlay: boolean } | null>(null);

  const [usePreviewProxy, setUsePreviewProxy] = useState(false);
  const [previewLocalPath, setPreviewLocalPath] = useState<string | null>(null);
  const [cacheBust, setCacheBust] = useState<number | undefined>(undefined);
  const [transcoding, setTranscoding] = useState(false);
  const [videoError, setVideoError] = useState<string | null>(null);

  useEffect(() => {
    stopAt.current = null;
    fallbackTriedRef.current = null;
    metadataLoadedRef.current = false;
    pendingSeekRef.current = null;
    setUsePreviewProxy(false);
    setPreviewLocalPath(null);
    setCacheBust(undefined);
    setTranscoding(false);
    setVideoError(null);
  }, [hit.mediaId]);

  const videoUrl = useMemo(() => {
    if (bridgeRunning && bridgePort > 0) {
      return videoStreamUrl(bridgePort, hit.mediaId, usePreviewProxy, cacheBust);
    }
    return src(usePreviewProxy && previewLocalPath ? previewLocalPath : hit.path);
  }, [bridgeRunning, bridgePort, hit.mediaId, hit.path, usePreviewProxy, previewLocalPath, cacheBust]);

  const now = () => Math.round((ref.current?.currentTime ?? 0) * 10) / 10;
  const play = (a: number, b?: number) => {
    const v = ref.current;
    if (!v || transcoding || videoError) return;
    if (!metadataLoadedRef.current || v.readyState < 1) {
      pendingSeekRef.current = { time: a, stop: b ?? null, autoPlay: true };
      return;
    }
    stopAt.current = b ?? null;
    v.currentTime = a;
    v.play().catch(() => {});
  };
  const set = (i: number, seg: [number, number]) => {
    const n = segs.map((s, k) => (k === i ? seg : s));
    onChange(n.sort((x, y) => x[0] - y[0]));
  };
  return (
    <>
      {transcoding && (
        <div
          style={{
            minWidth: 520,
            maxWidth: "86vw",
            minHeight: 280,
            background: "#111827",
            color: "#f9fafb",
            borderRadius: 8,
            display: "grid",
            placeItems: "center",
            padding: 24,
            textAlign: "center",
          }}
        >
          <div>
            <div style={{ fontSize: 15, fontWeight: 600, marginBottom: 6 }}>
              检测到 H.265/特殊编码，正在用 ffmpeg 生成兼容预览…
            </div>
            <div className="small" style={{ color: "#9ca3af" }}>
              首次打开会自动生成快速预览缓存（不影响裁剪导出原画质），请稍候几秒
            </div>
          </div>
        </div>
      )}
      {videoError && !transcoding && (
        <div className="ffmpeg-banner" style={{ marginBottom: 0, maxWidth: "86vw" }}>
          <span>
            <b>视频预览失败：</b>
            {videoError}
          </span>
          <div className="spacer" />
          <button className="ghost small" onClick={() => openPath(hit.path).catch(() => {})}>
            用系统默认播放器打开
          </button>
        </div>
      )}
      <video
        ref={ref}
        src={videoUrl}
        poster={hit.poster ? src(hit.poster) : undefined}
        controls
        autoPlay
        preload="metadata"
        style={{ display: transcoding || videoError ? "none" : "block" }}
        onLoadedMetadata={() => {
          metadataLoadedRef.current = true;
          const v = ref.current;
          if (!v) return;
          if (pendingSeekRef.current) {
            const p = pendingSeekRef.current;
            pendingSeekRef.current = null;
            stopAt.current = p.stop;
            v.currentTime = p.time;
            if (p.autoPlay) v.play().catch(() => {});
            return;
          }
          if (initialRange) {
            stopAt.current = initialRange[1];
            v.currentTime = initialRange[0];
          } else {
            stopAt.current = null;
            v.currentTime = segs[0]?.[0] ?? 0;
          }
          v.play().catch(() => {});
        }}
        onError={() => {
          if (transcoding) return;
          if (fallbackTriedRef.current !== hit.mediaId && !usePreviewProxy) {
            fallbackTriedRef.current = hit.mediaId;
            metadataLoadedRef.current = false;
            setVideoError(null);
            setTranscoding(true);
            const targetId = hit.mediaId;
            api
              .prepareVideoPreview(targetId)
              .then((localPath) => {
                if (activeMediaIdRef.current !== targetId) return;
                setPreviewLocalPath(localPath);
                setCacheBust(Date.now());
                setUsePreviewProxy(true);
                setTranscoding(false);
              })
              .catch((err) => {
                if (activeMediaIdRef.current !== targetId) return;
                setTranscoding(false);
                setVideoError(String(err));
              });
            return;
          }
          setVideoError("当前系统解码器无法播放该视频，可点击右侧按钮使用系统播放器打开，或直接裁剪导出 H.264 片段。");
        }}
        onTimeUpdate={() => {
          const v = ref.current;
          if (v && stopAt.current !== null && v.currentTime >= stopAt.current) {
            v.pause();
            stopAt.current = null;
          }
        }}
      />
      <div className="segs">
        <div className="row">
          <b>导出的片段</b>
          <span className="muted small">（时间可直接改，格式 分:秒；也可以暂停到想要的位置后点「设为起点/终点」）</span>
          <div className="spacer" />
          <button className="ghost" onClick={() => onChange([...segs, [now(), now() + 5]].sort((x, y) => x[0] - y[0]) as [number, number][])}>
            + 从当前位置加一段
          </button>
          {edited && (
            <button className="ghost" onClick={() => onChange(null)}>
              恢复自动
            </button>
          )}
        </div>
        {segs.length === 0 && <p className="muted small">没有片段，导出时会复制整个视频。</p>}
        {segs.map(([a, b], i) => (
          <div className="row seg" key={i}>
            <span className="muted">片段 {i + 1}</span>
            <TimeInput value={a} onCommit={(v) => set(i, [v, Math.max(b, v + 0.5)])} />
            –
            <TimeInput value={b} onCommit={(v) => set(i, [Math.min(a, v - 0.5), v].map((x) => Math.max(0, x)) as [number, number])} />
            <span className="muted small">{(b - a).toFixed(1)} 秒</span>
            <button className="ghost" onClick={() => set(i, [now(), Math.max(b, now() + 0.5)])}>设为起点</button>
            <button className="ghost" onClick={() => set(i, [Math.min(a, Math.max(0, now() - 0.5)), now()])}>设为终点</button>
            <button className="ghost" onClick={() => play(a, b)}>▶ 试看</button>
            <button className="icon" title="删除这段" onClick={() => onChange(segs.filter((_, k) => k !== i))}>✕</button>
          </div>
        ))}
      </div>
    </>
  );
}

function TimeInput({ value, onCommit }: { value: number; onCommit: (v: number) => void }) {
  const fmt = (s: number) => `${Math.floor(s / 60)}:${(s % 60).toFixed(1).padStart(4, "0")}`;
  const [text, setText] = useState(fmt(value));
  useEffect(() => setText(fmt(value)), [value]);
  const commit = () => {
    const v = parseTime(text);
    if (v === null) setText(fmt(value));
    else if (v !== value) onCommit(v);
  };
  return (
    <input
      className="time"
      value={text}
      onChange={(e) => setText(e.target.value)}
      onBlur={commit}
      onKeyDown={(e) => e.key === "Enter" && (e.target as HTMLInputElement).blur()}
    />
  );
}

function StorageCard({ status, refresh, notify }: Common & { status: Status }) {
  const [stats, setStats] = useState<StorageStats | null>(null);
  const [busy, setBusy] = useState<"mig" | "purge" | "">("");

  const loadStats = useCallback(() => {
    api.storageStats().then(setStats).catch(() => {});
  }, []);

  useEffect(() => {
    loadStats();
  }, [loadStats, status.storageRoot, status.total, status.pending]);

  const handleMigrate = async () => {
    const dest = await open({ directory: true, title: "选择新的媒体库存储目录（支持外接移动硬盘或其他大容量磁盘）" });
    if (!dest) return;
    if (
      !confirm(
        `将媒体库与缩略图迁移到：\n${dest}\n\n迁移过程会对每个文件做 SHA-256 完整性校验，全部校验通过后才会清理旧目录副本。继续？`,
      )
    ) {
      return;
    }
    setBusy("mig");
    try {
      const r = await api.migrateStorage(dest as string);
      notify(`存储目录迁移完成：校验并迁移 ${r.copiedFiles} 个文件（${fmtBytes(r.copiedBytes)}）`);
      await refresh();
      loadStats();
    } catch (e) {
      notify(String(e));
    }
    setBusy("");
  };

  const handlePurge = async () => {
    if (
      !confirm(
        "确认清理媒体库中「未命中任何已录入宝宝（含人脸与作品署名）」的受管原图和原视频吗？\n\n• 保护所有宝宝的人脸照与署名作品照绝不误删\n• 绝不改动电脑其他文件夹及微信目录里的外部原片\n• 保留文件指纹记录，今后重复导入或同步仍会秒级跳过",
      )
    ) {
      return;
    }
    setBusy("purge");
    try {
      const r = await api.purgeUnmatched();
      notify(
        `空间清理完成：释放 ${r.purgedCount} 个无娃原片（节省 ${fmtBytes(r.freedBytes)}），已保护 ${r.skippedKept} 个宝宝媒体`,
      );
      await refresh();
      loadStats();
    } catch (e) {
      notify(String(e));
    }
    setBusy("");
  };

  return (
    <div className="card">
      <div className="row">
        <h3>存储空间与目录迁移</h3>
        {stats?.isCustom && <span className="ok small">自定义存储盘</span>}
        <div className="spacer" />
        <button className="ghost" disabled={!!busy || status.processing} onClick={() => openPath(status.storageRoot)}>
          打开存储目录
        </button>
        <button className="ghost" disabled={!!busy || status.processing} onClick={handleMigrate}>
          {busy === "mig" ? "校验迁移中…" : "迁移存储目录到其他盘…"}
        </button>
        <button className="danger" disabled={!!busy || status.processing} onClick={handlePurge}>
          {busy === "purge" ? "清理中…" : "一键清理未命中原片"}
        </button>
      </div>
      <p className="hint small" style={{ marginBottom: 6 }}>
        当前存储路径：<code>{status.storageRoot}</code>
        {status.storageRoot !== status.dataDir && (
          <span className="muted">（配置与数据库：<code>{status.dataDir}</code>）</span>
        )}
      </p>
      {stats && (
        <div className="row small muted">
          <span>
            媒体库占用：<b>{fmtBytes(stats.libraryBytes)}</b>
          </span>
          <span>· 缩略图：{fmtBytes(stats.thumbsBytes)}</span>
          <span>· 当前磁盘可用：{fmtBytes(stats.freeBytes)}</span>
          {stats.externalCount > 0 && <span>· 原位外部视频：{stats.externalCount} 个</span>}
          {stats.purgedCount > 0 && <span>· 已释放无娃文件：{stats.purgedCount} 个（指纹已保留）</span>}
        </div>
      )}
    </div>
  );
}

function Settings({
  status,
  persons,
  onInstallExt,
  refresh,
  notify,
}: Common & { status: Status; persons: Person[]; onInstallExt: () => void }) {
  const [busy, setBusy] = useState("");
  const run = async (key: string, f: () => Promise<unknown>, ok: string) => {
    setBusy(key);
    try {
      await f();
      if (ok) notify(ok);
    } catch (e) {
      notify(String(e));
    }
    setBusy("");
    refresh();
  };
  const allModelsReady = status.modelsReady && status.ocrModelsReady;
  return (
    <section>
      <h2>设置</h2>
      <div className="card">
        <div className="row">
          <h3>离线识别模型（人脸 + 作品识字模型）</h3>
          <span className={status.modelsReady ? "ok small" : "warn small"}>
            人脸模型：{status.modelsReady ? "已就绪" : "待下载"}
          </span>
          <span className={status.ocrModelsReady ? "ok small" : "warn small"}>
            作品识字模型：{status.ocrModelsReady ? "已就绪" : "待下载"}
          </span>
          <div className="spacer" />
          {allModelsReady ? (
            <span className="ok">全部已就绪</span>
          ) : (
            <button disabled={!!busy} onClick={() => run("m", api.downloadModels, "人脸与作品识字模型下载完成")}>
              {busy === "m"
                ? "下载中…"
                : status.modelsReady
                  ? "补充下载作品识字模型（约 20MB）"
                  : "下载人脸 + 作品识字模型（约 210MB）"}
            </button>
          )}
        </div>
        <p className="hint">
          包含 InsightFace 人脸识别模型与 PP-OCRv5 中文手写/印刷署名识字模型（支持 1.8 万生僻名字字如「淏、浛、祎、玥」及竖排落款），100% 本机离线运行，照片绝不上传。
        </p>
      </div>
      <StorageCard status={status} refresh={refresh} notify={notify} />
      <ChromeExtensionCard status={status} persons={persons} onInstallExt={onInstallExt} refresh={refresh} notify={notify} />
      <div className="card">
        <div className="row">
          <h3>ffmpeg（处理视频用）</h3>
          <div className="spacer" />
          {status.ffmpeg ? (
            <span className="ok" title={status.ffmpeg}>已就绪</span>
          ) : (
            <button disabled={!!busy} onClick={() => run("f", api.installFfmpeg, "ffmpeg 安装完成")}>
              {busy === "f" ? "安装中…" : "自动安装"}
            </button>
          )}
        </div>
        {status.ffmpeg && <p className="hint small">{status.ffmpeg}</p>}
      </div>
      <div className="card">
        <div className="row">
          <h3>配置与数据库目录</h3>
          <div className="spacer" />
          <button className="ghost" onClick={() => openPath(status.dataDir)}>
            打开
          </button>
        </div>
        <p className="hint small">{status.dataDir}</p>
      </div>
      <div className="card">
        <div className="row">
          <h3>检查更新</h3>
          <span className="muted small">当前桌面端 v{status.appVersion} · 内置扩展 v{status.extensionVersion}</span>
          <div className="spacer" />
          <button
            disabled={!!busy}
            onClick={() =>
              run(
                "u",
                async () => {
                  const u = await check();
                  if (!u) throw "已经是最新版本";
                  if (confirm(`发现新版本 ${u.version}，现在更新？`)) {
                    await u.downloadAndInstall();
                    await relaunch();
                  }
                },
                "",
              )
            }
          >
            检查
          </button>
        </div>
      </div>
    </section>
  );
}

function ChromeExtensionCard({
  status,
  persons,
  onInstallExt,
  refresh,
  notify,
}: {
  status: Status;
  persons: Person[];
  onInstallExt: () => void;
  refresh: () => Promise<void>;
  notify: (m: string) => void;
}) {
  const readyKids = persons.filter((p) => p.refs.length > 0 || p.keywords.length > 0);
  return (
    <div className="card ext-card">
      <div className="row">
        <h3>Chrome 扩展 · 群相册网页直接识别勾选（人脸 + 作品署名）</h3>
        <span className="muted small">扩展 v{status.extensionVersion} · 桌面端 v{status.appVersion}</span>
        <span className={status.bridgeRunning ? "ok small" : "warn small"}>
          {status.bridgeRunning ? `本地服务已就绪 (127.0.0.1:${status.bridgePort})` : "本地服务未启动"}
        </span>
        <div className="spacer" />
        <button onClick={onInstallExt}>一键安装 / 更新扩展</button>
        <button className="ghost" onClick={() => api.openQxcWeb().catch((e) => notify(String(e)))}>
          打开群相册网页
        </button>
      </div>
      <p className="hint" style={{ marginBottom: 4 }}>
        不想把几百上千张照片全下到本地？装上扩展后，在 <b>qxc.hi.cn</b> 网页里支持<b>增量扫新增</b>与<b>作品署名识别</b>，扫过的照片特征与文字会持久化保存在本机数据库，以后自动跳过不重复扫描。
      </p>
      <div className="row small muted">
        {readyKids.length > 0 ? (
          <span>
            已同步宝宝：
            <b style={{ color: "#2e7d32" }}>
              {readyKids.map((p) => `${p.name}（${p.refs.length} 张照${p.keywords.length ? ` · 署名 ${p.keywords.length} 词` : ""}）`).join("、")}
            </b>
            ，网页端无需再次上传
          </span>
        ) : (
          <span>提示：先在「① 录入宝宝」添加正脸照或姓名，装好扩展后会自动同步到网页端</span>
        )}
        <span>· 已缓存网页照片 {status.webScanned} 张</span>
        {status.webScanned > 0 && (
          <a
            onClick={() => {
              if (confirm(`清空 ${status.webScanned} 张网页扫描缓存？下次打开群相册会重新识别这些照片。`)) {
                api
                  .clearWebCache()
                  .then(() => {
                    notify("已清空网页扫描缓存");
                    refresh();
                  })
                  .catch((e) => notify(String(e)));
              }
            }}
          >
            清空缓存
          </a>
        )}
      </div>
    </div>
  );
}

function ExtensionInstallModal({
  result,
  extensionVersion,
  persons,
  onClose,
  onCopy,
  onOpenFolder,
  onOpenQxc,
}: {
  result: ExtensionInstallResult;
  extensionVersion?: string;
  persons: Person[];
  onClose: () => void;
  onCopy: () => void;
  onOpenFolder: () => void;
  onOpenQxc: () => void;
}) {
  const readyKids = persons.filter((p) => p.refs.length > 0 || p.keywords.length > 0);
  return (
    <div className="modal" onClick={onClose}>
      <div className="ext-modal" onClick={(e) => e.stopPropagation()}>
        <div className="row">
          <h3 style={{ fontSize: 16 }}>
            安装「找娃助手」Chrome 扩展{extensionVersion ? ` v${extensionVersion}` : ""}
          </h3>
          <div className="spacer" />
          <button className="icon" onClick={onClose}>
            ✕
          </button>
        </div>
        <div className="ext-status-list">
          <div className="ok">✓ 扩展文件已准备就绪，路径已复制到剪贴板</div>
          {result.openedBrowser ? (
            <div className="ok">✓ 已为你打开 {result.browser} 扩展管理页</div>
          ) : (
            <div className="warn">• 请手动在浏览器地址栏打开 <code>chrome://extensions/</code></div>
          )}
          {result.openedFolder && <div className="ok">✓ 已在文件管理器中定位到 chrome-extension 文件夹</div>}
        </div>
        <ol className="ext-steps">
          <li>
            在浏览器扩展页右上角开启 <b>「开发者模式」</b>（如果已开启可跳过）。
          </li>
          <li>
            把刚打开的 <b>chrome-extension</b> 文件夹<b>直接拖入浏览器扩展页</b>即可完成安装
            <span className="muted">（或点击左上角「加载未打包的扩展程序」，按 Ctrl+V 粘贴路径并确认）</span>。
          </li>
          <li>
            保持本应用开启，打开 <b>https://qxc.hi.cn/#/</b> 群相册网页，右下角会自动出现「找娃助手」浮窗，一键识别并勾选宝宝照片与署名作品！
          </li>
        </ol>
        {readyKids.length > 0 && (
          <div className="banner" style={{ marginBottom: 0 }}>
            已同步宝宝：<b>{readyKids.map((p) => `${p.name}（${p.refs.length} 张参考照）`).join("、")}</b>，在网页端无需重新上传！
          </div>
        )}
        <div className="ext-path">{result.dir}</div>
        <div className="row">
          <button onClick={onOpenQxc}>打开群相册网页 (qxc.hi.cn)</button>
          <button className="ghost" onClick={onOpenFolder}>
            打开扩展文件夹
          </button>
          <button className="ghost" onClick={onCopy}>
            复制路径
          </button>
          <div className="spacer" />
          <button className="ghost" onClick={onClose}>
            完成
          </button>
        </div>
      </div>
    </div>
  );
}
