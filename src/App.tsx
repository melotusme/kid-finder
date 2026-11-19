import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";
import { openPath, revealItemInDir } from "@tauri-apps/plugin-opener";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { api, src, fmtTime, type Status, type Person, type Hit, type Progress, type ImportReport, type Album, type LoginTicket } from "./api";

type Tab = "kids" | "import" | "results" | "settings";
const MEDIA_EXT = ["jpg", "jpeg", "png", "webp", "bmp", "gif", "mp4", "mov", "m4v", "avi", "mkv", "3gp", "webm"];

export default function App() {
  const [tab, setTab] = useState<Tab>("kids");
  const [status, setStatus] = useState<Status | null>(null);
  const [persons, setPersons] = useState<Person[]>([]);
  const [progress, setProgress] = useState<Progress | null>(null);
  const [dl, setDl] = useState<{ label: string; done: number; total: number | null } | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  const [update, setUpdate] = useState<Update | null>(null);

  const refresh = useCallback(async () => {
    const [s, p] = await Promise.all([api.status(), api.persons()]);
    setStatus(s);
    setPersons(p);
  }, []);
  const notify = (m: string) => {
    setToast(m);
    setTimeout(() => setToast((t) => (t === m ? null : t)), 6000);
  };

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
            ["results", "③ 找到的宝宝"],
            ["settings", "设置"],
          ] as [Tab, string][]
        ).map(([k, label]) => (
          <button key={k} className={tab === k ? "active" : ""} onClick={() => setTab(k)}>
            {label}
          </button>
        ))}
        <div className="spacer" />
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
        {tab === "import" && status && <Import status={status} refresh={refresh} notify={notify} />}
        {tab === "results" && status && <Results persons={persons} status={status} refresh={refresh} notify={notify} />}
        {tab === "settings" && status && <Settings status={status} refresh={refresh} notify={notify} />}
      </main>
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

