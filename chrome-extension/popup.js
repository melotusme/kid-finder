function bridge(method, path) {
  return new Promise((resolve) => {
    chrome.runtime.sendMessage({ type: "bridge", method, path }, (resp) => {
      resolve(resp || { ok: false, offline: true });
    });
  });
}

async function init() {
  const el = document.getElementById("status");
  const resp = await bridge("GET", "/api/status");
  if (!resp.ok || !resp.data || !resp.data.ok) {
    el.innerHTML = `<div class="err">未连接到桌面端</div><div class="muted">请先打开「找娃助手」桌面应用（本地端口 27431）。</div>`;
    return;
  }
  const d = resp.data;
  const kids = (d.persons || []).filter((p) => p.refCount > 0);
  const kidText = kids.length
    ? `已同步宝宝：${kids.map((k) => `${k.name}(${k.refCount}张)`).join("、")}`
    : "尚未录入宝宝，可在桌面端或群相册网页浮窗中录入。";
  el.innerHTML = `
    <div class="ok">已连接桌面端 v${d.version || ""}</div>
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
