# 群相册 (qxc.hi.cn) 接口分析

> 分析时间：2026-11-20。来源：网页版 https://qxc.hi.cn/#/ 抓包 + 前端 bundle（`/js/app.*.js`，已混淆）。

## 基础

- API Base：`https://cloud-release-3g472rpybcfa654e-1326935991.ap-shanghai.app.tcloudbase.com`（腾讯云 CloudBase 云函数）
- 公共参数：`token=<token>&env=release&os=web`（GET 放 query，POST 放 JSON body）
- 失败返回：`{"code":-3,"msg":"ERROR_NOT_LOGIN"}`；成功 `code == 0`
- 无需 Cookie / Referer，直接 HTTP 调用即可（已用 PowerShell 验证）

## 扫码登录

1. `POST /createWebLoginTicketForWeb`  body `{"env":"release","os":"web"}`
   → `{"code":0,"token":"<T>","sid":"<T>","ts":1795119050991,"expiryTime":1795119095991}`
   - 二维码有效期 45 秒（`expiryTime - ts`），过期需重新创建
2. 本地生成二维码，内容：`https://qxc.hi.cn/loginqrcode.html?sid=<sid>&ts=<ts>`
   - 用户用微信扫码 → 打开群相册小程序确认
3. 轮询 `POST /checkLoginForWebV2`  body `{"token":"<T>","env":"release","os":"web"}`
   - 未确认：`{"code":-3,"msg":"ERROR_NOT_LOGIN"}`
   - 成功：`{"code":0,"data":[{"token":"<T>","expiryDuration":86400000,"wxName":"..."}]}`
   - **token 有效期 24 小时**，过期后需要重新扫码

## 数据层级

### 1. 群相册列表
`GET /getAllAlbumGroupForWebV3?token=…`
```json
{"albumList":[{"groupId":"Gh5g…","groupName":"某某幼儿园小一班","imgCnt":123,"videoCnt":4,
  "lastUploadTime":"2026-11-13T09:24:37.140Z","lastDeleteTime":"…","canDownload":true}],"code":0}
```
- `lastUploadTime`：可用于快速判断是否有新内容（不变则跳过）

### 2. 照片集（子文件夹）
`GET /getAlbumSubFolderListForWebV4?groupId=…&page=0&token=…`
- `hasMore` 分页；内置 `_id=-1`「所有照片视频」、`_id=-2`「所有视频」

### 3. 媒体列表
`GET /getSubFolderMediaForWebV3?groupId=…&subfolderId=-1&page=N&token=…`
```json
{"list":[{"_id":"d87b…","fileId":"cloud://cr/medias/<groupId>/group_1794561412965_xxx_s.jpg",
  "fileType":0,"openId":"…","timelineId":"…","uploadTime":"2026-11-13T09:24:37.033Z",
  "tempFileURL":"https://releasec.qxc.hi.cn/medias/<groupId>/group_…_s.jpg?imageMogr2/thumbnail/800x/format/jpg"}],
 "page":0,"isEnd":false,"code":0}
```
- 每页 40 条，按 `uploadTime` **倒序**；`isEnd` 表示末页
- `fileType`：0 = 图片，1 = 视频
- `_id` 唯一，作为去重主键

## 文件下载

- 原文件 = `tempFileURL` 去掉 `?` 及后面的处理参数
  - 图片：`…_s.jpg`（约 0.7MB，群相册不保留真·原图，`canUploadOrigImg=false`）
  - 视频：`…mp4`（完整视频，~29MB）；`?ci-process=snapshot&time=0.01` 为封面
- CDN `releasec.qxc.hi.cn` 公开可读，无需 token

## 增量同步策略

- 用 `subfolderId=-1` 拉全量（包含所有照片集）
- 从 page 0 往后翻，遇到**已在本地库里的 `_id`** 即停止（倒序，后面的都同步过了）
- 首次同步 / 历史数据：一直翻到 `isEnd=true`；每页落库，中断后可续（记录未完成的最旧 uploadTime）
- 先比较群相册的 `lastUploadTime`，没变直接跳过
- 删除：`lastDeleteTime` 变化时可做一次全量对账（可选）
