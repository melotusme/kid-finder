(() => {
  if (window.__KID_FINDER_EXT_LOADED__) return;
  window.__KID_FINDER_EXT_LOADED__ = true;

  // qxc.hi.cn 内置的未选中 / 已选中 radio 图标特征（来自前端 bundle）
  const UNCHECKED_SIG = "AAAEp0lEQVRYCdVZPUgcQRT2";
  const CHECKED_SIG = "AAAEGElEQVRYCc2Zv2/TQBTH";
  const MAX_BATCH_SELECT = 300;
  const BATCH_SIZE = 12;

  let observedUncheckedSrc = null;

  const state = {
    open: true,
    connected: false,
    modelsReady: false,
    offlineMsg: "",
    persons: [],
    personId: null,
    threshold: 0.4,
    skipVideos: true,
    onlyMatched: false,
    showEnroll: false,
    scanning: false,
    stopRequested: false,
    progressDone: 0,
    progressTotal: 0,
    progressLabel: "",
    notice: null, // { type: 'ok' | 'err' | 'warn', text: string }
    loadedCount: 0,
    // id (normalized url without query) -> { maxScore, faceCount, bestBbox, error }
    results: new Map(),
    matchedCount: 0,
    checkedCount: 0,
  };

  function bridge(method, path, body) {
    return new Promise((resolve) => {
      chrome.runtime.sendMessage({ type: "bridge", method, path, body }, (resp) => {
        if (chrome.runtime.lastError) {
          resolve({ ok: false, offline: true, error: chrome.runtime.lastError.message });
          return;
        }
        resolve(resp || { ok: false, offline: true, error: "无响应" });
      });
    });
  }

  function sleep(ms) {
    return new Promise((r) => setTimeout(r, ms));
  }

  // 把群相册缩略图尺寸提到 960px（匹配桌面端 SCRFD 960 输入，远景合照也能检出小脸）
  function upgradeThumbUrl(url) {
    if (!url) return "";
    if (/imageMogr2\/thumbnail\/\d+x/i.test(url)) {
      return url.replace(/imageMogr2\/thumbnail\/\d+x/i, "imageMogr2/thumbnail/960x");
    }
    if (/resize,w_\d+/i.test(url)) {
      return url.replace(/resize,w_\d+/i, "resize,w_960");
    }
    return url;
  }

  function itemIdFromUrl(url) {
    return (url || "").split("?")[0];
  }

  function getImgItemInfo(el) {
    const img =
      el.querySelector('.addAlbum img[src^="http"]') ||
      el.querySelector('img:not(.radio):not(.playIcon)[src^="http"]');
    if (!img || !img.src) return null;
    const rawSrc = img.src;
    const isVideo = Boolean(el.querySelector("img.playIcon")) || /ci-process=snapshot/i.test(rawSrc);
    return {
      el,
      img,
      id: itemIdFromUrl(rawSrc),
      url: upgradeThumbUrl(rawSrc),
      isVideo,
    };
  }

  function collectPageItems() {
    const nodes = document.querySelectorAll(".imgItem");
    const items = [];
    for (const el of nodes) {
      const info = getImgItemInfo(el);
      if (info) items.push(info);
    }
    return items;
  }

  function isBatchMode() {
    return Boolean(document.querySelector(".imgItem img.radio") || document.querySelector(".select-text-upload"));
  }

  async function ensureBatchMode() {
    if (isBatchMode()) {
      captureUncheckedBaseline();
      return true;
    }
    const enterBtn = document.querySelector(".batch-action-enter");
    if (!enterBtn) return false;
    enterBtn.click();
    for (let i = 0; i < 15; i++) {
      await sleep(80);
      if (document.querySelector(".imgItem img.radio")) {
        captureUncheckedBaseline();
        return true;
      }
    }
    return isBatchMode();
  }

  function captureUncheckedBaseline() {
    if (observedUncheckedSrc) return;
    const radios = document.querySelectorAll(".imgItem img.radio");
    for (const r of radios) {
      if (r.src && r.src.includes(UNCHECKED_SIG)) {
        observedUncheckedSrc = r.src;
        return;
      }
    }
    if (radios.length > 0 && radios[0].src && !radios[0].src.includes(CHECKED_SIG)) {
      observedUncheckedSrc = radios[0].src;
    }
  }

  function isRadioChecked(radio) {
    if (!radio || !radio.src) return false;
    if (radio.src.includes(CHECKED_SIG)) return true;
    if (radio.src.includes(UNCHECKED_SIG)) return false;
    if (observedUncheckedSrc) return radio.src !== observedUncheckedSrc;
    return false;
  }

  function setItemChecked(el, wantChecked) {
    const radio = el.querySelector("img.radio");
    if (!radio) return false;
    const cur = isRadioChecked(radio);
    if (cur !== wantChecked) {
      radio.click();
    }
    return true;
  }

  function clearTileDecoration(el) {
    el.classList.remove("kf-matched", "kf-unmatched");
    el.querySelectorAll(".kf-tile-badge, .kf-face-box").forEach((n) => n.remove());
  }

  function decorateTile(el, res, matched) {
    clearTileDecoration(el);
    if (!res) return;
    el.classList.add(matched ? "kf-matched" : "kf-unmatched");

    const badge = document.createElement("div");
    badge.className = "kf-tile-badge" + (matched ? "" : " miss");
    if (res.error) {
      badge.textContent = "失败";
    } else if (res.faceCount === 0) {
      badge.textContent = "无人脸";
    } else {
      badge.textContent = `${matched ? "✓ " : ""}${Math.round(res.maxScore * 100)}%`;
    }
    el.appendChild(badge);

    if (matched && Array.isArray(res.bestBbox) && res.bestBbox.length === 4) {
      const [x1, y1, x2, y2] = res.bestBbox;
      const box = document.createElement("div");
      box.className = "kf-face-box";
      box.style.left = `${Math.max(0, x1 * 100)}%`;
      box.style.top = `${Math.max(0, y1 * 100)}%`;
      box.style.width = `${Math.max(2, (x2 - x1) * 100)}%`;
      box.style.height = `${Math.max(2, (y2 - y1) * 100)}%`;
      el.appendChild(box);
    }
  }

  async function applySelection(autoCheck = true) {
    const items = collectPageItems();
    let matchedCount = 0;
    let checkedCount = 0;
    let overLimit = false;

    if (autoCheck && items.some((it) => {
      const r = state.results.get(it.id);
      return r && !r.error && r.maxScore >= state.threshold;
    })) {
      await ensureBatchMode();
    }

    const batchOn = isBatchMode();

    // 先取消不命中的勾选，避免占满 300 上限导致新的命中勾不上
    if (autoCheck && batchOn) {
      for (const it of items) {
        const res = state.results.get(it.id);
        if (!res) continue;
        const matched = !res.error && res.maxScore >= state.threshold;
        if (!matched) {
          setItemChecked(it.el, false);
        }
      }
    }

    for (const it of items) {
      const res = state.results.get(it.id);
      if (!res) {
        clearTileDecoration(it.el);
        continue;
      }
      const matched = !res.error && res.maxScore >= state.threshold;
      decorateTile(it.el, res, matched);
      if (matched) {
        matchedCount++;
        if (autoCheck && batchOn) {
          if (checkedCount < MAX_BATCH_SELECT) {
            setItemChecked(it.el, true);
            checkedCount++;
          } else {
            setItemChecked(it.el, false);
            overLimit = true;
          }
        }
      }
    }

    state.matchedCount = matchedCount;
    state.checkedCount = checkedCount;
    if (overLimit) {
      state.notice = {
        type: "warn",
        text: `找到 ${matchedCount} 张，因群相册单次批量上限为 ${MAX_BATCH_SELECT} 个，已自动勾选前 ${MAX_BATCH_SELECT} 张。`,
      };
    }
    render();
  }

  function uncheckAllOnPage() {
    if (!isBatchMode()) return;
    for (const el of document.querySelectorAll(".imgItem")) {
      setItemChecked(el, false);
    }
    state.checkedCount = 0;
    state.notice = { type: "ok", text: "已取消页面上全部勾选。" };
    render();
  }

  function findScrollContainer() {
    const first = document.querySelector(".imgItem");
    let cur = first ? first.parentElement : null;
    while (cur && cur !== document.body && cur !== document.documentElement) {
      const cs = getComputedStyle(cur);
      if ((cs.overflowY === "auto" || cs.overflowY === "scroll") && cur.scrollHeight > cur.clientHeight + 20) {
        return cur;
      }
      cur = cur.parentElement;
    }
    return document.scrollingElement || document.documentElement;
  }

  async function scanUnscannedItems() {
    const all = collectPageItems();
    const candidates = all.filter((it) => !(state.skipVideos && it.isVideo));
    const todo = candidates.filter((it) => !state.results.has(it.id));
    state.loadedCount = all.length;

    if (todo.length === 0) {
      await applySelection(true);
      return 0;
    }

    let scannedNow = 0;
    for (let i = 0; i < todo.length; i += BATCH_SIZE) {
      if (state.stopRequested) break;
      const chunk = todo.slice(i, i + BATCH_SIZE);
      state.progressDone = Math.min(candidates.length - todo.length + i, candidates.length);
      state.progressTotal = candidates.length;
      state.progressLabel = `识别中 ${state.progressDone}/${state.progressTotal} 张…`;
      render();

      const resp = await bridge("POST", "/api/match-urls", {
        personId: state.personId,
        items: chunk.map((c) => ({ id: c.id, url: c.url })),
      });
      if (!resp.ok || !resp.data || !resp.data.ok) {
        throw new Error((resp && resp.error) || (resp.data && resp.data.error) || "识别请求失败");
      }
      for (const r of resp.data.results || []) {
        state.results.set(r.id, r);
      }
      scannedNow += chunk.length;
      state.progressDone = Math.min(candidates.length - todo.length + i + chunk.length, candidates.length);
      await applySelection(true);
    }
    return scannedNow;
  }

  async function startScan(loadAll) {
    if (state.scanning) return;
    if (!state.connected) {
      await refreshStatus();
      if (!state.connected) return;
    }
    if (!state.modelsReady) {
      state.notice = { type: "err", text: "桌面端尚未下载人脸模型，请先在「找娃助手 → 设置」下载模型。" };
      render();
      return;
    }
    const person = state.persons.find((p) => p.id === state.personId);
    if (!person || person.refCount === 0) {
      state.showEnroll = true;
      state.notice = { type: "warn", text: "请先为宝宝录入至少 1 张清晰正脸照。" };
      render();
      return;
    }

    const initialItems = collectPageItems();
    if (initialItems.length === 0) {
      state.notice = { type: "warn", text: "当前页面没有检测到照片，请先点进某个群相册/照片集。" };
      render();
      return;
    }

    state.scanning = true;
    state.stopRequested = false;
    state.notice = null;
    render();

    try {
      await ensureBatchMode();
      await scanUnscannedItems();

      if (loadAll && !state.stopRequested) {
        const scroller = findScrollContainer();
        let idleRounds = 0;
        while (!state.stopRequested && idleRounds < 2) {
          const prevCount = collectPageItems().length;
          state.progressLabel = `已加载 ${prevCount} 张，正在下滑加载更多…`;
          render();
          scroller.scrollTop = scroller.scrollHeight;
          // 触发一次 scroll 事件兼容监听
          scroller.dispatchEvent(new Event("scroll", { bubbles: true }));

          let grew = false;
          for (let wait = 0; wait < 14; wait++) {
            if (state.stopRequested) break;
            await sleep(220);
            if (collectPageItems().length > prevCount) {
              grew = true;
              break;
            }
          }
          if (grew) {
            idleRounds = 0;
            await scanUnscannedItems();
          } else {
            idleRounds++;
          }
        }
        // 滚回顶部方便直接点右上/底部批量下载或检查
        scroller.scrollTop = 0;
      }

      await applySelection(true);
      if (!state.notice) {
        const scannedCount = collectPageItems().filter((it) => state.results.has(it.id)).length;
        state.notice = {
          type: state.matchedCount > 0 ? "ok" : "warn",
          text: state.stopRequested
            ? `已停止。已识别 ${scannedCount} 张，命中并勾选 ${state.checkedCount} 张。`
            : state.matchedCount > 0
              ? `识别完成：扫描 ${scannedCount} 张，命中并自动勾选 ${state.checkedCount} 张，直接点页面「批量下载」即可！`
              : `识别完成：扫描 ${scannedCount} 张，当前严格程度下未找到匹配照片，可调低滑块试试。`,
        };
      }
    } catch (err) {
      state.notice = { type: "err", text: `识别出错：${err.message || err}` };
    } finally {
      state.scanning = false;
      state.stopRequested = false;
      state.progressLabel = "";
      render();
    }
  }

  async function refreshStatus() {
    const resp = await bridge("GET", "/api/status");
    if (!resp.ok || !resp.data || !resp.data.ok) {
      state.connected = false;
      state.offlineMsg = "未连接到本机「找娃助手」桌面端，请先打开找娃助手应用。";
      render();
      return;
    }
    const d = resp.data;
    state.connected = true;
    state.offlineMsg = "";
    state.modelsReady = Boolean(d.modelsReady);
    state.persons = Array.isArray(d.persons) ? d.persons : [];
    if (typeof d.threshold === "number" && !Number.isNaN(d.threshold)) {
      state.threshold = d.threshold;
    }
    const hasCurrent = state.persons.some((p) => p.id === state.personId);
    if (!hasCurrent) {
      const withRefs = state.persons.find((p) => p.refCount > 0);
      state.personId = withRefs ? withRefs.id : state.persons[0] ? state.persons[0].id : null;
    }
    const hasAnyReadyKid = state.persons.some((p) => p.refCount > 0);
    // 如果桌面端已经录入了宝宝参考照，完全不需要上传界面
    if (hasAnyReadyKid) {
      state.showEnroll = false;
    }
    render();
  }

  async function handleEnrollUpload(nameInput, fileList) {
    if (!fileList || fileList.length === 0) return;
    const files = Array.from(fileList).slice(0, 8);
    const images = [];
    for (const f of files) {
      const dataUrl = await new Promise((resolve, reject) => {
        const reader = new FileReader();
        reader.onload = () => resolve(String(reader.result || ""));
        reader.onerror = reject;
        reader.readAsDataURL(f);
      });
      images.push(dataUrl);
    }

    const existing = state.persons.find((p) => p.id === state.personId);
    const useExisting = Boolean(existing && !nameInput);
    const payload = {
      personId: useExisting ? existing.id : null,
      name: useExisting ? existing.name : (nameInput || "宝宝").trim(),
      images,
    };
    state.notice = { type: "warn", text: "正在提取宝宝人脸特征…" };
    render();

    const resp = await bridge("POST", "/api/enroll", payload);
    if (!resp.ok || !resp.data || !resp.data.ok) {
      state.notice = { type: "err", text: `录入失败：${(resp && resp.error) || (resp.data && resp.data.error) || "未知错误"}` };
      render();
      return;
    }
    const r = resp.data;
    state.personId = r.personId;
    state.results.clear();
    state.showEnroll = false;
    const parts = [`已录入 ${r.added} 张参考照`];
    if (r.noFace > 0) parts.push(`${r.noFace} 张未检测到人脸`);
    if (r.multiFace > 0) parts.push(`${r.multiFace} 张检测到多张脸（已取最大人脸）`);
    state.notice = { type: r.added > 0 ? "ok" : "warn", text: parts.join("，") };
    await refreshStatus();
  }

  // ---------------- UI ----------------
  const root = document.createElement("div");
  root.id = "kf-root";
  document.body.appendChild(root);

  function render() {
    document.body.classList.toggle("kf-only-matched", state.onlyMatched);

    if (!state.open) {
      const label = state.matchedCount > 0 ? `找娃助手 · 命中 ${state.matchedCount}` : "找娃助手";
      root.innerHTML = `
        <button class="kf-pill" id="kf-open">
          <span class="kf-pill-dot ${state.connected ? "" : "off"}"></span>
          <span>${label}</span>
        </button>
      `;
      root.querySelector("#kf-open").onclick = () => {
        state.open = true;
        refreshStatus();
        render();
      };
      return;
    }

    const person = state.persons.find((p) => p.id === state.personId);
    const hasReadyKid = Boolean(person && person.refCount > 0);
    const statusTag = !state.connected
      ? `<span class="kf-status-tag err">未连接</span>`
      : !state.modelsReady
        ? `<span class="kf-status-tag warn">缺少模型</span>`
        : `<span class="kf-status-tag ok">已连接桌面端</span>`;

    const kidSection = !state.connected
      ? `<div class="kf-msg err">${escapeHtml(state.offlineMsg)}</div>
         <div class="kf-row">
           <button class="kf-btn secondary" id="kf-reconnect">重新连接</button>
         </div>`
      : !state.modelsReady
        ? `<div class="kf-msg err">桌面端尚未下载人脸识别模型，请先打开「找娃助手 → 设置」下载模型。</div>
           <div class="kf-row">
             <button class="kf-btn secondary" id="kf-reconnect">重新检测</button>
           </div>`
        : state.persons.length > 0 && !state.showEnroll
          ? `<div class="kf-kid-row">
               ${
                 person && person.avatar
                   ? `<img class="kf-avatar" src="${person.avatar}" alt="" />`
                   : `<div class="kf-avatar">${escapeHtml((person?.name || "宝").slice(0, 1))}</div>`
               }
               <div class="kf-kid-info">
                 <select class="kf-select" id="kf-person-select">
                   ${state.persons
                     .map(
                       (p) =>
                         `<option value="${p.id}" ${p.id === state.personId ? "selected" : ""}>${escapeHtml(p.name)}（${p.refCount} 张参考照）</option>`,
                     )
                     .join("")}
                 </select>
                 <div class="kf-row" style="justify-content:space-between; margin-top:2px;">
                   <span class="kf-sub">已同步桌面端宝宝，无需再上传</span>
                   <button class="kf-link" id="kf-add-ref-link">+ 补充照片</button>
                 </div>
               </div>
             </div>`
          : `<div class="kf-enroll-box">
               <div style="font-weight:600;color:#9a3412;">${
                 state.persons.length === 0 ? "桌面端尚未录入宝宝，可直接在此上传 1~3 张正脸照" : `为「${escapeHtml(person?.name || "新宝宝")}」补充参考照`
               }</div>
               ${
                 state.persons.length === 0
                   ? `<input class="kf-input" id="kf-new-name" placeholder="宝宝名字（如：小明）" value="宝宝" />`
                   : ""
               }
               <input type="file" id="kf-file-input" accept="image/*" multiple />
               <div class="kf-row">
                 ${state.persons.length > 0 ? `<button class="kf-btn secondary small" id="kf-cancel-enroll">取消</button>` : ""}
               </div>
             </div>`;

    const pct = state.progressTotal > 0 ? Math.round((state.progressDone / state.progressTotal) * 100) : 0;
    const scannedCount = collectPageItems().filter((it) => state.results.has(it.id)).length;

    root.innerHTML = `
      <div class="kf-panel">
        <div class="kf-header">
          <span class="kf-title">找娃助手</span>
          ${statusTag}
          <div class="kf-spacer"></div>
          <button class="kf-icon-btn" id="kf-refresh" title="刷新连接与宝宝列表">↻</button>
          <button class="kf-icon-btn" id="kf-min" title="收起">—</button>
        </div>
        <div class="kf-body">
          ${kidSection}

          ${
            state.connected && state.modelsReady
              ? `
            <div class="kf-row">
              ${
                state.scanning
                  ? `<button class="kf-btn danger" id="kf-stop">停止识别</button>`
                  : `<button class="kf-btn" id="kf-scan-page" ${!hasReadyKid ? "disabled" : ""}>识别当前页并勾选</button>
                     <button class="kf-btn secondary" id="kf-scan-all" ${!hasReadyKid ? "disabled" : ""}>加载全部并识别</button>`
              }
            </div>

            <div class="kf-row" style="justify-content:space-between;">
              <label class="kf-check">
                <input type="checkbox" id="kf-skip-video" ${state.skipVideos ? "checked" : ""} />
                跳过视频（仅照片）
              </label>
              <span class="kf-sub">页面已加载 ${state.loadedCount} 项</span>
            </div>

            ${
              state.scanning
                ? `<div class="kf-progress">
                     <div class="kf-sub">${escapeHtml(state.progressLabel || "正在识别…")}</div>
                     <div class="kf-bar"><div class="kf-bar-fill" style="width:${pct}%"></div></div>
                   </div>`
                : ""
            }

            <div class="kf-slider-box">
              <div class="kf-row" style="justify-content:space-between;">
                <span style="font-size:12px;font-weight:600;">严格程度：${state.threshold.toFixed(2)}</span>
                <span class="kf-sub">${scannedCount > 0 ? `命中 ${state.matchedCount} / 已勾 ${state.checkedCount}` : "拖动可实时增减勾选"}</span>
              </div>
              <input type="range" id="kf-threshold" min="0.25" max="0.65" step="0.01" value="${state.threshold}" />
              <div class="kf-row" style="justify-content:space-between;">
                <span class="kf-sub">宽松（少漏）</span>
                <span class="kf-sub">严格（少错）</span>
              </div>
            </div>

            ${
              scannedCount > 0
                ? `<div class="kf-row">
                     <button class="kf-btn secondary small" id="kf-recheck">重新勾选命中</button>
                     <button class="kf-btn secondary small" id="kf-uncheck">清空勾选</button>
                     <button class="kf-btn secondary small" id="kf-only-matched">${state.onlyMatched ? "显示全部" : "高亮命中"}</button>
                   </div>`
                : ""
            }
          `
              : ""
          }

          ${state.notice ? `<div class="kf-msg ${state.notice.type}">${escapeHtml(state.notice.text)}</div>` : ""}
        </div>
      </div>
    `;

    bindEvents();
  }

  function bindEvents() {
    const $ = (sel) => root.querySelector(sel);
    if ($("#kf-min")) {
      $("#kf-min").onclick = () => {
        state.open = false;
        render();
      };
    }
    if ($("#kf-refresh")) {
      $("#kf-refresh").onclick = () => {
        state.notice = null;
        refreshStatus();
      };
    }
    if ($("#kf-reconnect")) {
      $("#kf-reconnect").onclick = () => refreshStatus();
    }
    if ($("#kf-person-select")) {
      $("#kf-person-select").onchange = (e) => {
        state.personId = Number(e.target.value);
        state.results.clear();
        state.matchedCount = 0;
        state.checkedCount = 0;
        document.querySelectorAll(".imgItem").forEach(clearTileDecoration);
        state.notice = { type: "ok", text: "已切换宝宝，点击开始识别即可快速重新匹配（已缓存特征，极速完成）。" };
        render();
      };
    }
    if ($("#kf-add-ref-link")) {
      $("#kf-add-ref-link").onclick = () => {
        state.showEnroll = true;
        render();
      };
    }
    if ($("#kf-cancel-enroll")) {
      $("#kf-cancel-enroll").onclick = () => {
        state.showEnroll = false;
        render();
      };
    }
    if ($("#kf-file-input")) {
      $("#kf-file-input").onchange = (e) => {
        const nameEl = $("#kf-new-name");
        handleEnrollUpload(nameEl ? nameEl.value : "", e.target.files);
      };
    }
    if ($("#kf-scan-page")) {
      $("#kf-scan-page").onclick = () => startScan(false);
    }
    if ($("#kf-scan-all")) {
      $("#kf-scan-all").onclick = () => startScan(true);
    }
    if ($("#kf-stop")) {
      $("#kf-stop").onclick = () => {
        state.stopRequested = true;
        state.progressLabel = "正在停止…";
        render();
      };
    }
    if ($("#kf-skip-video")) {
      $("#kf-skip-video").onchange = (e) => {
        state.skipVideos = Boolean(e.target.checked);
      };
    }
    if ($("#kf-threshold")) {
      $("#kf-threshold").oninput = (e) => {
        state.threshold = Number(e.target.value);
        applySelection(true);
      };
      $("#kf-threshold").onchange = (e) => {
        const v = Number(e.target.value);
        bridge("POST", "/api/threshold", { value: v });
      };
    }
    if ($("#kf-recheck")) {
      $("#kf-recheck").onclick = () => applySelection(true);
    }
    if ($("#kf-uncheck")) {
      $("#kf-uncheck").onclick = () => uncheckAllOnPage();
    }
    if ($("#kf-only-matched")) {
      $("#kf-only-matched").onclick = () => {
        state.onlyMatched = !state.onlyMatched;
        render();
      };
    }
  }

  function escapeHtml(s) {
    return String(s ?? "")
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;");
  }

  // 监听页面照片数量变化（切换相册 / 手动下滑加载）
  let domTimer = null;
  const observer = new MutationObserver(() => {
    if (domTimer) return;
    domTimer = setTimeout(() => {
      domTimer = null;
      const count = document.querySelectorAll(".imgItem").length;
      if (count !== state.loadedCount) {
        state.loadedCount = count;
        if (count === 0) {
          state.matchedCount = 0;
          state.checkedCount = 0;
        }
        if (!state.scanning) render();
      }
    }, 350);
  });
  observer.observe(document.body, { childList: true, subtree: true });

  state.loadedCount = document.querySelectorAll(".imgItem").length;
  render();
  refreshStatus();
})();
