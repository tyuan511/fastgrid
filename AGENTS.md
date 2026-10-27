# fastgrid — Agent 协作规范

本文件是仓库内所有代码代理（pi / Claude Code / Cursor / Codex 等）的默认上下文，也同样是新人的上手说明。
`CLAUDE.md` 只做转发，规范以本文为准。

**语言约定**：对话、设计文档可以用中文；**代码、注释、提交信息、发布说明一律英文**。

---

## 1. 项目是什么

Rust workspace，一个跨平台的数据库客户端（对标 TablePlus 交互）+ 内嵌 MCP server，跑在 GPUI Kit 上。
一期做四个引擎：**SQLite → PostgreSQL → MySQL/MariaDB → SQL Server**。当前处于 pre-alpha，按里程碑推进。

必读文档（改之前先看，别凭印象动手）：

| 文档 | 什么时候必须读 |
|---|---|
| `docs/architecture.md` | 增删 crate、改依赖方向、改运行时/取消/元数据策略 |
| `docs/driver-abstraction.md` | 加驱动、改 `fg-core` 契约类型、改 `Session`/`Dialect`/`Capabilities` |
| `docs/data-model.md` | 加/改任何要落盘或对外暴露的类型（`record` 的 kind、载荷字段、迁移） |
| `docs/encryption-and-sync.md` | 碰加密、密钥、导出导入、同步、`fg-crypto` / `fg-store` |
| `docs/mcp-security.md` | 碰 MCP 的工具、权限、只读策略、审计 |
| `docs/roadmap.md` | 认领任务、完成后勾选、写验收标准 |
| `docs/shortcuts.md` | 加快捷键（同一焦点上下文内不得重复） |
| `docs/ui-testing.md` | 写或改 `fg-app` 的视图与 UI 测试（GPUI 测试有几个必踩的坑） |

---

## 2. 命令与提交前门槛

工具链由 `rust-toolchain.toml` 固定（stable + rustfmt + clippy），edition 2024，`rust-version = 1.92`（由依赖树里最高的 MSRV 决定）。

```bash
cargo fmt --all                                   # 格式化
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check                                  # 许可证/来源/重复依赖（CI 也跑）
cargo run -p fg-app                               # GUI（二进制名 fastgrid）
cargo run -p fg-cli                               # 无头（二进制名 fastgrid-cli）
```

**三条硬门槛**，与 `.github/workflows/ci.yml` 一致，macOS / Ubuntu / Windows 三端都要能过：

1. `cargo fmt --all --check` 干净；
2. `cargo clippy --workspace --all-targets -- -D warnings` 零告警；
3. `cargo test --workspace` 全绿。

本地过不了就别提交，更不要 push。Linux 端 gpui 需要的系统库见 `ci.yml`（`libxcb`/`libxkbcommon`/`libwayland`/`libasound`/`libfontconfig`/`libssl`/`libvulkan`）。

新外部依赖：版本写在根 `Cargo.toml` 的 `[workspace.dependencies]`，成员 crate 用 `xxx.workspace = true`；许可证必须落在 `deny.toml` 的白名单里（Apache-2.0 / MIT / BSD / ISC / Zlib / MPL-2.0 / CC0-1.0 / OpenSSL 等）。TLS 一律 **rustls**，不引入 OpenSSL；隧道用 `russh`。

---

## 3. 架构与 crate 边界

依赖**只能向下**，不许横向或向上：

```
fg-app (GPUI)      fg-cli (headless)
      \                /
        fg-service  <---- fg-mcp
       /     |     \
fg-driver  fg-store  fg-sql
       \     |     /
          fg-core        fg-crypto  <---- fg-store
```

