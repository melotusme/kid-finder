import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";
import { openPath, revealItemInDir } from "@tauri-apps/plugin-opener";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { api, src, fmtTime, parseTime, padSegments, loadClipSettings, saveClipSettings, type ClipSettings, type Status, type Person, type Hit, type Progress, type ImportReport, type Album, type LoginTicket, type ExtensionInstallResult } from "./api";

type Tab = "kids" | "import" | "photos" | "videos" | "settings";
const MEDIA_EXT = ["jpg", "jpeg", "png", "webp", "bmp", "gif", "mp4", "mov", "m4v", "avi", "mkv", "3gp", "webm"];

export default function App() {
  const [tab, setTab] = useState<Tab>("kids");
  const [status, setStatus] = useState<Status | null>(null);
  const [persons, setPersons] = useState<Person[]>([]);
  const [selectedPersonId, setSelectedPersonId] = useState<number | null>(null);
  const [threshold, setThreshold] = useState<number>(0.4);
  const thresholdSynced = useRef(false);
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
      setThreshold(s.threshold);
      thresholdSynced.current = true;
    }
  }, []);
  const notify = (m: string) => {
    setToast(m);
    setTimeout(() => setToast((t) => (t === m ? null : t)), 6000);
  };

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
  }, [refresh]);

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
    ];
    check().then((u) => u && setUpdate(u)).catch(() => {});
    return () => uns.forEach((u) => u.then((f) => f()));
  }, [refresh]);

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
          ] as [Tab, string][]
        ).map(([k, label]) => (
          <button key={k} className={tab === k ? "active" : ""} onClick={() => setTab(k)}>
            {label}
          </button>
        ))}
        <div className="spacer" />
        <button className="ext-nav-btn" onClick={triggerInstallExtension} title="安装 Chrome 扩展，在群相册网页直接人脸识别并勾选">
          安装 Chrome 扩展
        </button>
        {status && (
          <div className="stat">
            库内 {status.images} 张照片 · {status.videos} 个视频
            {status.pending > 0 && <div>待分析 {status.pending}</div>}
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
        {progress && (
          <div className="progress">
            <span>{{ import: "导入中", analyze: "人脸分析中", export: "导出中" }[progress.stage]}</span>
            <progress value={progress.done} max={Math.max(progress.total, 1)} />
            <span>
              {progress.done}/{progress.total} {progress.current}
            </span>
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
            threshold={threshold}
            onChangeThreshold={setThreshold}
            onGoKids={() => setTab("kids")}
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
            threshold={threshold}
            onChangeThreshold={setThreshold}
            onGoKids={() => setTab("kids")}
            refresh={refresh}
            notify={notify}
          />
        )}
        {tab === "settings" && status && (
          <Settings status={status} persons={persons} onInstallExt={triggerInstallExtension} refresh={refresh} notify={notify} />
        )}
      </main>
      {extGuide && (
        <ExtensionInstallModal
          result={extGuide}
          persons={persons}
          onClose={() => setExtGuide(null)}
          onCopy={() => {
            navigator.clipboard?.writeText(extGuide.dir).then(() => notify("已复制扩展目录路径"));
          }}
          onOpenFolder={() => openPath(extGuide.dir).catch(() => {})}
          onOpenQxc={() => api.openQxcWeb().catch((e) => notify(String(e)))}
        />
      )}
      {toast && <div className="toast" onClick={() => setToast(null)}>{toast}</div>}
    </div>
  );
}

type Common = { refresh: () => Promise<void>; notify: (m: string) => void };

