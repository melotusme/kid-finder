# 找娃助手 (kid-finder)

从幼儿园老师发在微信群 / 群相册里的大量照片和视频中，自动找出**自家宝宝**的那些，并把视频里宝宝出现的片段裁剪出来。

- 人脸识别全部在本机完成（InsightFace 开源模型 + ONNX Runtime），照片不会上传
- 支持 Windows / macOS（Apple Silicon）
- 自动更新

## 使用

1. **设置**：首次打开先下载人脸模型（约 190MB）；要处理视频再点「自动安装」ffmpeg（macOS 可用 `brew install ffmpeg`）
2. **① 录入宝宝**：新建宝宝，添加几张清晰的正脸照（越多角度越准）
3. **② 导入照片视频**：
   - 把微信里保存下来的照片/视频文件夹拖进来；或
   - 扫码登录「群相册」小程序，一键同步整个相册（登录有效期 24 小时）
   - 重复文件按内容自动去重，并提示之前何时导入过
4. **③ 找到的宝宝**：查看结果，可调节相似度阈值；点「是 / 不是」纠正，会越用越准；选中后导出，照片原图复制、视频只导出宝宝出现的片段

## 下载

到 [Releases](https://github.com/melotusme/kid-finder/releases/latest) 下载：

- Windows：`*_x64-setup.exe`
- macOS：`*_aarch64.dmg`。应用未经 Apple 签名，首次打开若提示「已损坏」，在终端执行：
  `xattr -cr /Applications/找娃助手.app`

## 开发

```bash
npm install
npm run tauri dev
```

需要 Node 20+ 和 Rust stable。数据（数据库、模型、导入的媒体）存放在系统应用数据目录，可在「设置」页打开。

## 发布

推送 `v*` tag 触发 GitHub Actions 构建并发布 Release（含自动更新用的 `latest.json`）。
需要在仓库 Secrets 中配置 `TAURI_SIGNING_PRIVATE_KEY` 和 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`。
发版前同步修改 `package.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml` 中的版本号。

## 说明

群相册接口为对其网页版的逆向整理，见 [docs/qxc-api.md](docs/qxc-api.md)，仅供个人下载自己有权查看的相册使用。

## License

MIT
