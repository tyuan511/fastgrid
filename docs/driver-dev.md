# 驱动开发指南

新引擎的完整契约见 [driver-abstraction.md](driver-abstraction.md)，
这里是落地步骤与铁律。

## 步骤（对照 driver-abstraction.md 的 Adding a driver 清单）

1. `fg-core` 加 `DbKind` 变体与 dialect（含单元测试）。
2. `fg-driver/src/<engine>/`，挂在 cargo feature 后面，
   实现 `Driver` 与 `Session`，在 `driver_for` 里注册。
3. 原生类型映射到 `TypeCategory`，值映射到 `Value`
   （认不出的走 `Value::Other`，不要丢信息）。
4. 用 `Batcher` 做分批与行数上限；原生错误映射成
   `QueryError`（带 code 与 position，编辑器靠它画线）。
5. 诚实地声明 `Capabilities`：不支持就说不支持，
   不要用 `Error::Other` 糊弄过去。
6. 跑完与 SQLite 同一组行为测试
   （`crates/fg-driver/tests/sqlite.rs` 的场景：多语句、
   参数绑定、流式分批、`max_rows` 截断、结构化错误、
   元数据、只读会话、取消）。网络引擎的测试读环境变量
   指向真实服务器（如 `FG_TEST_POSTGRES_URL`、
   `FG_TEST_MYSQL_URL`），本地没有 docker 时就用这种方式。

## 铁律

- 依赖只能向下：`fg-app` / `fg-cli` → `fg-service` /
  `fg-mcp` → `fg-driver` / `fg-store` / `fg-sql` → `fg-core`。
  只有 `fg-app` 能依赖 gpui-kit；只有 `fg-service` 能碰
  tokio runtime、连接池、SSH/TLS。
- 上层不 `match DbKind`：要差异就问驱动的 `Capabilities`
  与 `Dialect`。加一个引擎，`fg-service` / `fg-app` /
  `fg-mcp` 应该一行不改。
- TLS 一律 rustls，不引入 OpenSSL；隧道用 russh。
- `fg-core` 的公共类型只加字段/变体，不重命名、不删除，
  新字段带 `#[serde(default)]`。
- 生产路径不 `unwrap` / `expect`（clippy 已 warn，
  `-D warnings` 会失败）；测试文件顶部统一放开。
- 每个新行为都要有测试，包括失败路径；不要改测试迁就实现。