- `fg-core`：领域类型与词汇（`Value` / `DataType` / `ObjectPath` / `Capabilities` / `Dialect` / `Error`）。**不做 IO、不依赖 UI、不依赖 tokio**。
- `fg-driver`：`Driver` / `Session` / `ExecutionStream` / `Batcher` 契约 + 各引擎实现，引擎代码在 `src/<engine>/` 后面挂 cargo feature。
- `fg-crypto`：纯密码学（信封加密、口令保护、Argon2id、密钥类型）。**不做 IO，不认识 fastgrid 的数据模型。**
- `fg-store`：本地 SQLite 存储，所有内容是加密的通用 `record`（见 `docs/data-model.md`）。只依赖 `DeviceKeyProvider` trait，**钥匙串的具体实现放在 `fg-service`**，`fg-store` 自己不碰钥匙串。
- `fg-sql`：纯函数——语句拆分、分类、浏览/DML 构造。不做 IO。
- `fg-service`：连接池、任务生命周期、元数据缓存、策略（只读/行数上限）、SSH/TLS。**独占 tokio runtime**；GUI 与 MCP 共用它。
- `fg-mcp` / `fg-cli`：**必须能在无显示器的机器上跑**。
- `fg-app`：**唯一**允许依赖 `gpui-kit` 的 crate。

上层的铁律：**不 `match` `DbKind`**。要差异就问驱动的 `Capabilities` 和 `Dialect`。加一个引擎 = 实现两个 trait + 一个 dialect，`fg-service` / `fg-app` / `fg-mcp` 一行不改（清单见 `driver-abstraction.md` §“Adding a driver”）。

UI 线程只做渲染和小的状态更新；数据库 IO 在 `fg-service` 的 tokio runtime 上，结果通过 channel 回到 UI。

---

## 4. 代码规范

- `unsafe_code = forbid`（workspace lints）。真需要 `unsafe` 先停下来讨论，不要偷偷加 `#[allow]`。
- **不 `unwrap()` / `expect()`**（clippy 已 warn，且 `-D warnings` 会失败）。生产路径一律返回 `fg_core::Result<T>` / `fg_core::Error`。
  测试里要放开，写在文件顶部，别在函数里散落 `#[allow]`：
  ```rust
  #![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]   // lib.rs
  #![allow(clippy::unwrap_used, clippy::expect_used)]                   // tests/*.rs
  ```
- 错误分层：引擎报的错映射成 `QueryError { message, code, position }`，编辑器要靠 `position` 画下划线、MCP 要靠 `code` 回 SQLSTATE；能力缺失用 `Error::Unsupported`，策略拦截用 `Error::PolicyDenied`，不要塞 `Other("...")` 了事。
- 不留 `dbg!` / `println!` 调试。日志用 `tracing`，**格式串里绝不出现密码、连接串、含敏感参数的 SQL**。
- 每个 crate 的 `lib.rs` 顶部写 `//!` 说明（一两段，指向相关设计文档），模块私有（`mod x;`），对外只在 `lib.rs` 里 `pub use` 重新导出——门面模式，别让调用方摸内部路径。
- 公共项写 doc 注释 `///`，说明「为什么」而不只是「是什么」。
- **`fg-core` 的公共类型是昂贵资产**：只允许**加**字段/变体，新字段带 `#[serde(default)]`；不重命名、不删除、不用 `deny_unknown_fields`；单元枚举用 `#[serde(other)]` 兜 `Unknown`，带数据的枚举列表走 `compat::lenient_vec`。做不到就走存储迁移。（规则原文在 `crates/fg-core/src/compat.rs` 顶部。）
- 命名：模块/文件 snake_case，类型 UpperCamelCase，crate 目录 `crates/fg-*`，二进制名 `fastgrid` / `fastgrid-cli`。
- 不做超出请求的改动：不顺手重构、不顺手改格式、不新增“将来可能用到”的抽象或 feature flag。

---

## 5. 测试规范

