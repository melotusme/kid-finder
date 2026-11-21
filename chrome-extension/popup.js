const EXT_VERSION = chrome.runtime.getManifest().version;

function parseVer(v) {
  const s = String(v || "").trim();
  if (!s) return null;
  const parts = s.split(".");
  if (parts.length < 2 || parts.length > 4) return null;
  const nums = [];
  for (const p of parts) {
    if (!/^\d+$/.test(p)) return null;
    nums.push(Number(p));
  }
  return nums;
}

function isCompatible(appVer, extVer) {
  const a = parseVer(appVer);
  const b = parseVer(extVer);
  if (!a || !b) return false;
  return a[0] === b[0] && a[1] === b[1];
}

function compareVer(a, b) {
  const pa = parseVer(a);
  const pb = parseVer(b);
  if (!pa || !pb) return 0;
  const len = Math.max(pa.length, pb.length);
  for (let i = 0; i < len; i++) {
    const va = pa[i] ?? 0;
    const vb = pb[i] ?? 0;
    if (va !== vb) return va > vb ? 1 : -1;
  }
  return 0;
}

function escapeHtml(s) {
  return String(s ?? "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function bridge(method, path) {
  return new Promise((resolve) => {
    chrome.runtime.sendMessage({ type: "bridge", method, path }, (resp) => {
      resolve(resp || { ok: false, offline: true });
    });
  });
}

async function init() {
  const verEl = document.getElementById("ext-ver");
  if (verEl) verEl.textContent = `v${EXT_VERSION}`;

  const el = document.getElementById("status");
  const openBtn = document.getElementById("open");
  const resp = await bridge("GET", "/api/status");
  if (!resp.ok || !resp.data || !resp.data.ok) {
    el.innerHTML = `<div class="err">未连接到桌面端</div><div class="muted">请先打开「找娃助手」桌面应用（本地端口 27431）。</div>`;
    return;
  }
  const d = resp.data;
  if (!isCompatible(d.version, EXT_VERSION)) {
    const targetVer = d.extensionVersion || d.version || "";
    el.innerHTML = `
      <div class="err">版本不匹配：扩展 v${escapeHtml(EXT_VERSION)} / 桌面端 v${escapeHtml(d.version || "")}</div>
      <div class="err-box">大版本基线不一致，扫描动作已停用。请打开 <code>chrome://extensions/</code>，在「找娃助手」上点 <b>↻</b> 加载 <b>v${escapeHtml(targetVer)}</b>。</div>
    `;
    if (openBtn) {
      openBtn.disabled = true;
      openBtn.title = "大版本不匹配，请先在 chrome://extensions/ 点击 ↻ 同步扩展版本";
    }
    return;
  }

  const updateTip =
    d.extensionVersion && compareVer(d.extensionVersion, EXT_VERSION) > 0
      ? `<div class="tip">扩展有小版本可刷新：当前 v${escapeHtml(EXT_VERSION)} → 本机 v${escapeHtml(d.extensionVersion)}，在 chrome://extensions 点 ↻ 即可同步</div>`
      : "";

  const kids = (d.persons || []).filter((p) => p.refCount > 0);
  const kidText = kids.length
    ? `已同步宝宝：${kids.map((k) => `${escapeHtml(k.name)}(${k.refCount}张)`).join("、")}`
    : "尚未录入宝宝，可在桌面端或群相册网页浮窗中录入。";
  el.innerHTML = `
    <div class="ok">已连接桌面端 v${escapeHtml(d.version || "")}</div>
    ${updateTip}
    <div class="muted" style="margin-top:4px;">${d.modelsReady ? "人脸模型已就绪" : "请先在桌面端「设置」下载模型"} · ${d.ffmpegReady ? "ffmpeg 已就绪" : "未装 ffmpeg"}</div>
    <div class="muted" style="margin-top:4px;">已缓存：照片 ${d.scannedCount ?? 0} 张 · 视频 ${d.videoScannedCount ?? 0} 个</div>
    <div class="muted" style="margin-top:4px;">${kidText}</div>
  `;
}

document.getElementById("open").onclick = () => {
  chrome.runtime.sendMessage({ type: "open-qxc" });
  window.close();
};

init();