function Kids({ persons, refresh, notify }: Common & { persons: Person[] }) {
  const [name, setName] = useState("");
  const [busy, setBusy] = useState<number | null>(null);
  const addRefs = async (p: Person) => {
    const files = await open({ multiple: true, filters: [{ name: "照片", extensions: ["jpg", "jpeg", "png", "webp", "bmp"] }] });
    if (!files || files.length === 0) return;
    setBusy(p.id);
    try {
      const r = await api.addRefs(p.id, files as string[]);
      const msg = [`已录入 ${r.added} 张`];
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
      <p className="hint">为每个孩子添加 3~10 张清晰的正脸照（最好是不同角度、不同时间），识别会更准。照片只保存在本机。</p>
      <div className="row">
        <input placeholder="宝宝的名字" value={name} onChange={(e) => setName(e.target.value)} />
        <button
          disabled={!name.trim()}
          onClick={async () => {
            await api.addPerson(name);
            setName("");
            refresh();
          }}
        >
          添加
        </button>
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
            <button
              className="ghost"
              onClick={() => {
                const n = prompt("新名字", p.name);
                if (n?.trim()) api.renamePerson(p.id, n).then(refresh);
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
        新导入 <b>{r.added}</b> 个
        {r.duplicates.length > 0 && <>，<b className="warn">{r.duplicates.length}</b> 个之前已导入过（已跳过）</>}
        {r.unsupported > 0 && <>，{r.unsupported} 个不是照片/视频</>}
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
    [refresh],
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
        把微信里保存下来的照片/视频（或整个文件夹）拖进来，照片与视频会自动分类归档到「③ 宝宝照片」与「④ 宝宝视频」。同一个文件导入过会自动跳过，不会重复导入。
        {!status.ffmpeg && <b className="warn"> 处理视频需要 ffmpeg，请到「④ 宝宝视频」或「设置」一键安装。</b>}
      </p>
      <div className={"drop" + (hover ? " hover" : "")}>
        <div>拖放文件或文件夹到这里</div>
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
  }, [ticket]);

  useEffect(() => {
    if (status.qxcUser !== null) api.qxcAlbums().then(setAlbums).catch((e) => notify(String(e)));
    else setAlbums(null);
  }, [status.qxcUser]);

  useEffect(() => {
    const un = listen<{ page: number; added: number; duplicates: number }>("sync-progress", (e) =>
      setSyncInfo(`第 ${e.payload.page + 1} 页 · 新增 ${e.payload.added} · 已有 ${e.payload.duplicates}`),
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
      {albums?.map((a) => (
        <div className="row album" key={a.groupId}>
          <span>{a.groupName}</span>
          <span className="muted">
            {a.imgCnt} 张 · {a.videoCnt} 个视频
          </span>
          <div className="spacer" />
          {syncing === a.groupId && <span className="muted">{syncInfo}</span>}
          <button
            disabled={syncing !== null}
            onClick={async () => {
              setSyncing(a.groupId);
              setSyncInfo("");
              try {
                const r = await api.qxcSync(a.groupId);
                notify(
                  `同步完成：新增 ${r.added} 个，${r.duplicates} 个之前已有（已跳过）` +
                    (r.errors.length ? `，${r.errors.length} 个失败` : ""),
                );
              } catch (e) {
                notify(String(e));
              }
              setSyncing(null);
              refresh();
            }}
          >
            {syncing === a.groupId ? "同步中…" : "同步"}
          </button>
        </div>
      ))}
    </div>
  );
}

type MediaViewProps = Common & {
  persons: Person[];
  status: Status;
  pid: number | null;
  onSelectPid: (id: number) => void;
  threshold: number;
  onChangeThreshold: (v: number) => void;
  onGoKids: () => void;
};

function usePersonHits({
  kind,
  persons,
  pid,
  threshold,
  refsSignature,
  pending,
  notify,
}: {
  kind: "image" | "video";
  persons: Person[];
  pid: number | null;
  threshold: number;
  refsSignature: string;
  pending: number;
  notify: (m: string) => void;
}) {
  const [hits, setHits] = useState<Hit[]>([]);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [loading, setLoading] = useState(false);
  const reqSeq = useRef(0);

  const currentPerson = persons.find((p) => p.id === pid) ?? null;
  const refsKey = currentPerson?.refs.map((r) => r.id).join(",") ?? "";
  const hasRefs = (currentPerson?.refs.length ?? 0) > 0;

  const load = useCallback(async () => {
    const seq = ++reqSeq.current;
    if (pid === null || !currentPerson || !hasRefs) {
      setHits([]);
      setSelected(new Set());
      setLoading(false);
      return;
    }
    setLoading(true);
    try {
      const all = await api.search(pid, threshold);
      if (seq !== reqSeq.current) return;
      const filtered = all.filter((h) => h.kind === kind);
      setHits(filtered);
      setSelected(new Set(filtered.map((x) => x.mediaId)));
    } finally {
      if (seq === reqSeq.current) setLoading(false);
    }
  }, [pid, currentPerson?.id, hasRefs, refsKey, refsSignature, threshold, kind]);

  useEffect(() => {
    load().catch((e) => notify(String(e)));
  }, [load, pending]);

  const toggle = (id: number) =>
    setSelected((s) => {
      const n = new Set(s);
      n.has(id) ? n.delete(id) : n.add(id);
      return n;
    });

  return { hits, selected, setSelected, loading, currentPerson, hasRefs, load, toggle };
}

function PhotosView({ persons, status, pid, onSelectPid, threshold, onChangeThreshold, onGoKids, refresh, notify }: MediaViewProps) {
  const { hits, selected, setSelected, loading, currentPerson, hasRefs, load, toggle } = usePersonHits({
    kind: "image",
    persons,
    pid,
    threshold,
    refsSignature: status.refsSignature,
    pending: status.pending,
    notify,
  });
  const [preview, setPreview] = useState<Hit | null>(null);

  useEffect(() => {
    setPreview((prev) => (prev ? hits.find((x) => x.mediaId === prev.mediaId) ?? null : null));
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

  const doExport = async () => {
    const dir = await open({ directory: true, title: "导出照片到哪个文件夹？" });
    if (!dir) return;
    const items = hits.filter((h) => selected.has(h.mediaId)).map((h) => ({ mediaId: h.mediaId, segments: [] as [number, number][] }));
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
        <label className="muted" title="越高越严格：漏掉的多，认错的少">
          严格程度 {threshold.toFixed(2)}
          <input
            type="range"
            min={0.25}
            max={0.65}
            step={0.01}
            value={threshold}
            onChange={(e) => onChangeThreshold(Number(e.target.value))}
            onMouseUp={() => api.setThreshold(threshold)}
            onTouchEnd={() => api.setThreshold(threshold)}
            onKeyUp={() => api.setThreshold(threshold)}
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
          找到 {hits.length} 张照片 · 已选 {selected.size}
        </span>
        <button className="ghost" disabled={!hits.length} onClick={() => setSelected(new Set(selected.size ? [] : hits.map((h) => h.mediaId)))}>
          {selected.size ? "全不选" : "全选"}
        </button>
        <button disabled={!selected.size} onClick={doExport}>
          导出选中照片
        </button>
      </div>
      {!hasRefs ? (
        <div className="card empty-card">
          <p className="hint">「{currentPerson?.name ?? "该宝宝"}」还没有参考照，请先到「① 录入宝宝」添加至少一张清晰的正脸照片。</p>
          <button onClick={onGoKids}>去「① 录入宝宝」添加参考照</button>
        </div>
      ) : (
        <>
          <p className="hint">认错了点 ✕ 排除，认对了点 ✓ 加入参考照，每次增删或确认参考照都会自动重算全部照片相似度。</p>
          {hits.length === 0 ? (
            <div className="card empty-card">
              <p className="hint" style={{ margin: 0 }}>
                {status.images === 0
                  ? "库内还没有导入照片，请先到「② 导入照片视频」导入。"
                  : "当前严格程度下没有找到匹配照片，可适当调低严格程度或补充更多角度的参考照。"}
              </p>
            </div>
          ) : (
            <div className="grid">
              {hits.map((h) => (
                <div key={h.mediaId} className={"tile" + (selected.has(h.mediaId) ? " sel" : "")}>
                  <div className="imgwrap" onClick={() => setPreview(h)}>
                    <img src={src(h.path)} loading="lazy" />
                    <div
                      className="box"
                      style={{
                        left: `${h.bbox[0] * 100}%`,
                        top: `${h.bbox[1] * 100}%`,
                        width: `${(h.bbox[2] - h.bbox[0]) * 100}%`,
                        height: `${(h.bbox[3] - h.bbox[1]) * 100}%`,
                      }}
                    />
                  </div>
                  <div className="row tilebar">
                    <input type="checkbox" checked={selected.has(h.mediaId)} onChange={() => toggle(h.mediaId)} />
                    <span className="muted small">
                      {h.time.slice(0, 10)} · {(h.score * 100).toFixed(0)}%
                    </span>
                    <div className="spacer" />
                    <button
                      className="icon"
                      title="是我家宝宝（加入参考并重算）"
                      onClick={() =>
                        api
                          .confirmFace(h.faceId, pid!)
                          .then(() => {
                            notify("已加入参考，已重新计算相似度");
                            return refresh();
                          })
                          .catch((e) => notify(String(e)))
                      }
                    >
                      ✓
                    </button>
                    <button
                      className="icon"
                      title="不是我家宝宝"
                      onClick={() =>
                        api
                          .rejectFace(h.faceId, pid!)
                          .then(() => refresh())
                          .catch((e) => notify(String(e)))
                      }
                    >
                      ✕
                    </button>
                  </div>
                </div>
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
                {preview.origName} <span className="muted small">· 相似度 {(preview.score * 100).toFixed(0)}%</span>
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

function VideosView({ persons, status, pid, onSelectPid, threshold, onChangeThreshold, onGoKids, refresh, notify }: MediaViewProps) {
  const { hits, selected, setSelected, loading, currentPerson, hasRefs, load, toggle } = usePersonHits({
    kind: "video",
    persons,
    pid,
    threshold,
    refsSignature: status.refsSignature,
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
          严格程度 {threshold.toFixed(2)}
          <input
            type="range"
            min={0.25}
            max={0.65}
            step={0.01}
            value={threshold}
            onChange={(e) => onChangeThreshold(Number(e.target.value))}
            onMouseUp={() => api.setThreshold(threshold)}
            onTouchEnd={() => api.setThreshold(threshold)}
            onKeyUp={() => api.setThreshold(threshold)}
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
        <button className="ghost" disabled={!hits.length} onClick={() => setSelected(new Set(selected.size ? [] : hits.map((h) => h.mediaId)))}>
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
              {hits.map((h) => {
                const segs = segsOf(h);
                const clipSum = segs.reduce((s, [a, b]) => s + b - a, 0);
                const isEdited = editKey(h.mediaId) in edits;
                return (
                  <div key={h.mediaId} className={"tile video-card" + (selected.has(h.mediaId) ? " sel" : "")}>
                    <div className="vidwrap" onClick={() => setPreview({ hit: h, initialRange: null })}>
                      {h.poster ? <img src={src(h.poster)} loading="lazy" /> : <div className="noposter">▶ 视频预览</div>}
                      <div className="badge">
                        ▶ {segs.length} 段 · 共 {fmtTime(clipSum)}
                        {isEdited && " · 已手动调整"}
                      </div>
                      {h.duration !== null && <div className="dur-badge">总长 {fmtTime(h.duration)}</div>}
                    </div>
                    {segs.length > 0 && (
                      <div className="seg-chips">
                        {segs.map(([a, b], idx) => (
                          <button
                            key={idx}
                            className="seg-chip"
                            title="点击试看并微调该片段"
                            onClick={() => setPreview({ hit: h, initialRange: [a, b] })}
                          >
                            {fmtTime(a)}–{fmtTime(b)}
                          </button>
                        ))}
                      </div>
                    )}
                    <div className="row tilebar">
                      <input type="checkbox" checked={selected.has(h.mediaId)} onChange={() => toggle(h.mediaId)} />
                      <span className="muted small">
                        {h.time.slice(0, 10)} · {(h.score * 100).toFixed(0)}%
                      </span>
                      <div className="spacer" />
                      <button
                        className="icon"
                        title="是我家宝宝（加入参考并重算）"
                        onClick={() =>
                          api
                            .confirmFace(h.faceId, pid!)
                            .then(() => {
                              notify("已加入参考，已重新计算相似度");
                              return refresh();
                            })
                            .catch((e) => notify(String(e)))
                        }
                      >
                        ✓
                      </button>
                      <button
                        className="icon"
                        title="不是我家宝宝"
                        onClick={() =>
                          api
                            .rejectFace(h.faceId, pid!)
                            .then(() => refresh())
                            .catch((e) => notify(String(e)))
                        }
                      >
                        ✕
                      </button>
                    </div>
                  </div>
                );
              })}
            </div>
          )}
        </>
      )}
      {preview && (
        <div className="modal" onClick={() => setPreview(null)}>
          <div onClick={(e) => e.stopPropagation()}>
            <VideoPreview
              hit={preview.hit}
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
  segs,
  initialRange,
  edited,
  onChange,
}: {
  hit: Hit;
  segs: [number, number][];
  initialRange?: [number, number] | null;
  edited: boolean;
  onChange: (s: [number, number][] | null) => void;
}) {
  const ref = useRef<HTMLVideoElement>(null);
  const stopAt = useRef<number | null>(null);
  const now = () => Math.round((ref.current?.currentTime ?? 0) * 10) / 10;
  const play = (a: number, b?: number) => {
    const v = ref.current;
    if (!v) return;
    stopAt.current = b ?? null;
    v.currentTime = a;
    v.play();
  };
  const set = (i: number, seg: [number, number]) => {
    const n = segs.map((s, k) => (k === i ? seg : s));
    onChange(n.sort((x, y) => x[0] - y[0]));
  };
  return (
    <>
      <video
        ref={ref}
        src={src(hit.path)}
        controls
        autoPlay
        onLoadedMetadata={() => {
          if (!ref.current) return;
          if (initialRange) {
            stopAt.current = initialRange[1];
            ref.current.currentTime = initialRange[0];
          } else {
            ref.current.currentTime = segs[0]?.[0] ?? 0;
          }
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
      notify(ok);
    } catch (e) {
      notify(String(e));
    }
    setBusy("");
    refresh();
  };
  return (
    <section>
      <h2>设置</h2>
      <div className="card">
        <div className="row">
          <h3>人脸识别模型</h3>
          <div className="spacer" />
          {status.modelsReady ? (
            <span className="ok">已就绪</span>
          ) : (
            <button disabled={!!busy} onClick={() => run("m", api.downloadModels, "模型下载完成")}>
              {busy === "m" ? "下载中…" : "下载模型（约 190MB）"}
            </button>
          )}
        </div>
        <p className="hint">首次使用需要下载 InsightFace 开源模型，所有识别都在本机完成，照片不会上传。</p>
      </div>
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
          <h3>数据目录</h3>
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
  const readyKids = persons.filter((p) => p.refs.length > 0);
  return (
    <div className="card ext-card">
      <div className="row">
        <h3>Chrome 扩展 · 群相册网页直接识别勾选</h3>
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
        不想把几百上千张照片全下到本地？装上扩展后，在 <b>qxc.hi.cn</b> 网页里支持<b>增量扫新增</b>与<b>存量补扫</b>，扫过的照片特征会持久化保存在本机数据库，以后自动跳过不重复扫描。
      </p>
      <div className="row small muted">
        {readyKids.length > 0 ? (
          <span>
            已同步宝宝：<b style={{ color: "#2e7d32" }}>{readyKids.map((p) => `${p.name}（${p.refs.length} 张）`).join("、")}</b>
            ，网页端无需再次上传
          </span>
        ) : (
          <span>提示：先在「① 录入宝宝」添加正脸照，装好扩展后会自动同步到网页端</span>
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
  persons,
  onClose,
  onCopy,
  onOpenFolder,
  onOpenQxc,
}: {
  result: ExtensionInstallResult;
  persons: Person[];
  onClose: () => void;
  onCopy: () => void;
  onOpenFolder: () => void;
  onOpenQxc: () => void;
}) {
  const readyKids = persons.filter((p) => p.refs.length > 0);
  return (
    <div className="modal" onClick={onClose}>
      <div className="ext-modal" onClick={(e) => e.stopPropagation()}>
        <div className="row">
          <h3 style={{ fontSize: 16 }}>安装「找娃助手」Chrome 扩展</h3>
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
            保持本应用开启，打开 <b>https://qxc.hi.cn/#/</b> 群相册网页，右下角会自动出现「找娃助手」浮窗，一键识别并勾选宝宝照片！
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