- 单元测试与被测代码同文件：`#[cfg(test)] mod tests { ... }`。
- 跨层行为放 `crates/<crate>/tests/<engine>.rs`（现有参考：`crates/fg-driver/tests/sqlite.rs`）。
- **每个新行为都要有测试**，包括失败路径；写了策略（只读、行数上限、取消、L1 拦截 DROP / 无 WHERE 的 UPDATE-DELETE）就必须有把它钉住的测试。
- 新驱动必须跑完与 SQLite 同一组场景：多语句、参数绑定、流式分批、`max_rows` 截断、结构化错误、元数据、只读会话、取消。网络引擎用 testcontainers（MySQL 5.7/8.x、MariaDB、PostgreSQL 12–17）。
- 数值/时间/JSON/数组/枚举等解码写属性测试；解码热点用 criterion 基准；网格滚动有固定数据集的性能检查（参考 `crates/fg-app/examples/grid_spike.rs`）。
- **不许改测试来迁就实现**。测试失败时先怀疑实现；确认是测试写错了，要在提交信息里说明为什么。
- **三端都要能跑**（CI 的 `check` 在 macOS / Ubuntu / Windows 各跑一遍）。两条最容易踩的坑：
  1. Windows 删不掉**仍被打开**的文件：测试里清理临时目录前必须先 `drop` 掉 `Store` / `Connection`，
     并且对删除结果做重试而不是 `unwrap()`（参考 `fg-store` 的 `testutil::cleanup`）；SQLite 的 WAL/SHM 句柄
     是在最后一个连接关闭后才释放的。
  2. 不要依赖 checkout 的换行符：`.gitattributes` 已把文本文件钉成 LF，任何按文件字节算的东西
     （迁移校验和、夹具摘要）都要显式忽略 `\r`，否则 Windows 上算出另一个值。

---

## 6. 安全红线（改动前先确认）

1. 用户产生的一切数据都以**加密记录**落盘，系统钥匙串（`keyring`）里**只放设备密钥**。密码是加密的 `secret` 记录，由 `fg-service` 读出后作为参数传给 `Driver::connect`。**绝不明文写进 `ConnectionConfig`、本地 SQLite、导出文件、日志、错误信息或测试夹具。** 改动加密或存储格式时必须保留并通过 `fg-store/tests/encryption.rs`（磁盘上搜不到明文），并同步 `docs/encryption-and-sync.md`。
2. MCP **默认只读**，且在两处同时生效：引擎层只读事务 + `fg-sql` 语句分类。任何削弱这一点的改动，必须在同一个提交里同步更新 `docs/mcp-security.md`。
3. 无头模式拒绝写操作；GUI 里的写要逐条显式确认。标记为生产（`Environment::Production`，未知环境同样按生产对待）的连接默认不暴露给 MCP。
4. 行数上限与语句超时在服务端强制执行，结果被截断要告诉调用方。
5. 每次 MCP 调用都进本地审计日志（谁、哪个连接、哪条语句、结果、耗时）。
6. 代理不得自行执行不可逆操作：`git push --force`、删分支、`rm -rf`、对真实数据库跑写语句（除非用户明确要求并确认）。

---

## 7. Git 提交规范

### 7.1 工作流

- `main` 是唯一长期分支。工作分支短生命周期，命名 `<type>/<slug>`：`feat/m1-grid-paging`、`fix/pg-cancel-token`、`docs/mcp-setup`。
- 更新用 `git pull --rebase`；feature 分支合并前 rebase 到 `main`，保持线性历史。不 `--force` 推 `main`。
- 一个提交只做一件事。逻辑改动与纯格式化改动不混在一个提交里。
- 提交前 `git status` 看一眼再 `git add`，**不要盲目 `git add -A`**（容易把临时文件、密钥、`target/` 带进去）。`Cargo.lock` 需要提交（本仓库有二进制）。
- 不提交：`target/`、`*.log`、`.DS_Store`、任何密钥或真实连接串、任何生成产物。
- **代理默认不 `git commit`，也不 `git push`** —— 只有用户明确要求时才做。

### 7.2 Commit message 格式

采用 Conventional Commits，**英文**：