function Import({ status, refresh, notify }: Common & { status: Status }) {
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
        把微信里保存下来的照片/视频（或整个文件夹）拖进来。同一个文件导入过会自动识别并提示，不会重复导入。
        {!status.ffmpeg && <b className="warn"> 处理视频需要 ffmpeg，请到「设置」安装。</b>}
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

function Results({ persons, status, refresh, notify }: Common & { persons: Person[]; status: Status }) {
  const [pid, setPid] = useState<number | null>(persons[0]?.id ?? null);
  const [threshold, setThreshold] = useState(status.threshold);
  const [hits, setHits] = useState<Hit[]>([]);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [filter, setFilter] = useState<"all" | "image" | "video">("all");
  const [preview, setPreview] = useState<Hit | null>(null);

  const load = useCallback(async () => {
    if (pid === null) return;
    const h = await api.search(pid, threshold);
    setHits(h);
    setSelected(new Set(h.map((x) => x.mediaId)));
  }, [pid, threshold]);

  useEffect(() => {
    load().catch((e) => notify(String(e)));
  }, [load, status.pending]);

  if (!persons.length) return <section><p className="hint">请先在「录入宝宝」里添加孩子和参考照。</p></section>;

  const shown = hits.filter((h) => filter === "all" || h.kind === filter);
  const toggle = (id: number) =>
    setSelected((s) => {
      const n = new Set(s);
      n.has(id) ? n.delete(id) : n.add(id);
      return n;
    });

  const doExport = async () => {
    const dir = await open({ directory: true, title: "导出到哪个文件夹？" });
    if (!dir) return;
    const items = hits.filter((h) => selected.has(h.mediaId)).map((h) => ({ mediaId: h.mediaId, segments: h.segments }));
    try {
      const r = await api.exportItems(dir as string, items);
      notify(
        `已导出 ${r.images} 张照片、${r.clips} 段视频` +
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
      <div className="row toolbar">
        <select value={pid ?? ""} onChange={(e) => setPid(Number(e.target.value))}>
          {persons.map((p) => (
            <option key={p.id} value={p.id}>
              {p.name}
            </option>
          ))}
        </select>
        <select value={filter} onChange={(e) => setFilter(e.target.value as typeof filter)}>
          <option value="all">全部</option>
          <option value="image">只看照片</option>
          <option value="video">只看视频</option>
        </select>
        <label className="muted" title="越高越严格：漏掉的多，认错的少">
          严格程度 {threshold.toFixed(2)}
          <input
            type="range"
            min={0.25}
            max={0.65}
            step={0.01}
            value={threshold}
            onChange={(e) => setThreshold(Number(e.target.value))}
            onMouseUp={() => api.setThreshold(threshold)}
          />
        </label>
        <div className="spacer" />
        <span className="muted">
          找到 {shown.length} 个 · 已选 {selected.size}
        </span>
        <button className="ghost" onClick={() => setSelected(new Set(selected.size ? [] : hits.map((h) => h.mediaId)))}>
          {selected.size ? "全不选" : "全选"}
        </button>
        <button disabled={!selected.size} onClick={doExport}>
          导出选中
        </button>
      </div>
      <p className="hint">视频只会导出孩子出现的片段。认错了点 ✕，认对了点 ✓ 可以让识别越来越准。</p>
      <div className="grid">
        {shown.map((h) => (
          <div key={h.mediaId} className={"tile" + (selected.has(h.mediaId) ? " sel" : "")}>
            <div className="imgwrap" onClick={() => setPreview(h)}>
              <img src={src(h.kind === "image" ? h.path : h.poster)} loading="lazy" />
              {h.kind === "image" && (
                <div
                  className="box"
                  style={{
                    left: `${h.bbox[0] * 100}%`,
                    top: `${h.bbox[1] * 100}%`,
                    width: `${(h.bbox[2] - h.bbox[0]) * 100}%`,
                    height: `${(h.bbox[3] - h.bbox[1]) * 100}%`,
                  }}
                />
              )}
              {h.kind === "video" && (
                <div className="badge">
                  ▶ {h.segments.length} 段 · {fmtTime(h.segments.reduce((s, [a, b]) => s + b - a, 0))}
                </div>
              )}
            </div>
            <div className="row tilebar">
              <input type="checkbox" checked={selected.has(h.mediaId)} onChange={() => toggle(h.mediaId)} />
              <span className="muted small">
                {h.time.slice(0, 10)} · {(h.score * 100).toFixed(0)}%
              </span>
              <div className="spacer" />
              <button className="icon" title="是我家宝宝（加入参考）" onClick={() => api.confirmFace(h.faceId, pid!).then(() => { notify("已加入参考"); refresh(); })}>
                ✓
              </button>
              <button className="icon" title="不是我家宝宝" onClick={() => api.rejectFace(h.faceId, pid!).then(load)}>
                ✕
              </button>
            </div>
          </div>
        ))}
      </div>
      {preview && (
        <div className="modal" onClick={() => setPreview(null)}>
          <div onClick={(e) => e.stopPropagation()}>
            {preview.kind === "image" ? (
              <img src={src(preview.path)} />
            ) : (
              <VideoPreview hit={preview} />
            )}
            <div className="row">
              <span>{preview.origName}</span>
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

function VideoPreview({ hit }: { hit: Hit }) {
  const ref = useRef<HTMLVideoElement>(null);
  return (
    <>
      <video ref={ref} src={src(hit.path)} controls autoPlay onLoadedMetadata={() => ref.current && (ref.current.currentTime = hit.segments[0]?.[0] ?? 0)} />
      <div className="row">
        宝宝出现的片段：
        {hit.segments.map(([a, b], i) => (
          <button key={i} className="ghost" onClick={() => ref.current && ((ref.current.currentTime = a), ref.current.play())}>
            {fmtTime(a)}–{fmtTime(b)}
          </button>
        ))}
      </div>
    </>
  );
}

function Settings({ status, refresh, notify }: Common & { status: Status }) {
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
