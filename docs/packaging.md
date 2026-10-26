# 打包与更新

## Linux（本地可验证）

```sh
./scripts/package-linux.sh [version]
```

版本默认取工作区 `Cargo.toml`，产物在 `dist/`：

- `fastgrid-<version>-amd64.deb`：`fastgrid` 与 `fastgrid-cli` 两个
  二进制、桌面入口、256px 图标。依赖系统 GUI 库
  （`libxkbcommon-x11-0 libxcb1 libwayland-client0
  libasound2t64 | libasound2t libfontconfig1 libssl3 libvulkan1`；
  asound 在 Ubuntu 24.04 上叫 libasound2t64，在 Debian 上叫
  libasound2t，故写成二选一；其余名字两边一致）。
- `fastgrid-<version>-x86_64.AppImage`：同上内容加 `AppRun`。
  它**不是全量捆绑**：GUI 系统库仍来自宿主，和 .deb 的依赖
  一致。`appimagetool` 首次运行时从其 continuous 发布页下载，
  并以 `--appimage-extract-and-run` 运行（CI 与最小化宿主
  都没有 FUSE）。

验证方式（本机跑过）：`dpkg-deb --info/--contents`、解包后
`ldd` 无缺失库、`fastgrid-cli --help` 正常输出、deb 安装后
可正常配置与卸载。GUI 本体在无显示器的机器上无法开窗：
本机只验证到链接与加载运行（无 `DISPLAY` 时 5 秒内无输出、
未崩溃退出，`timeout` 返回 124），开窗本身未验证。

## macOS / Windows（CI 专属，未在本机验证）

打 tag 触发 `.github/workflows/release.yml`（`v*`，版本号
取 tag 去掉 `v`）：

- macOS：手工组装 `Fastgrid.app`（`fastgrid` + `fastgrid-cli`
  放 `Contents/MacOS`，`assets/AppIcon.icns`，最小 `Info.plist`），
  再 `hdiutil` 打 `.dmg`。只有仓库里配好 `APPLE_CERTIFICATE` /
  `APPLE_CERTIFICATE_PASSWORD` / `APPLE_SIGNING_IDENTITY` 时才
  签名，配好 `APPLE_ID` / `APPLE_PASSWORD` / `APPLE_TEAM_ID`
  时才公证；缺 secret 时产物是未签名的，job 不会因此失败。
- Windows：`choco` 装 WiX，`cargo-wix` 打 `.msi`。`wix/main.wxs`
  由 `cargo wix init` 在 CI 里按工具模板生成，不手写（变量名
  以工具为准）。这个 job 从未跑通过：第一个 tag 才是第一次
  测试，绿了也只代表“打出来了”，不代表“在 Windows 上装得上”。

## 更新检查（默认关闭）

`PrivacySettings.auto_update` 默认 `false`，启动页诊断面板里
可以开关。打开后，启动页每次打开时向 GitHub releases API
问一次最新 tag（10 秒超时，失败静默），比当前版本新才在
启动页下方显示一行提示。**只检查、不下载、不安装**：安装
永远是用户手动从发布页下载。两个隐私开关是分开持久化的，
改一个不会把另一个重置回默认。