```
<type>(<scope>): <subject>
                                ← 空行必填
<body：为什么改、影响是什么，每行 ≤72 字符>
                                ← 空行
[可选 trailer]
```

示例：

```
feat(fg-driver): stream SQLite rows through Batcher

Execute now emits ResultStart/Rows/ResultEnd per statement instead of
buffering the whole result set. A slow UI blocks the driver, and dropping
the stream stops the query, which is what cancellation needs.

Refs: M1.0
```

**type**（只用这些）：

| type | 用途 |
|---|---|
| `feat` | 新功能 |
| `fix` | 缺陷修复 |
| `refactor` | 不改外部行为的调整 |
| `perf` | 性能优化（要带前后数据） |
| `docs` | 文档 |
| `test` | 只增删测试 |
| `build` | Cargo.toml / features / 打包 |
| `ci` | `.github/` |
| `chore` | 格式化、清理、锁文件等杂项 |
| `revert` | 回滚 |

**scope**：受影响 crate 的目录名 `fg-core` / `fg-driver` / `fg-store` / `fg-sql` / `fg-service` / `fg-mcp` / `fg-cli` / `fg-app`；跨多个用 `workspace`，另有 `deps`（依赖变动）、`ci`、`docs`。历史提交一律带 scope（2026-10-20 已把早期不带 scope 的 8 个提交重写成规范写法并 `--force-with-lease` 推送，无遗留例外）；新提交同样必须带。

**subject**：祈使现在时（`add`，不是 `added`/`adds`）；小写开头；**结尾不加句号**；整行 ≤72 字符（建议 ≤60）；说清「做了什么」，别写 `fix bug`。

**body**：解释**为什么**这样改、有什么影响/取舍/风险，diff 已经说明「改了哪里」，正文不要复述。多段用空行分隔，每行 ≤72 字符。关联里程碑（`Refs: M2`）或 issue。

**破坏性变更**：type 后加 `!` —— `feat(fg-core)!: rename ObjectPath::catalog to namespace`，并在正文末尾单独一段

```
BREAKING CHANGE: persisted workspace files from older builds need a store migration.
```

**trailer**：本仓库的既有惯例是可选的 `Co-Authored-By: <name> <email>`；除此之外不要加 `Generated-by`、emoji 或其它水印。

### 7.3 好 / 坏对照

| 好 | 坏 | 原因 |
|---|---|---|
| `fix(fg-service): keep pending cancel from killing the next task` | `fix: cancel bug` | 说清是哪个 bug |
| `feat(fg-sql): split statements without breaking strings or comments` | `update sql splitter` | 缺 type/scope，且没信息量 |
| `refactor(fg-driver)!: Session::describe takes ObjectPath` | `refactor` | 破坏性变更必须可见 |
| `docs: record rustls-only TLS policy in architecture.md` | `docs: 更新架构文档` | 提交信息统一英文 |

`git log` 里标题一眼能读出「哪个模块发生了什么」，就算合格。

### 7.4 版本号与 tag

- 版本在根 `Cargo.toml` 的 `[workspace.package].version`（当前 `0.0.1`）统一升，遵循 SemVer；`0.x` 期的破坏性调整算 patch/minor，但要在提交信息里写清。
- 发布用 annotated tag：`git tag -a v0.1.0 -m "fastgrid 0.1.0"`；升版本与 tag 分开两个提交（`build(workspace): release v0.1.0` 单独一条）。

---

## 8. 文档维护

改了行为就改文档，同一个提交里改：

- 新增/调整设计决策 → 写进对应 `docs/*.md`（为什么这么选，而不只是是什么）。
- 完成里程碑条目 → 勾选 `docs/roadmap.md`，补齐验收数据。
- 加了界面动作 → 更新 `docs/shortcuts.md`，确认同上下文不冲突。
- 改了安全模型 → 更新 `docs/mcp-security.md`。
- 文档里的性能/事实结论要能复现，附上复现命令或样例路径。

---

## 9. 快照（会过期，2026-10-27）

