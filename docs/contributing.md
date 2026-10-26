# 贡献指南

fastgrid 还在 pre-alpha，`main` 是唯一长期分支。
先读仓库根目录的 [AGENTS.md](../AGENTS.md)——它是
代理与新人的共同上下文，这里只摘要。

## 环境

- Rust stable（`rust-toolchain.toml` 固定，含 rustfmt 与
  clippy），edition 2024。
- Linux 上 GUI 需要的系统库见 `.github/workflows/ci.yml`
  的 apt 列表；macOS / Windows 由 CI 覆盖。
- 依赖版本：新外部依赖先写根 `Cargo.toml` 的
  `[workspace.dependencies]`（历史上有 inline 的先例，
  但新依赖优先走 workspace）；许可证必须在 `deny.toml`
  的白名单里；TLS 只用 rustls。

## 提交前门禁（三条，缺一不可）

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
```

本地过不了就别提交。`cargo test` 在小核机器上建议
单线程跑（`-- --test-threads=1`），gpui 的视图测试
在负载下会因 20 秒 settle 超时而抖动；先单跑一次
再怀疑代码。

## 提交规范

- Conventional Commits，英文：`feat(fg-sql): ...`、
  `fix(fg-service): ...`；scope 是 crate 目录名，
  跨多个用 `workspace`；标题祈使小写、无句号、
  ≤72 字符；正文写“为什么”，每行 ≤72 字符。
- 一个提交只做一件事；格式化与逻辑改动不混；
  `git add` 前先看 `git status`，不要 `git add -A`；
  不提交 `target/`、日志、密钥与生成产物。
- 改了行为就改文档，同一个提交里改：新动作进
  [shortcuts.md](shortcuts.md)，里程碑进度进
  [roadmap.md](roadmap.md)，安全模型进
  [mcp-security.md](mcp-security.md)。
- 对话、设计文档可以用中文；代码、注释、提交信息、
  发布说明一律英文。

## 工作流

- 工作分支短生命周期，命名 `<type>/<slug>`，
  如 `feat/m1-grid-paging`；更新用 `git pull --rebase`，
  合并前 rebase 到 `main`，不 `--force` 推 `main`。
- 当前阶段维护者直接向 `main` 落地；大改动先开 issue
  对齐方向，小修小补可以直接动手。
- 安全红线：用户数据加密落盘，钥匙串只放设备密钥；
  MCP 默认只读（引擎层与语句分类层同时生效）；
  不擅自执行不可逆操作（删分支、强制推送、对真实
  数据库跑写语句）。
