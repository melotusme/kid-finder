import { useState, type CSSProperties } from "react";

const kbdStyle: CSSProperties = {
  display: "inline-block",
  padding: "1px 6px",
  fontSize: 12,
  lineHeight: 1.4,
  fontFamily: "ui-monospace, SFMono-Regular, Consolas, monospace",
  color: "#444",
  background: "#f3f4f6",
  border: "1px solid #d1d5db",
  borderRadius: 4,
};

const stepGridStyle: CSSProperties = {
  display: "grid",
  gridTemplateColumns: "repeat(auto-fit, minmax(240px, 1fr))",
  gap: 12,
  margin: "12px 0",
};

const stepCardStyle: CSSProperties = {
  margin: 0,
  display: "flex",
  flexDirection: "column",
  gap: 6,
};

const subBoxStyle: CSSProperties = {
  background: "#f8fafc",
  border: "1px solid #e2e8f0",
  borderRadius: 8,
  padding: "10px 12px",
  fontSize: 13,
  lineHeight: 1.6,
};

const warnBoxStyle: CSSProperties = {
  background: "#fffbeb",
  border: "1px solid #fde68a",
  color: "#92400e",
  borderRadius: 8,
  padding: "8px 12px",
  fontSize: 12.5,
  lineHeight: 1.6,
};

/**
 * Compact collapsible helper card for importing photos/videos safely from WeChat PC 4.x.
 * Verified empirically against real Weixin 4.2 UI & drag/export behavior.
 */
export function WechatImportGuide({ defaultOpen = false }: { defaultOpen?: boolean } = {}) {
  const [open, setOpen] = useState(defaultOpen);

  return (
    <div className="card ext-card">
      <div
        className="row"
        style={{ cursor: "pointer", userSelect: "none" }}
        onClick={() => setOpen((v) => !v)}
      >
        <h3>💡 电脑微信（4.x）班级群怎么批量导？（实测可行 · 100% 零封号）</h3>
        {!open && (
          <span className="muted small">
            聊天记录 → 图片与视频 → 右键「多选」拉框（≤100 项）→ 点底部第 5 个「保存」图标到中转夹
          </span>
        )}
        <div className="spacer" />
        <button
          type="button"
          className="ghost small"
          style={{ padding: "3px 10px" }}
          onClick={(e) => {
            e.stopPropagation();
            setOpen((v) => !v);
          }}
        >
          {open ? "收起步骤 ▴" : "查看实测步骤 ▾"}
        </button>
      </div>

      {open && (
        <div style={{ marginTop: 10, display: "grid", gap: 10 }}>
          <div className="ext-status-list">
            <div>
              <b className="ok">✓ 零封号说明：</b>
              新版微信 4.x 群图片底层是动态加密 <code>.dat</code>，未点开的原视频在本地也只有封面图；强行读微信内存解密有<b>封号风险</b>，本软件<b>绝不触碰微信进程</b>。
            </div>
            <div>
              <b className="warn">⚠ 避坑提醒（微信 4.x 实测限制）：</b>
              不要直接从微信窗口往本软件里拖（微信 4.x 拖拽采用虚拟流协议，应用窗口无法直接接收）；在「图片与视频」里{" "}
              <code>Ctrl+A</code> 无效，多选后<b>右键菜单里也没有另存</b>，必须点<b>窗口最底部的「保存」图标</b>！
            </div>
          </div>

          <ol className="ext-steps">
            <li>
              <b>准备中转夹：</b>建一个空文件夹（如 <code>微信导出中转</code>），建议绑定为本页的<b>「微信中转监听文件夹」</b>（保存完自动后台入库；也可保存后点上方「选择文件夹」导入）。
            </li>
            <li>
              <b>进入平铺多选：</b>在电脑微信打开班级群 → 点右上角 <kbd style={kbdStyle}>...</kbd> → 点 <b>「聊天记录」</b> → 点上方{" "}
              <b>「图片与视频」</b> 标签（折叠多图会自动平铺展开）→ 在任意缩略图上<b>点鼠标右键 → 选「多选」</b>。
            </li>
            <li>
              <b>拉框勾选并点底部「保存」：</b>在网格中<b>按住鼠标左键拖动拉框勾选</b>（微信单批上限 <b>100 项</b>，几百张分 2～3 批即可）→ 点击窗口<b>最底部工具栏第 5 个「保存」图标</b>（收藏右边、删除桶左边）→ 选第 1 步的中转文件夹，等微信自动拉取未下载的大图与原视频并保存完毕。
            </li>
            <li>
              <b>阅后即清省空间：</b>识别并导出自家娃的照片与视频片段后，<b>中转夹可直接清空</b>（找娃助手已记住文件 <code>SHA-256</code> 指纹，下次不小心框选重了也会秒级跳过）；微信自身缓存的视频可在微信左下角{" "}
              <kbd style={kbdStyle}>≡</kbd> → <b>设置 → 存储空间 → 聊天数据「管理」</b> 一键清理。
            </li>
          </ol>
        </div>
      )}
    </div>
  );
}