- **M0–M10 全部落地**：数据层、应用外壳、只读网格、SQL 编辑器 + `fg-lsp`、安全模式、暂存变更与数据编辑、
  PostgreSQL、MySQL/MariaDB、结构管理 + 导入导出、MCP 完整版（stdio + 本地 HTTP + 写入门控 + 审批 + 审计）、
  打磨与三端打包脚本、加密跨端同步，以及 **M10 的 SQL Server 驱动**（第四个引擎）。roadmap 里唯一还没勾的
  一项是 M8 的「Beta 发布 → 收集反馈 → v1.0」；版本号仍是 `0.0.1`，一个 tag 都没打，发布流程尚未启动。
- **SQL Server 的已知限制**（都在 `docs/connections/mssql.md` 与 roadmap 的 M10 诚实缺口里）：只读模式靠驱动内
  语句分类而非引擎事务（`Capabilities::read_only_session = false`）；取消用 `KILL`，低权限登录会失败并退化为
  丢弃连接；结果列没有来源信息（`column_origin = false`）；EXPLAIN 不支持；只在本地 SQL Server 2022 容器上实测。
- **界面的视觉效果从未被人眼验证过**，只验证了行为；改视觉相关的东西请让用户本地看一眼。
- 剩下的是细节与验证缺口（不是功能缺失）：范围（矩形）选区 gpui-kit 不支持；分隔条双击复位；MySQL 的 `USE`
  免重连切换；网格暂存编辑（Changeset）不在崩溃恢复的会话状态里；Claude Desktop / Cursor / Claude Code 三个真
  客户端的 MCP 端到端演示未跑（官方 TS SDK 已把 stdio + HTTP 双路跑通）；PG 12–17 与 MySQL 5.7/8.0/8.4、
  MariaDB 10.6/11 的 testcontainers 矩阵未跑（本地实测只有 PG 16.2 与 MariaDB 10.11.7）；macOS 与 Windows 的
  打包 CI 任务从未跑过（第一个 tag 就是首测）；自动更新只检查不下载；同步的 S3 / WebDAV 未经真实服务器验证、
  同步密码只存内存、没有后台自动同步；图标按钮的无障碍名称被上游 `gpui-component` 挡住。
- 三端 CI：`deny` / macOS / Ubuntu 绿。Windows 失败过两次原因：早前是 `fg-store` 的落盘测试（文件占用 + 迁移
  校验和，已在测试与 `deny.toml` 里修掉）；最近一段时间是 `crates/fg-service/src/ssh.rs` 顶部的两个 russh agent
  import 在 Windows 上是死导入（`SshAuth::Agent` 的实现被 `#[cfg(unix)]` 包住，Windows 走 `Unsupported` 早退），
  2026-10-27 给那两行 `use` 加了 `#[cfg(unix)]`，待 CI 复跑确认。**注意本机只有 `aarch64-apple-darwin` target，
  Windows 路径无法本地验证**，改 `#[cfg]` 分支时要把「推送后 CI 复跑」当作验收的一部分。
- deny 只在 GPUI Kit 传递依赖的 `unmaintained` 通告上失败过，已逐条忽略。
- `cargo deny`（本机 0.20.2）可跑；新增依赖若带来 `unmaintained` 通告，在 `deny.toml` 里**逐条**忽略并写清来源，
  不要把 `unmaintained` 整体降级成 warning。
- 三条硬门槛最近一次本地实测（2026-10-27，SQL Server 接入后）：fmt 干净；`clippy --workspace --all-targets
  -- -D warnings` 零告警；`cargo test --workspace` 937 passed / 0 failed / 2 ignored（ignored 是钥匙串实测与
  百万行性能）。网络引擎的行为测试要显式开 feature 才编：`cargo test -p fg-driver --features mssql --test mssql`
  （本地 SQL Server 2022 容器上 20 passed），MySQL/PG 同理；默认 feature 只有 `sqlite`，所以 `--workspace`
  不会跑它们。
