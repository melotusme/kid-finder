(() => {
  if (window.__KID_FINDER_EXT_LOADED__) return;
  window.__KID_FINDER_EXT_LOADED__ = true;

  // qxc.hi.cn 内置的未选中 / 已选中 radio 图标特征（来自前端 bundle）
  const UNCHECKED_SIG = "AAAEp0lEQVRYCdVZPUgcQRT2";
  const CHECKED_SIG = "AAAEGElEQVRYCc2Zv2/TQBTH";
  const MAX_BATCH_SELECT = 300;
  const BATCH_SIZE = 12;
  // 增量模式下，列表尾部连续遇到多少项「此前已扫过」即认为到达存量边界并停止下滑
  const INCREMENTAL_STOP_STREAK = 12;

  let observedUncheckedSrc = null;

  const state = {
    open: true,
    connected: false,
    modelsReady: false,
    ffmpegReady: false,
    offlineMsg: "",
    persons: [],
    personId: null,
    threshold: 0.4,
    totalDbScanned: 0,
    totalVideoScanned: 0,
    defaultExportDir: "",
    // 多选筛选类型：可单独选照片、单独选视频、或照片+视频一起选
    selectPhotos: true,
    selectVideos: true,
    autoClipVideos: true,
    clipLead: 2,
    clipTail: 2,
    clipFade: 0.5,
    includeCached: false, // false = 仅勾选/裁剪本次新增命中；true = 同时包含已扫存量命中
    onlyMatched: false,
    showEnroll: false,
    scanning: false,
    clipping: false,
    stopRequested: false,
    progressDone: 0,
    progressTotal: 0,
    progressLabel: "",
    notice: null, // { type: 'ok' | 'err' | 'warn', text: string }
    lastExportDir: "",
    loadedCount: 0,
    photoTotal: 0,
    photoUnscanned: 0,
    videoTotal: 0,
    videoUnscanned: 0,
    // id (normalized url without query) -> { kind, maxScore, faceCount, bestBbox, mediaId, duration, segments, error, cached }
    results: new Map(),
    // 本次会话中新扫描的 ID（区分「今天新增」与「以前扫过的存量」）
    sessionNewIds: new Set(),
    matchedPhotoCount: 0,
    matchedVideoCount: 0,
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

  function fmtDur(sec) {
    const s = Math.max(0, Math.round(sec || 0));
    return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
  }

  // 把群相册照片缩略图尺寸提到 960px（匹配桌面端 SCRFD 960 输入，远景合照也能检出小脸）
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
    const rawNoQuery = itemIdFromUrl(rawSrc);
    const isVideo =
      Boolean(el.querySelector("img.playIcon")) ||
      /(ci-process=snapshot|video\/snapshot)/i.test(rawSrc) ||
      /\.(mp4|mov|m4v|avi|mkv|webm)$/i.test(rawNoQuery);
    return {
      el,
      img,
      id: rawNoQuery,
      // 照片用 960px 缩略图；视频用去参后的原 MP4 地址交给桌面端下载并 1fps 全帧识别
      url: isVideo ? rawNoQuery : upgradeThumbUrl(rawSrc),
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

  function getSelectedCandidates() {
    const all = collectPageItems();
    state.loadedCount = all.length;
    return all.filter((it) => (it.isVideo ? state.selectVideos : state.selectPhotos));
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
    el.classList.remove("kf-matched", "kf-matched-old", "kf-unmatched");
    el.querySelectorAll(".kf-tile-badge, .kf-face-box").forEach((n) => n.remove());
  }

  function decorateTile(el, res, matched, isNew, isVideo) {
    clearTileDecoration(el);
    if (!res) return;

    if (matched) {
      el.classList.add(isNew || state.includeCached ? "kf-matched" : "kf-matched-old");
    } else {
      el.classList.add("kf-unmatched");
    }

    const badge = document.createElement("div");
    if (res.error) {
      badge.className = "kf-tile-badge miss";
      badge.textContent = "失败";
    } else if (matched) {
      badge.className = "kf-tile-badge" + (isNew ? "" : " old-hit");
      if (isVideo) {
        const segs = Array.isArray(res.segments) ? res.segments : [];
        const totalSec = segs.reduce((acc, [a, b]) => acc + Math.max(0, b - a), 0);
        const segInfo = segs.length > 0 ? `▶ ${segs.length}段 ${fmtDur(totalSec)} · ` : "▶ ";
        badge.textContent = `${isNew ? "新 " : "已扫 "}${segInfo}${Math.round(res.maxScore * 100)}%`;
      } else {
        badge.textContent = `${isNew ? "新 ✓ " : "已扫 ✓ "}${Math.round(res.maxScore * 100)}%`;
      }
    } else if (!isNew) {
      badge.className = "kf-tile-badge miss";
      badge.textContent = isVideo ? "视频已扫" : "已扫";
    } else if (res.faceCount === 0) {
      badge.className = "kf-tile-badge miss";
      badge.textContent = isVideo ? "未检出人脸" : "无人脸";
    } else {
      badge.className = "kf-tile-badge miss";
      badge.textContent = `${isVideo ? "未出现 " : ""}${Math.round(res.maxScore * 100)}%`;
    }
    el.appendChild(badge);

    if (!isVideo && matched && Array.isArray(res.bestBbox) && res.bestBbox.length === 4) {
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

  function updatePageCounts() {
    const all = collectPageItems();
    state.loadedCount = all.length;
    let pTot = 0, pUn = 0, vTot = 0, vUn = 0;
    for (const it of all) {
      const scanned = state.results.has(it.id);
      if (it.isVideo) {
        vTot++;
        if (!scanned) vUn++;
      } else {
        pTot++;
        if (!scanned) pUn++;
      }
    }
    state.photoTotal = pTot;
    state.photoUnscanned = pUn;
    state.videoTotal = vTot;
    state.videoUnscanned = vUn;
  }

  /**
   * 仅查本地 SQLite 缓存（0 网络下载、0 模型推理），同时查询已扫过的照片(web_scans)与视频(media)。
   */
  async function syncPageScannedState(autoDecorate = true) {
    if (!state.connected) return;
    const all = collectPageItems();
    const unknownIds = [];
    const seen = new Set();
    for (const it of all) {
      if (!state.results.has(it.id) && !seen.has(it.id)) {
        seen.add(it.id);
        unknownIds.push(it.id);
      }
    }
    if (unknownIds.length > 0) {
      const resp = await bridge("POST", "/api/check-scanned", {
        personId: state.personId,
        threshold: state.threshold,
        ids: unknownIds,
      });
      if (resp.ok && resp.data && resp.data.ok) {
        if (typeof resp.data.scannedCount === "number") state.totalDbScanned = resp.data.scannedCount;
        if (typeof resp.data.videoScannedCount === "number") state.totalVideoScanned = resp.data.videoScannedCount;
        for (const r of resp.data.results || []) {
          if (r.scanned) {
            state.results.set(r.id, {
              id: r.id,
              kind: r.kind,
              cached: true,
              maxScore: r.maxScore,
              faceCount: r.faceCount,
              bestBbox: r.bestBbox,
              mediaId: r.mediaId,
              duration: r.duration,
              segments: r.segments || [],
              error: null,
            });
          }
        }
      }
    }
    updatePageCounts();
    if (autoDecorate) {
      await applySelection(false);
    } else {
      render();
    }
  }

  function isTypeEnabled(it) {
    return it.isVideo ? state.selectVideos : state.selectPhotos;
  }

  function shouldSelectItem(it, res, photosOnly = false) {
    if (!isTypeEnabled(it)) return false;
    if (photosOnly && it.isVideo) return false;
    if (!res || res.error || res.maxScore < state.threshold) return false;
    const isNew = state.sessionNewIds.has(it.id);
    return isNew || state.includeCached;
  }

  function getMatchedVideoMediaIds(onlyNew = false) {
    const ids = [];
    const seen = new Set();
    for (const it of collectPageItems()) {
      if (!it.isVideo || !state.selectVideos) continue;
      const res = state.results.get(it.id);
      if (!res || res.error || res.maxScore < state.threshold || !res.mediaId) continue;
      if (onlyNew && !state.sessionNewIds.has(it.id)) continue;
      if (!onlyNew && !state.includeCached && !state.sessionNewIds.has(it.id)) continue;
      if (!seen.has(res.mediaId)) {
        seen.add(res.mediaId);
        ids.push(res.mediaId);
      }
    }
    return ids;
  }

  async function applySelection(autoCheck = true, photosOnly = false) {
    const items = collectPageItems();
    let matchedPhotoCount = 0;
    let matchedVideoCount = 0;
    let checkedCount = 0;
    let overLimit = false;

    const hasAnyToCheck =
      autoCheck && items.some((it) => shouldSelectItem(it, state.results.get(it.id), photosOnly));
    if (hasAnyToCheck) {
      await ensureBatchMode();
    }

    const batchOn = isBatchMode();

    // 先取消不需要勾选的项，避免占满 300 上限导致新的命中勾不上
    if (autoCheck && batchOn) {
      for (const it of items) {
        const res = state.results.get(it.id);
        if (!res && !photosOnly) continue;
        if (!shouldSelectItem(it, res, photosOnly)) {
          setItemChecked(it.el, false);
        }
      }
    }

    for (const it of items) {
      const res = state.results.get(it.id);
      if (!res || !isTypeEnabled(it)) {
        clearTileDecoration(it.el);
        continue;
      }
      const matched = !res.error && res.maxScore >= state.threshold;
      const isNew = state.sessionNewIds.has(it.id);
      decorateTile(it.el, res, matched, isNew, it.isVideo);

      if (matched) {
        if (it.isVideo) matchedVideoCount++;
        else matchedPhotoCount++;
        if (autoCheck && batchOn && shouldSelectItem(it, res, photosOnly)) {
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

    updatePageCounts();
    state.matchedPhotoCount = matchedPhotoCount;
    state.matchedVideoCount = matchedVideoCount;
    state.checkedCount = checkedCount;
    if (overLimit) {
      state.notice = {
        type: "warn",
        text: `命中数超过群相册单次批量上限（${MAX_BATCH_SELECT} 个），已自动勾选前 ${MAX_BATCH_SELECT} 项。`,
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

  function countTrailingOldScanned(candidates) {
    let streak = 0;
    for (let i = candidates.length - 1; i >= 0; i--) {
      const id = candidates[i].id;
      const res = state.results.get(id);
      if (res && !res.error && !state.sessionNewIds.has(id)) {
        streak++;
      } else {
        break;
      }
    }
    return streak;
  }

  async function scanUnscannedOnPage() {
    await syncPageScannedState(false);
    const candidates = getSelectedCandidates();
    const todoPhotos = candidates.filter((it) => !it.isVideo && !state.results.has(it.id));
    const todoVideos = candidates.filter((it) => it.isVideo && !state.results.has(it.id));

    if (todoPhotos.length === 0 && todoVideos.length === 0) {
      await applySelection(true);
      return { photos: 0, videos: 0 };
    }

    let photosDone = 0;
    let videosDone = 0;
    const totalTodo = todoPhotos.length + todoVideos.length;

    // 1. 批量扫描新照片（缩略图极速识别）
    for (let i = 0; i < todoPhotos.length; i += BATCH_SIZE) {
      if (state.stopRequested) break;
      const chunk = todoPhotos.slice(i, i + BATCH_SIZE);
      state.progressDone = photosDone;
      state.progressTotal = totalTodo;
      state.progressLabel = `识别新照片 ${photosDone}/${todoPhotos.length}…`;
      render();

      const resp = await bridge("POST", "/api/match-urls", {
        personId: state.personId,
        items: chunk.map((c) => ({ id: c.id, url: c.url })),
      });
      if (!resp.ok || !resp.data || !resp.data.ok) {
        throw new Error((resp && resp.error) || (resp.data && resp.data.error) || "照片识别请求失败");
      }
      if (typeof resp.data.scannedCount === "number") {
        state.totalDbScanned = resp.data.scannedCount;
      }
      for (const r of resp.data.results || []) {
        state.results.set(r.id, r);
        if (!r.cached && !r.error) {
          state.sessionNewIds.add(r.id);
        }
      }
      photosDone += chunk.length;
      state.progressDone = photosDone;
      await applySelection(true);
    }

    // 2. 逐个下载并 1fps 全帧分析新视频
    for (let i = 0; i < todoVideos.length; i++) {
      if (state.stopRequested) break;
      const v = todoVideos[i];
      state.progressDone = photosDone + videosDone;
      state.progressTotal = totalTodo;
      state.progressLabel = `下载并逐秒识别视频 ${i + 1}/${todoVideos.length}（首次稍慢，识别后会永久记住）…`;
      render();

      const resp = await bridge("POST", "/api/analyze-video", {
        personId: state.personId,
        id: v.id,
        url: v.url,
        threshold: state.threshold,
      });
      if (!resp.ok || !resp.data || !resp.data.ok) {
        const errMsg = (resp && resp.error) || (resp.data && resp.data.error) || "视频分析失败";
        // 如果是缺少 ffmpeg 这类全局配置错误，直接抛出提示用户安装
        if (errMsg.includes("ffmpeg") || errMsg.includes("模型")) {
          throw new Error(errMsg);
        }
        state.results.set(v.id, { id: v.id, kind: "video", maxScore: 0, faceCount: 0, segments: [], error: errMsg });
      } else {
        const d = resp.data;
        if (typeof d.videoScannedCount === "number") {
          state.totalVideoScanned = d.videoScannedCount;
        }
        state.results.set(v.id, {
          id: v.id,
          kind: "video",
          cached: Boolean(d.cached),
          mediaId: d.mediaId,
          maxScore: d.maxScore || 0,
          faceCount: d.faceCount || 0,
          duration: d.duration,
          segments: d.segments || [],
          error: null,
        });
        if (!d.cached) {
          state.sessionNewIds.add(v.id);
        }
      }
      videosDone++;
      state.progressDone = photosDone + videosDone;
      await applySelection(true);
    }

    return { photos: photosDone, videos: videosDone };
  }

  async function exportMatchedVideos(mediaIds, openFolder = true) {
    if (!mediaIds || mediaIds.length === 0) {
      state.notice = { type: "warn", text: "当前没有可裁剪的命中视频。" };
      render();
      return null;
    }
    state.clipping = true;
    state.progressLabel = `正在用 ffmpeg 自动裁剪 ${mediaIds.length} 个命中视频的宝宝片段…`;
    render();
    try {
      const resp = await bridge("POST", "/api/export-clips", {
        personId: state.personId,
        threshold: state.threshold,
        mediaIds,
        lead: state.clipLead,
        tail: state.clipTail,
        fade: state.clipFade,
        openFolder,
      });
      if (!resp.ok || !resp.data || !resp.data.ok) {
        throw new Error((resp && resp.error) || (resp.data && resp.data.error) || "裁剪失败");
      }
      const d = resp.data;
      state.lastExportDir = d.outDir || "";
      const errs = Array.isArray(d.errors) && d.errors.length ? `，${d.errors.length} 个失败` : "";
      const skip = d.skipped > 0 ? `（${d.skipped} 段已存在跳过）` : "";
      state.notice = {
        type: errs ? "warn" : "ok",
        text: `视频自动裁剪完成：新生成 ${d.clips} 段宝宝视频${skip}${errs}，保存在 ${d.outDir}`,
      };
      return d;
    } catch (err) {
      state.notice = { type: "err", text: `视频裁剪出错：${err.message || err}` };
      return null;
    } finally {
      state.clipping = false;
      state.progressLabel = "";
      render();
    }
  }

  async function startScan(mode) {
    if (state.scanning || state.clipping) return;
    if (!state.selectPhotos && !state.selectVideos) {
      state.notice = { type: "warn", text: "请至少勾选「照片」或「视频」中的一项。" };
      render();
      return;
    }
    if (!state.connected) {
      await refreshStatus();
      if (!state.connected) return;
    }
    if (!state.modelsReady) {
      state.notice = { type: "err", text: "桌面端尚未下载人脸模型，请先在「找娃助手 → 设置」下载模型。" };
      render();
      return;
    }
    if (state.selectVideos && !state.ffmpegReady) {
      state.notice = { type: "err", text: "已勾选「视频」，但桌面端未安装 ffmpeg。请先在「找娃助手 → 设置」点击自动安装，或先取消勾选「视频」。" };
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
      state.notice = { type: "warn", text: "当前页面没有检测到照片/视频，请先点进某个群相册或照片集。" };
      render();
      return;
    }

    if (mode === "full") {
      state.includeCached = true;
    }

    state.scanning = true;
    state.stopRequested = false;
    state.notice = null;
    let newPhotos = 0;
    let newVideos = 0;
    let hitIncrementalBoundary = false;
    render();

    try {
      await ensureBatchMode();
      const first = await scanUnscannedOnPage();
      newPhotos += first.photos;
      newVideos += first.videos;

      const scroller = findScrollContainer();
      let idleRounds = 0;

      while (!state.stopRequested && idleRounds < 2) {
        if (mode === "incremental") {
          const cands = getSelectedCandidates();
          if (countTrailingOldScanned(cands) >= INCREMENTAL_STOP_STREAK) {
            hitIncrementalBoundary = true;
            break;
          }
        }

        const prevCount = collectPageItems().length;
        state.progressLabel =
          mode === "incremental"
            ? `已加载 ${prevCount} 项，检查是否还有新增…`
            : `已加载 ${prevCount} 项，下滑补扫存量（已扫过自动跳过）…`;
        render();

        scroller.scrollTop = scroller.scrollHeight;
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
          const more = await scanUnscannedOnPage();
          newPhotos += more.photos;
          newVideos += more.videos;
        } else {
          idleRounds++;
        }
      }

      if (scroller && !state.stopRequested) {
        scroller.scrollTop = 0;
      }

      await applySelection(true);

      // 如果勾选了「视频」且开启了「扫完自动裁剪宝宝片段」，自动裁剪命中的视频！
      const videoIdsToClip =
        state.selectVideos && state.autoClipVideos && !state.stopRequested
          ? getMatchedVideoMediaIds(mode === "incremental" && !state.includeCached)
          : [];

      let clipReport = null;
      if (videoIdsToClip.length > 0) {
        state.scanning = false;
        clipReport = await exportMatchedVideos(videoIdsToClip, true);
      }

      if (!state.notice) {
        const scannedParts = [];
        if (newPhotos > 0) scannedParts.push(`${newPhotos} 张新照片`);
        if (newVideos > 0) scannedParts.push(`${newVideos} 个新视频`);
        const scannedStr = scannedParts.length ? `新扫 ${scannedParts.join("、")}` : "没有未扫的新内容";
        const boundaryTip = hitIncrementalBoundary ? "（已到达存量边界自动停止）" : "";
        if (state.stopRequested) {
          state.notice = { type: "warn", text: `已停止。本次${scannedStr}（已永久缓存），页面已勾选 ${state.checkedCount} 项。` };
        } else if (newPhotos === 0 && newVideos === 0 && mode === "incremental") {
          state.notice = {
            type: "ok",
            text: `没有发现新增内容，本页此前均已扫过。如需重新勾选/裁剪历史存量，可勾选「同时勾选已扫存量」或点「补扫全部存量」。`,
          };
        } else {
          state.notice = {
            type: state.checkedCount > 0 || clipReport ? "ok" : "warn",
            text: `${mode === "incremental" ? "增量" : "全量"}完成：${scannedStr}${boundaryTip}，命中照片 ${state.matchedPhotoCount} 张、视频 ${state.matchedVideoCount} 个，页面已勾选 ${state.checkedCount} 项。`,
          };
        }
      }
    } catch (err) {
      state.notice = { type: "err", text: `处理出错：${err.message || err}` };
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
    state.ffmpegReady = Boolean(d.ffmpegReady);
    state.defaultExportDir = d.defaultExportDir || "";
    state.persons = Array.isArray(d.persons) ? d.persons : [];
    if (typeof d.scannedCount === "number") state.totalDbScanned = d.scannedCount;
    if (typeof d.videoScannedCount === "number") state.totalVideoScanned = d.videoScannedCount;
    if (typeof d.threshold === "number" && !Number.isNaN(d.threshold)) {
      state.threshold = d.threshold;
    }
    const hasCurrent = state.persons.some((p) => p.id === state.personId);
    if (!hasCurrent) {
      const withRefs = state.persons.find((p) => p.refCount > 0);
      state.personId = withRefs ? withRefs.id : state.persons[0] ? state.persons[0].id : null;
    }
    if (state.persons.some((p) => p.refCount > 0)) {
      state.showEnroll = false;
    }
    await syncPageScannedState(true);
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
    parts.push("已用本机缓存重新打分，无需重新下载");
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
      const unscanned = (state.selectPhotos ? state.photoUnscanned : 0) + (state.selectVideos ? state.videoUnscanned : 0);
      const label =
        state.checkedCount > 0
          ? `找娃助手 · 已勾 ${state.checkedCount}`
          : unscanned > 0
            ? `找娃助手 · ${unscanned} 项未扫`
            : "找娃助手";
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
                 state.persons.length === 0 ? "桌面端尚未录入宝宝，可直接在此上传 1~5 张正脸照" : `为「${escapeHtml(person?.name || "新宝宝")}」补充参考照`
               }</div>
               <div class="kf-sub">1 张清晰正脸即可用，放 3~10 张不同角度/光线识别更稳</div>
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
    const busy = state.scanning || state.clipping;
    const hasScannedOnPage = state.results.size > 0;
    const matchedVideoIds = getMatchedVideoMediaIds(false);

    root.innerHTML = `
      <div class="kf-panel">
        <div class="kf-header">
          <span class="kf-title">找娃助手</span>
          ${statusTag}
          <div class="kf-spacer"></div>
          <button class="kf-icon-btn" id="kf-refresh" title="刷新连接与增量状态">↻</button>
          <button class="kf-icon-btn" id="kf-min" title="收起">—</button>
        </div>
        <div class="kf-body">
          ${kidSection}

          ${
            state.connected && state.modelsReady
              ? `
            <div class="kf-type-box">
              <div class="kf-row" style="justify-content:space-between;">
                <span style="font-size:12px;font-weight:600;">筛选类型（可多选）</span>
                <span class="kf-sub" title="本机累计记住的照片/视频数，永不重复扫">缓存 照${state.totalDbScanned}/视${state.totalVideoScanned}</span>
              </div>
              <div class="kf-row" style="margin-top:4px; gap:14px;">
                <label class="kf-check">
                  <input type="checkbox" id="kf-type-photo" ${state.selectPhotos ? "checked" : ""} ${busy ? "disabled" : ""} />
                  <span>照片 <b>${state.photoTotal}</b>${state.photoUnscanned > 0 ? ` <span class="kf-pill-mini new">新${state.photoUnscanned}</span>` : ""}</span>
                </label>
                <label class="kf-check">
                  <input type="checkbox" id="kf-type-video" ${state.selectVideos ? "checked" : ""} ${busy ? "disabled" : ""} />
                  <span>视频 <b>${state.videoTotal}</b>${state.videoUnscanned > 0 ? ` <span class="kf-pill-mini new">新${state.videoUnscanned}</span>` : ""}</span>
                </label>
              </div>
              ${
                state.selectVideos
                  ? `<div class="kf-video-opts">
                       <label class="kf-check">
                         <input type="checkbox" id="kf-auto-clip" ${state.autoClipVideos ? "checked" : ""} ${busy ? "disabled" : ""} />
                         <span>下载识别后自动裁剪宝宝片段</span>
                       </label>
                       <div class="kf-sub" style="margin-top:2px;">
                         ${
                           state.ffmpegReady
                             ? `提前 ${state.clipLead}s / 延后 ${state.clipTail}s · 淡入淡出 ${state.clipFade}s → 下载目录/找娃助手导出`
                             : `<span style="color:#b91c1c;">⚠ 桌面端未安装 ffmpeg，请先在桌面端「设置」自动安装</span>`
                         }
                       </div>
                     </div>`
                  : ""
              }
            </div>

            <div class="kf-row">
              ${
                state.scanning
                  ? `<button class="kf-btn danger" id="kf-stop">停止扫描</button>`
                  : `<button class="kf-btn" id="kf-scan-inc" ${!hasReadyKid || busy || (!state.selectPhotos && !state.selectVideos) ? "disabled" : ""} title="只扫新增照片/视频，遇到已扫存量自动停止">扫新增并勾选</button>
                     <button class="kf-btn secondary" id="kf-scan-full" ${!hasReadyKid || busy || (!state.selectPhotos && !state.selectVideos) ? "disabled" : ""} title="首次处理整本相册或补全存量，已扫过的自动跳过">补扫全部存量</button>`
              }
            </div>

            <div class="kf-row" style="justify-content:space-between;">
              <label class="kf-check" title="开启后会把之前扫过的存量命中照片/视频也一起勾选和裁剪（0 下载，直接读本机缓存）">
                <input type="checkbox" id="kf-include-cached" ${state.includeCached ? "checked" : ""} ${busy ? "disabled" : ""} />
                同时包含已扫存量
              </label>
              ${
                state.lastExportDir || state.totalVideoScanned > 0
                  ? `<button class="kf-link" id="kf-open-export">打开视频裁剪目录</button>`
                  : ""
              }
            </div>

            ${
              busy
                ? `<div class="kf-progress">
                     <div class="kf-sub">${escapeHtml(state.progressLabel || "处理中…")}</div>
                     <div class="kf-bar"><div class="kf-bar-fill" style="width:${state.clipping ? 100 : pct}%"></div></div>
                   </div>`
                : ""
            }

            <div class="kf-slider-box">
              <div class="kf-row" style="justify-content:space-between;">
                <span style="font-size:12px;font-weight:600;">严格程度：${state.threshold.toFixed(2)}</span>
                <span class="kf-sub">${
                  hasScannedOnPage
                    ? `照${state.matchedPhotoCount} · 视${state.matchedVideoCount} · 已勾${state.checkedCount}`
                    : "拖动可实时增减勾选"
                }</span>
              </div>
              <input type="range" id="kf-threshold" min="0.25" max="0.65" step="0.01" value="${state.threshold}" ${busy ? "disabled" : ""} />
              <div class="kf-row" style="justify-content:space-between;">
                <span class="kf-sub">宽松（少漏）</span>
                <span class="kf-sub">严格（少错）</span>
              </div>
            </div>

            ${
              hasScannedOnPage
                ? `<div class="kf-row">
                     ${
                       matchedVideoIds.length > 0
                         ? `<button class="kf-btn small" id="kf-clip-now" ${busy ? "disabled" : ""}>裁剪命中视频 (${matchedVideoIds.length})</button>`
                         : ""
                     }
                     ${
                       state.selectPhotos && state.selectVideos && state.matchedVideoCount > 0
                         ? `<button class="kf-btn secondary small" id="kf-check-photos-only" ${busy ? "disabled" : ""} title="视频已本地自动裁剪时，只勾选网页里的照片方便批量下载">只勾照片</button>`
                         : `<button class="kf-btn secondary small" id="kf-recheck" ${busy ? "disabled" : ""}>重新勾选</button>`
                     }
                     <button class="kf-btn secondary small" id="kf-uncheck" ${busy ? "disabled" : ""}>清空</button>
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
      $("#kf-person-select").onchange = async (e) => {
        state.personId = Number(e.target.value);
        state.results.clear();
        state.matchedPhotoCount = 0;
        state.matchedVideoCount = 0;
        state.checkedCount = 0;
        document.querySelectorAll(".imgItem").forEach(clearTileDecoration);
        await syncPageScannedState(true);
        state.notice = { type: "ok", text: "已切换宝宝，已扫照片与视频已直接从本机缓存重算得分（无需重扫）。" };
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
    if ($("#kf-type-photo")) {
      $("#kf-type-photo").onchange = (e) => {
        state.selectPhotos = Boolean(e.target.checked);
        applySelection(true);
      };
    }
    if ($("#kf-type-video")) {
      $("#kf-type-video").onchange = (e) => {
        state.selectVideos = Boolean(e.target.checked);
        applySelection(true);
      };
    }
    if ($("#kf-auto-clip")) {
      $("#kf-auto-clip").onchange = (e) => {
        state.autoClipVideos = Boolean(e.target.checked);
      };
    }
    if ($("#kf-scan-inc")) {
      $("#kf-scan-inc").onclick = () => startScan("incremental");
    }
    if ($("#kf-scan-full")) {
      $("#kf-scan-full").onclick = () => startScan("full");
    }
    if ($("#kf-stop")) {
      $("#kf-stop").onclick = () => {
        state.stopRequested = true;
        state.progressLabel = "正在停止…";
        render();
      };
    }
    if ($("#kf-include-cached")) {
      $("#kf-include-cached").onchange = (e) => {
        state.includeCached = Boolean(e.target.checked);
        applySelection(true);
      };
    }
    if ($("#kf-open-export")) {
      $("#kf-open-export").onclick = () => {
        bridge("POST", "/api/open-export-dir", { personId: state.personId });
      };
    }
    if ($("#kf-clip-now")) {
      $("#kf-clip-now").onclick = () => {
        const ids = getMatchedVideoMediaIds(false);
        exportMatchedVideos(ids, true);
      };
    }
    if ($("#kf-check-photos-only")) {
      $("#kf-check-photos-only").onclick = async () => {
        await applySelection(true, true);
        state.notice = { type: "ok", text: `已仅勾选 ${state.checkedCount} 张命中照片（已取消视频勾选，避免重复下载未裁剪大视频）。` };
        render();
      };
    }
    if ($("#kf-threshold")) {
      $("#kf-threshold").oninput = (e) => {
        state.threshold = Number(e.target.value);
        applySelection(true);
      };
      $("#kf-threshold").onchange = async (e) => {
        const v = Number(e.target.value);
        bridge("POST", "/api/threshold", { value: v });
        // 阈值变化后重新向桌面端刷新视频的片段信息（0 推理，秒返回）
        if (state.selectVideos && state.videoTotal > 0) {
          for (const [k, r] of state.results.entries()) {
            if (r.kind === "video") state.results.delete(k);
          }
          await syncPageScannedState(true);
        }
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

  let domTimer = null;
  const observer = new MutationObserver(() => {
    if (domTimer) return;
    domTimer = setTimeout(() => {
      domTimer = null;
      const count = document.querySelectorAll(".imgItem").length;
      if (count !== state.loadedCount) {
        state.loadedCount = count;
        if (count === 0) {
          state.matchedPhotoCount = 0;
          state.matchedVideoCount = 0;
          state.checkedCount = 0;
          state.photoTotal = 0;
          state.photoUnscanned = 0;
          state.videoTotal = 0;
          state.videoUnscanned = 0;
          if (!state.scanning) render();
        } else if (!state.scanning) {
          syncPageScannedState(true);
        }
      }
    }, 400);
  });
  observer.observe(document.body, { childList: true, subtree: true });

  state.loadedCount = document.querySelectorAll(".imgItem").length;
  render();
  refreshStatus();
})();
