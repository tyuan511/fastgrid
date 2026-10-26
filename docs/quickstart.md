# 快速开始

fastgrid 是跨平台数据库客户端（对标 TablePlus 的交互），内嵌 MCP
server。当前 pre-alpha：SQLite、PostgreSQL、MySQL/MariaDB 三个引擎
可用，MCP 默认只读。

## 安装与运行

- 从源码：`cargo run -p fg-app`（GUI，二进制名 `fastgrid`），
  `cargo run -p fg-cli`（无头，二进制名 `fastgrid-cli`）。
- Linux 打包产物：见 [packaging.md](packaging.md)（`.deb` 与
  AppImage 由 `scripts/package-linux.sh` 产出）。
- GUI 需要的系统库见 `.github/workflows/ci.yml` 里的 apt 列表
 （`libxcb` / `libxkbcommon` / `libwayland` / `libasound` /
  `libfontconfig` / `libssl` / `libvulkan`）。

## 第一个连接

1. 启动后在启动页按 `⌘N` 新建连接。
2. 选引擎，填表单；网络型连接（PostgreSQL / MySQL）也可以在
   “Connection URL（paste，then parse）”里粘贴连接串再解析。
   各引擎的字段说明见 [connections/](connections/) 下的指南。
3. 点连接，对象树出现后点一张表即开始浏览——从启动到第一张表
   的目标是 1 分钟内。

## 日常操作

- 新建查询标签：`⌘T`；执行：编辑器焦点内 `⌘Return`
  （`⌘⇧Return` 执行到光标处分号为止的语句，`⌘.` 取消）。
- 查询历史：`⌘Y`。命令面板：`⌘⇧P`。完整快捷键见
  [shortcuts.md](shortcuts.md)。
- 编辑查询结果：仅单表且有主键的结果集可编辑，改完 `⌘S`
  提交；提交前可用 `⌘⌥P` 预览要执行的 SQL。
- 安全模式：连接可设 Normal / Alert / Strict / ReadOnly。
  写操作在非只读模式下也要经过确认（Alert 会逐条问）；
  ReadOnly 连接拒绝一切写。
- 结构模式：在表标签页切到 Structure，可以暂存列、索引、
  外键与 DDL 改动，预览确认后再应用。注意：MySQL 的 DDL
  不可回滚，预览里会提示。
- 导入导出：网格与结果集可用 `⌘⇧E` 导出（CSV / JSON / SQL，
  支持 gzip）、`⌘⇧I` 导入 CSV。备份见启动页的备份面板。

## MCP 与诊断

- 想让外部 AI 客户端查库：见 [mcp-setup.md](mcp-setup.md)；
  完整安全模型见 [mcp-security.md](mcp-security.md)。
- 出问题先看 [diagnostics.md](diagnostics.md)：日志位置、
  轮转、诊断包导出与遥测开关（默认关闭，且什么都不发送）。