/**
 * Full in-app user guide ("使用说明") tab component.
 */
export default function Guide({ onNavigate }: { onNavigate?: (tab: string) => void } = {}) {
  return (
    <section>
      <h2>使用说明</h2>
      <p className="hint">
        找娃助手帮你从班级群成百上千张照片和长视频里自动挑出自家宝宝与署名作品。所有人脸与文字识别均在本机离线完成，照片绝不上传云端。
      </p>

      <div className="banner">
        <span className="ok">
          <b>首次使用提示：</b>若尚未下载离线人脸与作品识字模型（处理视频还需安装 ffmpeg），请先前往「设置」一键准备就绪。
        </span>
        {onNavigate && (
          <>
            <div className="spacer" />
            <button type="button" className="ghost small" onClick={() => onNavigate("settings")}>
              前往「设置」
            </button>
          </>
        )}
      </div>

      {/* 1. 快速开始流程 */}
      <div className="card">
        <div className="row">
          <h3>一、快速开始（四步找齐自家娃）</h3>
        </div>
        <div style={stepGridStyle}>
          <div style={{ ...subBoxStyle, ...stepCardStyle }}>
            <div className="row">
              <b>① 录入宝宝</b>
              <span className="seg-chip" style={{ cursor: "default" }}>正脸 + 名字</span>
            </div>
            <div className="muted">
              添加 <b>1 张</b>清晰正脸照即可开始使用，建议补充 <b>3~10 张</b>不同角度、光线、时期的单人照更稳；同时填写<b>姓名与小名/署名</b>（三字或四字姓名会自动补充双字名），用于识别<b>没有露脸的画作、手工、奖状</b>。
            </div>
            {onNavigate && (
              <div>
                <a className="small" onClick={() => onNavigate("kids")}>
                  去录入宝宝 →
                </a>
              </div>
            )}
          </div>

          <div style={{ ...subBoxStyle, ...stepCardStyle }}>
            <div className="row">
              <b>② 统一导入照片和视频</b>
              <span className="seg-chip" style={{ cursor: "default" }}>指纹去重</span>
            </div>
            <div className="muted">
              支持把本地保存的文件夹选入/拖入、监听微信导出中转夹、原位扫描微信已下载明文视频，或同步群相册。按文件内容指纹自动去重并分流归档。
            </div>
            {onNavigate && (
              <div>
                <a className="small" onClick={() => onNavigate("import")}>
                  去导入照片视频 →
                </a>
              </div>
            )}
          </div>

          <div style={{ ...subBoxStyle, ...stepCardStyle }}>
            <div className="row">
              <b>③ 宝宝照片</b>
              <span className="seg-chip" style={{ cursor: "default" }}>人脸 + 作品</span>
            </div>
            <div className="muted">
              汇总展示命中的<b>人脸照</b>与<b>作品署名照</b>（支持按类型筛选），可拖动「严格程度」滑块实时调节人脸匹配阈值，一键批量导出无损高清原图。
            </div>
            {onNavigate && (
              <div>
                <a className="small" onClick={() => onNavigate("photos")}>
                  查看宝宝照片 →
                </a>
              </div>
            )}
          </div>

          <div style={{ ...subBoxStyle, ...stepCardStyle }}>
            <div className="row">
              <b>④ 宝宝视频</b>
              <span className="seg-chip" style={{ cursor: "default" }}>智能剪辑</span>
            </div>
            <div className="muted">
              自动定位宝宝出场的每一段画面，支持设置前后缓冲秒数与淡入淡出，点击时间胶囊即可试看并逐段微调起止时间，只裁剪导出有娃的精华片段。
            </div>
            {onNavigate && (
              <div>
                <a className="small" onClick={() => onNavigate("videos")}>
                  查看宝宝视频 →
                </a>
              </div>
            )}
          </div>
        </div>
      </div>

      {/* 2. 从微信班级群导入（重点章节 — 微信 4.2 实测验证） */}
      <div className="card ext-card">
        <div className="row">
          <h3>二、从微信班级群导入（电脑微信 4.x 实测可行 · 100% 零封号）</h3>
          <span className="ok small">★ 重点必读</span>
          {onNavigate && (
            <>
              <div className="spacer" />
              <button type="button" onClick={() => onNavigate("import")}>
                去「② 导入照片视频」
              </button>
            </>
          )}
        </div>
        <p className="hint" style={{ marginBottom: 10 }}>
          为什么不直接读取微信底层图片缓存？新版电脑微信（4.x）聊天图片底层是 <code>V2</code> 动态加密 <code>.dat</code> 文件，且在手机或电脑没手动点开过的群图片/视频在本地只有小缩略图或封面；强行注入或读取微信内存密钥解密有极高<b>封号风险</b>。<b>本软件绝不触碰微信进程，也绝不改动微信内部目录。</b>
        </p>

        <div style={{ display: "grid", gap: 10 }}>
          <div style={subBoxStyle}>
            <div style={{ marginBottom: 6 }}>
              <b>1. 电脑微信 4.x 实测 4 步批量导出法（一次性解决「折叠多图」+「未点开原视频下载」+「.dat 解密」）：</b>
            </div>
            <ol className="ext-steps">
              <li>
                <b>准备一个固定「微信导出中转」文件夹：</b>在剩余空间充裕的磁盘新建一个空文件夹，并在「② 导入照片视频」设为<b>「微信中转监听文件夹」</b>（也可等保存完再手动点「选择文件夹」导入）。
              </li>
              <li>
                <b>打开平铺网格并开启「多选」：</b>在电脑微信进入班级群 → 点击右上角 <kbd style={kbdStyle}>...</kbd> → 点击{" "}
                <b>「聊天记录」</b> → 切换到上方 <b>「图片与视频」</b> 标签页（所有被折叠的连发多图在这里会自动全部平铺展开）→{" "}
                <b>在任意一张缩略图上点击鼠标右键，选择「多选」</b>。
              </li>
              <li>
                <b>拉框勾选（每批最多 100 项）→ 点最底部「保存」图标：</b>在缩略图网格上<b>按住鼠标左键拖动拉框勾选</b>（微信单次最多允许选 <b>100 项</b>，几百张分 2～3 批即可）→ 点击窗口<b>最底部操作栏第 5 个「保存」图标</b>（向下箭头入托盘图标，位于「收藏」右边、「删除」左边）→ 选择第 1 步的中转文件夹，等待 <code>正在保存第 X/Y 个文件...</code> 进度条走完。
                <div className="muted small" style={{ marginTop: 2 }}>
                  此时微信官方客户端会自动从云端拉取未下载的高清原图与完整 <code>.mp4</code> 大视频，并自动解密保存为标准明文文件。
                </div>
              </li>
              <li>
                <b>自动找娃 + 释放双端原视频空间：</b>找娃助手会自动入库分析（或手动把中转夹拖入/选入）。挑出自家娃的照片并剪出几秒精华视频片段后，<b>中转文件夹可直接清空删除</b>——软件已永久记录文件 <code>SHA-256</code> 指纹，下次即使在微信里框选重了旧图也会毫秒级自动跳过！
              </li>
            </ol>
          </div>

          <div style={warnBoxStyle}>
            <b>⚠ 电脑微信 4.x 三个常见卡点说明（实测验证）：</b>
            <ul style={{ margin: "4px 0 0", paddingLeft: 18, display: "grid", gap: 2 }}>
              <li>
                <b>为什么不能从微信直接拖进找娃助手窗口？</b>微信 4.x 拖拽走内部虚拟流（<code>FilePromise</code>）而非本地文件路径，仅系统资源管理器文件夹窗口可接收，直接拖向第三方软件窗口不会响应；请务必用底部「保存」存入文件夹，或从本地文件夹拖入找娃助手。
              </li>
              <li>
                <b>为什么 <code>Ctrl+A</code> 或右键没反应？</b>微信 4.x「图片与视频」不支持 <code>Ctrl+A</code> 全选，右键「另存为」也只能单存 1 张；批量导出必须<b>先右键点「多选」→ 鼠标左键拉框勾选 → 点窗口最底部第 5 个「保存」图标</b>。
              </li>
              <li>
                <b>原视频下载后占微信空间怎么办？</b>微信批量保存视频时会在微信内部缓存一份副本；导出宝宝片段后，打开微信左下角{" "}
                <kbd style={kbdStyle}>≡</kbd> → <b>「设置」→「存储空间」→「聊天数据」右侧点「管理」</b>，即可安全清理该群的视频缓存释放数 GB 空间。
              </li>
            </ul>
          </div>

          <div style={subBoxStyle}>
            <b>2. 日常免导出的两项自动化辅助：</b>
            <ul style={{ margin: "6px 0 0", paddingLeft: 18, display: "grid", gap: 4 }} className="muted">
              <li>
                <b>同步手机看过的视频 + 原位零拷贝扫描：</b>在电脑微信左下角 <kbd style={kbdStyle}>≡</kbd> → <b>「设置」→「账号与存储」</b> 开启{" "}
                <b>「自动下载在其他设备查看过的内容」</b>（注：「自动下载小于 X MB 的文件」仅对文件附件生效，不管群视频）。开启后你在手机上点开看过的视频会自动同步到电脑微信 <code>msg/video</code> 明文目录，找娃助手可<b>一键原位直接抽帧识别</b>，不占双份磁盘空间！
              </li>
              <li>
                <b>中转监听文件夹自动入库：</b>绑定中转目录后，微信每次「保存」写完文件会自动触发后台增量入库与识别，无需来回切换窗口手工导入。
              </li>
            </ul>
          </div>
        </div>
      </div>

      {/* 3. 群相册小程序 / 网页版 */}
      <div className="card">
        <div className="row">
          <h3>三、群相册小程序 / 网页版（qxc.hi.cn）</h3>
        </div>
        <p className="hint" style={{ marginBottom: 10 }}>
          如果老师平时把照片发在微信「群相册」小程序，无需在微信里分批保存，提供两种更省心的方式：
        </p>
        <div style={{ display: "grid", gap: 8 }}>
          <div style={subBoxStyle}>
            <b>方式 1 · 安装 Chrome 扩展在网页端直接筛选（免全量下载原图）：</b>
            <div className="muted" style={{ marginTop: 4 }}>
              点击左侧导航栏底部的 <b>「安装 Chrome 扩展」</b> 按提示装入浏览器，打开 <code>qxc.hi.cn</code> 群相册网页并保持桌面端运行。扩展会自动复用桌面端已录入的宝宝参考照与姓名/小名，直接在浏览器里<b>增量扫新增</b>（已扫特征持久化保存在本机自动跳过）、<b>自动勾选命中照片（含作品署名照）</b>，并支持对命中的视频<b>自动裁剪宝宝出场片段</b>下载。
            </div>
          </div>
          <div style={subBoxStyle}>
            <b>方式 2 · 桌面端扫码一键同步：</b>
            <div className="muted" style={{ marginTop: 4 }}>
              在「② 导入照片视频」下方点击「扫码登录」，用手机微信扫码确认后即可按班级相册一键增量同步到本地库，已同步过的内容自动跳过（登录态有效期 24 小时）。
            </div>
          </div>
        </div>
      </div>

      {/* 4. 识别纠错与磁盘空间管理 */}
      <div className="card">
        <div className="row">
          <h3>四、识别纠错与磁盘空间管理</h3>
          {onNavigate && (
            <>
              <div className="spacer" />
              <button type="button" className="ghost small" onClick={() => onNavigate("settings")}>
                前往「设置」管理空间
              </button>
            </>
          )}
        </div>
        <div style={{ display: "grid", gap: 8, marginTop: 10 }}>
          <div style={subBoxStyle}>
            <b>越用越准的识别纠错（✓ 与 ✕）：</b>
            <ul style={{ margin: "6px 0 0", paddingLeft: 18, display: "grid", gap: 4 }} className="muted">
              <li>
                在结果卡片右下角点击 <b>✓</b>，会把该人脸补充进宝宝参考照，帮助认出侧脸、远景或不同光线角度，越用越准；
              </li>
              <li>
                遇到认错的人脸或误命中的作品照，点击 <b>✕</b> 会立即精准排除该条结果（同一张合照里若还有自家娃会自动保留切换），以后不再重复出现；
              </li>
              <li>
                参考照或宝宝姓名/小名每次增删改后，系统都会基于已提取特征<b>毫秒级自动重算相似度与作品匹配</b>，无需重新扫描原片。
              </li>
            </ul>
          </div>

          <div style={subBoxStyle}>
            <b>磁盘空间管理（大容量迁移 & 一键清理无娃原片）：</b>
            <ul style={{ margin: "6px 0 0", paddingLeft: 18, display: "grid", gap: 4 }} className="muted">
              <li>
                <b>一键迁移存储目录到大容量磁盘 / 移动硬盘：</b>系统盘空间不足时，可在「设置」中将媒体库经 <code>SHA-256</code> 完整性校验后一键平滑迁移到其他大容量分区或外接移动硬盘，所有宝宝档案、缩略图与识别记录完整保留；
              </li>
              <li>
                <b>一键清理未命中原片：</b>在「设置」中可一键清理媒体库内所有「未命中任何已录入宝宝」的原图与原视频（严格保护电脑其他目录与微信目录里的外部原片不误删），瞬间释放大量磁盘空间，同时保留仅占<b>几 KB 的文件指纹记录</b>——以后再次导入或同步时依然秒级跳过，绝不重复导入重扫！
              </li>
            </ul>
          </div>
        </div>
      </div>
    </section>
  );
}
