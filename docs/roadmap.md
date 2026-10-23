# Roadmap

## 范围

**一期（v0.x → v1.0）支持三个引擎：SQLite、PostgreSQL、MySQL（含 MariaDB）。** 其他引擎不在一期范围内。

定位：对标 TablePlus 的原生速度与交互手感（调研见 [tableplus-interaction-research.md](tableplus-interaction-research.md)），
加上开源免费，以及面向外部 agent 的 MCP。

估算以一名全职开发者为准，只是排期参考，不是承诺。每个里程碑结束时都应是**可运行、可演示**的状态。

| 里程碑 | 主题 | 估时 | 累计 |
|---|---|---|---|
| M0 | 骨架与技术验证 | ✅ 已完成 | — |
| M1 | 数据层补全 + 应用外壳（SQLite 端到端浏览） | 5w | 5w |
| M2 | SQL 编辑器 + 一级安全模式 | 3w | 8w |
| M2.5 | MCP 只读 MVP | ✅ 已实现，待联调 | — |
| M3 | 暂存变更与数据编辑 | 4w | 13w |
| M4 | PostgreSQL | 3w | 16w |
| M5 | MySQL / MariaDB | 2.5w | 18.5w |
| M6 | 表结构管理 + 导入导出 | 3w | 21.5w |
| M7 | MCP 完整版 | 3w | 24.5w |
| M8 | 打磨与发布（v1.0） | 3w | 27.5w |
| M9 | 加密跨端同步（v1.x） | 4w | 31.5w |

一期（到 v1.0）约 27 周（6 个多月）；M9 是发布之后的增量，数据结构与加密从 M1 起就按它设计，M9 只补同步后端与协议。排序的原因：先用最简单的引擎（SQLite，无网络、无认证）把 UI 与交互打磨到位，
再接入网络型引擎，这样 M4/M5 只是在已验证的抽象上新增驱动，而不是同时调试驱动和界面。

---

## M0 骨架与技术验证 ✅

- [x] Cargo workspace、8 个 crate、Apache-2.0、`cargo-deny` 配置
- [x] 三端 CI 配置（fmt / clippy / test / deny）。**状态：配置已写好，三端实际运行结果尚未确认**
- [x] 网格性能验证：gpui-kit `DataTable`，100 万行 × 200 列，macOS 上平均 59.9fps、
      无超过 100ms 的帧、60 秒内存增长不足 1MB（数据现场生成，见 `crates/fg-app/examples/grid_spike.rs`）。
      Windows / Linux 未测
- [x] 数据层抽象设计（[driver-abstraction.md](driver-abstraction.md)）
- [x] SQLite 驱动：多语句、参数绑定、流式批处理、行数上限、取消、只读会话、元数据（16 个集成测试）

---

## M1 数据层补全 + 应用外壳（5w）

目标：能在 GUI 里打开 SQLite 文件，浏览所有表，网格滚动流畅，具备 TablePlus 式的整体布局。**本里程碑只读，不含编辑。**

### M1.0 数据层补全（1w）— 已完成

调研报告的需求暴露出抽象里的缺口，在做网格之前补上，避免之后改接口：

- [x] `ColumnMeta.origin`：结果列的来源（namespace / table / column），用于「可编辑的查询结果」。
      SQLite 用列元数据；PG 用表 OID + 列号；MySQL 用 org_table / org_name
- [x] `Session::list_relations(namespace)`：批量返回全库外键，供反向引用、ER 图、MCP 使用（避免逐表 N 次往返）
- [x] `ObjectInfo.row_estimate`：对象树中的行数估算（SQLite 无廉价估算，返回 `None`）
- [x] `fg-sql`：结构化过滤条件（列、操作符、值）→ 经 `Dialect` 编译为带参数的 SQL；浏览查询构造器（分页、排序、过滤、主键兜底排序、`COUNT(*)`）；
      用户手写的 `WHERE` 经校验只能是单个表达式，且可对 agent 整体禁用
- [x] `fg-sql`：语句拆分（分号感知，处理字符串、注释、`$$`、SQLite/MySQL 触发器体、光标所在语句）与语句分类（读 / 写 / DDL / 事务 / 工具 / 未知，
      危险语句：DROP、TRUNCATE、无 WHERE 的 UPDATE/DELETE、破坏性 ALTER；可穿透可写 CTE、`EXPLAIN ANALYZE`、MySQL 可执行注释）。
      **MySQL 的 `DELIMITER` 与不带分隔符的存储过程体留到 M5**
- [x] 以上全部有单元测试（`fg-sql` 47 个）；与真实 SQLite 的端到端见 `fg-service/tests/e2e.rs`

**验收**：`cargo test --workspace` 通过；`list_relations` 在含外键的 SQLite 库上返回正确结果。

### M1.1 `fg-store` 与 `fg-service` 最小版（1w）

- [x] `fg-crypto`：XChaCha20-Poly1305 封装、Argon2id 口令保护、密钥零化（16 个测试）
- [x] `fg-store`：加密的通用 `record` 表、迁移框架（备份 / 回滚 / 拒绝新版本）、设备密钥校验、墓碑与版本号、
      连接 / 分组 / 密码 / 设置 / 工作区 / 表视图 / 历史 / 审计的读写、同步原语（`dirty_records` / `mark_clean` / `apply_remote`）、
      口令加密的导出 / 导入；「磁盘上无明文」回归测试（含 WAL）。设计见 [encryption-and-sync.md](encryption-and-sync.md)、[data-model.md](data-model.md)
- [x] 系统钥匙串：`KeychainKeyProvider`（`keyring` 4.x，条目 `device-key:<store_id>`）；逻辑用内存后端测试。
      **真实钥匙串还需在三端各手动验证一次：`cargo test -p fg-service -- --ignored os_keychain`（macOS 已通过编译，三端均未实测）**
- [ ] 首次启动与钥匙串丢失的引导：`KeyUnavailable` 时给出明确提示（从同步或加密备份恢复）；引导用户导出加密备份
- [x] 片段（`snippet`）与编辑器草稿（`document`）的读写接口：`fg-store` 加密 CRUD（墓碑删除；草稿按 `updated_at` 排序；片段按 `sort_order` 再 `updated_at`；连接删除级联其草稿与连接作用域片段，全局片段保留；`StoredDocument` / `StoredSnippet` 把不可读记录和缺失分开，保存拒绝覆盖不可读行），`Service` 转发同一组方法。**界面未做**，仍属 M2
- [x] `fg-service`：按连接的会话池（独占租约，`:memory:` 只用 1 个会话）、任务生命周期（流式结果、取消、丢弃即取消）、
      元数据缓存（TTL + `refresh`）、tokio runtime 桥接（公共 API 为 `async`，可从任意执行器调用）、
      连接增删改与密码（加密存储）、`test_connection`、`subscribe()` 事件、工作区 / 表视图 / 设置的读写、
      表浏览（`browse` / `count`，主键兜底排序、`max_cell_bytes` 截断）
- [x] 取消语义：取消只在任务仍在运行时转发给驱动；任务结束前会话不归还；丢弃未读完的流会先取消。
      带变异验证的测试（去掉保护后「晚到的取消」测试必然失败）
- [x] 测试：服务层对真实 SQLite 的端到端测试（20 个，无 GUI）

### M1.2 应用外壳（1.5w）— 主体完成，细节待补

对应调研报告 §3、§4.1–4.3。行为用无头 UI 测试验证（见 [ui-testing.md](ui-testing.md)），**视觉效果还没有人眼验证过**。

- [x] 主窗口布局：顶栏（连接环境胶囊）、标签栏、左侧栏、主工作区、底部状态栏。侧栏默认 220px，可拖拽调整（140–450，随会话保存）。*双击分隔条复位待做（分栏组件不支持）*
- [~] 连接启动页：列出已保存连接、环境色点、SQLite 文件选择器、双击 / 按钮打开、连接失败提示。
      已支持搜索、编辑 / 复制 / 删除（带确认）、键盘上下 / 回车、`⌘N` 新建。*待做：分组界面；网络型连接表单随 M4*
- [~] 顶栏环境胶囊：显示连接名、引擎与环境色。*`⌘K` 数据库 / 命名空间切换待做（SQLite 只有 `main`，PG 在 M4 才有意义）*
- [~] 左侧对象树：按类型分组（Tables / Views…）、顶部模糊过滤框、双击打开。
      行数估算后缀已接入（SQLite 取自 `sqlite_stat1`）。*待做：匹配字符高亮、键盘导航需实机确认（树组件自带）*
- [x] 多标签（32px 自绘标签条，图标 + 标题 + 关闭，拖拽重排已有无头测试）：打开（同一张表复用标签）、关闭、`⌘W`、`⌘[` / `⌘]` / `Ctrl+Tab` 切换、关闭最后一个显示提示、会话保存（防抖）与恢复（跳过已不存在的表）。
- [x] `⌘P` Quick Open：模糊匹配（`ordit` → `order_items`）、上下键选择、Enter 打开、Esc 关闭并恢复焦点
- [~] 明暗主题：全部颜色取自主题 token，没有硬编码。已跟随系统，并可用顶栏按钮在 系统/明/暗 间循环（选择被持久化）。
- [x] 快捷键：`⌘P` `⌘W` `⌘B` `⌘R` `⌘F` `⌘C` `⌘[` `⌘]`（见 [shortcuts.md](shortcuts.md)）

### M1.3 只读数据网格（1.5w）— 主体完成

对应调研报告 §4.4、§4.5、§5.1。

- [x] 基于 `DataTable` 的表浏览：分页（100 / 300 / 1000）、后台 `COUNT(*)`、首尾页 / 上下页；过期结果按代号丢弃（有变异验证）；加载期间状态栏显示 `Loading…`。*预取下一页未做*
- [~] 表头：列名 + 类型徽章 + 排序（点击在升序 / 降序 / 关闭之间循环）。列宽拖拽与列重排由组件自带（未实机确认）。
      外键列显示 ↗ 标识。*待做：Shift 点击多列排序（模型已支持，组件点击事件拿不到修饰键）、双击自适应列宽*
- [x] 单元格渲染：数值右对齐等宽、日期 / UUID 等宽、`NULL` 药丸（与字符串 "NULL" 区分）、空串留白、布尔着色、大字段 / 二进制摘要（`[BLOB 45 KB]`、`[JSON 40 keys]`）、换行折叠成一行
- [~] 选区：单元格 / 行 / 列（组件自带）；`⌘C` 复制为 TSV（含制表符 / 换行 / 引号的单元格会加引号，`NULL` 复制为 `NULL`）。*范围选区组件不支持，待评估*
- [x] 过滤抽屉（`⌘F`）：多条规则、启停、列 / 操作符下拉、按列类型解析值（数字列里写字母会在该行报错而不是被猜成别的查询）、`between` / `in` / `is null`、Raw SQL 模式（只允许单个表达式，agent 通道整体禁用）、应用后回到第一页、状态栏 `Filters (N)` 徽标、已保存的规则随表恢复
- [~] 底部状态栏：过滤按钮、`Showing 1–300 of 14,200 · 12 ms`、分页大小。列可见性（`Columns (a/b)` 面板，随表保存）已做。
- [x] 外键单元格：点击 ↗ 跳转到目标表并自动带过滤条件
- [~] 验证 gpui-kit 能力缺口：
      已确认可用 —— 自定义表头 / 单元格渲染、单元格 / 行 / 列选区、列宽拖拽、列重排、固定列（`Column::fixed`）、虚拟滚动（M0 实测 100 万行 × 200 列 60fps）。
      **已确认不支持** —— 范围（矩形）选区。
      *待验证* —— 行号槽状态条与单元格覆盖层（M3 编辑需要）

**M1 验收标准** 与目前的实测状态

| 标准 | 状态 |
|---|---|
| 100 万行 SQLite 表首屏出数据 < 200ms | **服务层已达标**：首批 300 行 约 60–90 µs，`COUNT(*)` 6 ms，翻到偏移 90 万处 9 ms（release，`cargo test -p fg-service --release --test e2e -- --ignored --nocapture perf_`）。*界面渲染耗时未测* |
| 滚动 60fps、内存有上限 | 网格滚动 M0 已测（100 万行 × 200 列，59.9fps，内存 +<1MB/分钟）；带真实数据的整窗口滚动未测 |
| 切换标签、折叠侧栏、拖动分割线无明显卡顿 | 切换与折叠无阻塞（所有 IO 在后台）；分割线拖拽未做 |
| 全程仅键盘可完成：打开连接 → `⌘P` 打开表 → 过滤 → 翻页 | **部分**：`⌘P`、`⌘F`、标签与侧栏快捷键已有；启动页的键盘选择、抽屉里的 Tab 顺序、翻页快捷键未做 |
| 三端均能构建并启动（CI 绿） | 仅 macOS 实测。**真实启动已验证**：二进制起来后稳定运行、数据库文件创建（`FASTGRID_DATA_DIR` + `FASTGRID_MEMORY_KEYCHAIN` 开发开关）；Windows / Linux 未测 |

---

## M2 SQL 编辑器 + 一级安全模式（3w）

对应调研报告 §7、§8。无界面的策略与落盘已有；编辑器、执行快捷键、结果区、L1 确认框、补全、查询历史与安全模式切换已落地。**不要把本里程碑标成完成**：hover / 跳转定义、崩溃恢复、完整的连接级安全模式界面仍未做，视觉也没有人眼验证过。

无界面，已落地：

- [x] 执行策略：`plan(sql, kind, safe_mode)` 给出每条语句的分类、危险原因、是否需要确认，以及原文中的字节偏移。`OpenConnection::run` 在租会话前拒绝。每次尝试恰好一行历史（`Ok` / `Error` / `Cancelled` / `NotRun`）。非编辑器调用另写一行审计，身份是传入的 client id，不是固定标签。`RunOptions` 只能通过 `gui` / `gui_confirmed`（绑定该脚本的摘要）/ `agent(client)` 构造。
- [x] L1 拒绝逻辑：Alert 对危险语句要求确认；Strict 对每个可能写入的语句要求确认。只读（含未知安全模式）拒绝写，确认不能覆盖。`execute` 走同一分类且没有确认通道。`SafeMode::Strict` 的口令再认证**没有实现**。
- [x] 历史与审计落盘，以及 `Service` 上的 history / audit / documents / snippets、`exec_settings` / `set_exec_settings`、`drain_records`。审计拒绝重复 `AuditId`（含墓碑）。安全相关设置走严格读取，普通设置仍宽松。
- [x] `max_rows` 与语句超时在 `fg-service::run` 强制：`ExecSettings`、单次选项、连接 `timeouts.statement_secs`（仅超时）取最严；设置不能读或不能解密反序列化时失败关闭。截断会标出。SQLite 由服务按语句计时。

界面，已落地（行为有无界面测试；**画面没有被人眼验证过**）：

- [x] 编辑器：`component::input::EditorState` + Tree-sitter SQL 高亮（`language("sql")`，`crates/fg-app/tests/editor_grammar.rs` 钉住语法在运行时确实注册）、行号、多光标、查找 / 替换、撤销重做全部来自库。`crates/fg-app/src/views/query_tab.rs`。
- [x] 执行：`⌘↵` 运行光标所在语句、`⌘⇧↵` 运行全部、有选区则运行选区（`model::run_target` 决定，运行内容在事件被处理时才从缓冲区取，所以跑的就是屏幕上那串字）；`⌘.` 取消（`RunStream::canceller`）。`⌘T` / 顶栏 SQL 徽标新建查询标签。
- [x] 结果区：按驱动给的 `index` 分标签（`Result 1 · 3 rows` / `Result 2 · 7 affected`），截断在该标签与状态行都标出；状态行汇总行数 / 耗时 / 跑了几条语句；`Notice` 进消息区；语句跑成功但历史或审计没写进去时用 `record-error` 明确告知（不静默）。
- [x] 多类型标签：`WorkspaceView` 的标签内容改成 `TabContent::Table | Query`，会话能同时保存并恢复表标签与查询标签；查询标签绑定 `document`，改动 600ms 防抖落盘（`Service::save_document`），读不懂的草稿（新版本写的）只读展示且不回写。
- [x] L1 / L2 确认框：plan 要求确认时先弹内联确认区，展示语句 + 危险原因 +（生产连接时）PRODUCTION 标记；点「Run anyway」才用 `gui_confirmed(同一段 SQL)` 重跑。拒绝时什么都不发（不碰数据库、不写历史）。

界面与其余行为，仍未做：

- [x] 补全（经 `fg-lsp` 的作用域分析）：`i.` 解析到 `items` 是因为 `i` 被声明成它的别名，不是名字前缀碰巧对上。列 > 作用域内关系 > 表 > 关键字；`alias.` 只给该关系的列；CTE 与 JOIN 别名各自解析；大小写不敏感；同名不重推；上限 50 条。数据来自元数据缓存（schema 落地后逐表 `describe` 灌入，不在敲键时查库）。
- [x] **SQL language server `fg-lsp` + `fastgrid-cli lsp`**：进程层的完整实现——`wire.rs` 是 `Content-Length` 分帧（长度按**字节**算，UTF-16 位置换算在 `protocol.rs` 里做一次），`protocol.rs` 是纯函数式的「请求 → 响应」状态机（`initialize` / `initialized` / `shutdown` / `exit`、`didOpen` / `didChange` / `didClose`、`completion`、`diagnostic`、`publishDiagnostics`），`fg-cli` 只负责 stdin/stdout 的循环。客户端可以通过 `initializationOptions.fastgrid` 传 schema（`kind` + `tables[].columns[]`），进程才有列可补全。**进程级测试**（`crates/fg-cli/tests/lsp_process.rs`，7 个）真的 spawn 二进制、走管道：`initialize` 应答、别名解析出列、`didOpen` 推空诊断、畸形消息不断连、stdout 只有协议字节、`exit` 后正常退出。
- [x] **SQL 分析核心 `fg-lsp`**（补全与诊断的所在地，而不是编辑器侧）：`scope.rs` 解析一条语句在光标处能看见什么（`FROM` / `JOIN` / 逗号列表 / `AS` 别名 / 限定名 / CTE / 派生表，后出现的别名遮蔽先出现的，只有光标所在那条语句被分析）；`complete.rs` 在此基础上排序候选；`diagnostics.rs` 解决「错误属于哪条语句」——靠「产生了几个结果集」推断失败的是第几条，所以**多语句脚本现在也能正确下划线**。纯函数、无 IO，schema 由调用方通过 `Catalog` 提供。前缀匹配器已删除，不并存。
   - 已接入编辑器的 `CompletionProvider`；`HoverProvider` / `DefinitionProvider` 的挂点还在（`gpui_base::input::Lsp`），是后续增量。
   - **CTE 与派生表的列已解析**：递归分析子查询的 `SELECT` 列表（`SELECT *` 按它自己的 `FROM` 展开、`AS name` 改名、类型从内层表带出），显式列名列表优先；CTE 体里的表**不会**泄漏到外层作用域；递归 CTE 与 `AS MATERIALIZED` 都处理；嵌套超过 8 层就停止（`MAX_DEPTH`），不会爆栈。
- [x] 格式化 SQL（`⌥⇧F`）：`fg-sql::format_sql` 是纯函数——子句各占一行、括号内按层缩进并在逗号处换行、关键字统一大写、字符串 / 引号标识符 / 注释原样保留、语句之间空一行；幂等（`format_sql(format_sql(x)) == format_sql(x)`，有测试）。刻意保守：不做需要语法树的重新排版，宁可少格式化也不改语义。
- [x] 注释切换（`⌘/`）：`fg-sql::toggle_line_comments`，用 lexer 判断某行是否**真的**是行注释（`SELECT '--x'` 不会被当成注释），只给非空行加/去 `--`，保留缩进；整段选中时若已全注释则取消、否则全部注释（混合选择按其他编辑器的惯例全注释）。只做行注释，块注释不做。
- [x] 查询历史界面（`⌘Y` / 顶栏按钮）：右侧抽屉，按语句搜索、按结果着色（ok / error / cancelled / not run）、显示耗时与相对时间，点一条把语句填进当前查询标签。数据走 `Service::history`（加密历史记录），不另存一份。
- [x] 收藏片段界面（`⌘⇧S` 打开 / `⌘⇧K` 把当前语句存为片段）：右侧抽屉，按标题 / SQL / 标签搜索，点一条填进当前查询标签，可删除；读不懂的片段单独提示且不可编辑。存储走 `fg-store` 的加密 `snippet` 记录（M1.1 已勾选），界面不另存副本。
- [ ] 未保存内容的崩溃恢复：现在只有 600ms 防抖的 `save_document`，没有独立的崩溃日记。
- [x] EXPLAIN（`⌘⇧E`）：走同一条受检路径 `OpenConnection::run` 跑 `EXPLAIN QUERY PLAN …`（SQLite）/ `EXPLAIN …`（其他引擎），计划就是引擎返回的行，直接显示在结果区；因为被分类的是 `EXPLAIN` 本身（读），**只读连接也能解释一条写语句**，且这次尝试照样进历史。
- [x] 安全模式切换：顶栏锁形按钮按 Normal → Alert → Strict → ReadOnly 循环（只读时图标变实锁），tooltip 说明当前级别，改动立刻写回连接配置并关闭旧连接，所以下一条语句就按新级别执行。L1 / L2 确认对话框也在上面勾选。
- [ ] 完整的连接级安全模式设置界面（下拉、说明文字、按环境给建议值）。现在只有这个循环按钮；默认映射已在 `Environment::default_safe_mode`（生产与未知 → Alert）

~~已知 API 缺口：多语句脚本的错误无法归属~~ —— **已解决**，不需要驱动改动：`fg-lsp::diagnostic_for` 用「产生了几个结果集」推断失败的是第几条语句（服务按顺序执行、遇错即停），再把语句内偏移加上语句起点。测试：`fg-lsp` 的 `a_failure_in_the_third_statement_is_attributed_to_it`、`a_position_is_relative_to_the_statement_not_the_script`；界面上是 `a_multi_statement_failure_underlines_the_statement_that_failed`。

另外两条待办：确认框目前是标签内的内联区，不是模态；`⌘↵` 之后对象树不会因为 DDL 自动刷新（手动 `⌘R`）。

**验收**

- 在 10 万行结果上执行查询，UI 在流式接收时保持响应，可中途取消（执行与取消路径已实现且有测试；**十万行的界面表现未实测**，需要 `fixtures/sqlite/m1_test.db` 手工跑一次）
- 语句拆分与分类通过针对字符串、注释、嵌套引号的测试集（M1.0 已覆盖，`cargo test -p fg-sql`）
- L1 对「无 WHERE 的 DELETE」必定拦截（有测试）；只读模式下任何写语句都被拒绝（无界面部分已有测试；确认框也已实现并有 e2e：点确认前表里 3 行不变，点完才删）

无界面已经能量化的部分（本次核对，不代表 M2 或 M1 完成）：

- `cargo test -p fg-service --test run_policy --test policy_gaps`：`run_policy` 11 passed / 0 failed，`policy_gaps` 28 passed / 0 failed。覆盖 L1 / Strict / 只读拒绝、确认摘要不能换脚本重放、`max_rows` 截断、历史四种结果、审计只记非编辑器调用且使用 client id、启动 SQL 白名单、设置失败关闭、丢弃流仍记录。
- `cargo test -p fg-service --test foreign_executor`：3 passed。用**非 tokio 执行器**（普通线程 + 手搓 block_on）驱动 `run` / `next` / `collect`，钉住「GUI 的 executor 上不能有 tokio reactor 依赖」这条契约（这条是 e2e 先炸出来、再补的回归测试）。
- `cargo test -p fg-app --test query_tab_view --test query_tab_e2e`：`query_tab_view` 33 passed（无头驱动真实编辑器：`⌘↵` / `⌘⇧↵` / `⌘.`、选区优先、确认框出现与拒绝、结果分标签、截断标记、错误下划线、陈旧结果丢弃、读不懂的草稿只读），`query_tab_e2e` 4 passed（真实 SQLite + 加密存储 + Service + controller + 视图：跑查询进历史、Alert 下的 DELETE 点确认前不生效、ReadOnly 拒绝 UPDATE 且记 `NotRun`、草稿防抖落盘并可读回）。
- `cargo test --workspace`：570 passed / 0 failed / 2 ignored，exit 0。2 个 ignored 是钥匙串实测与百万行性能，不是失败。

---

## M2.5 MCP 只读 MVP（1w）— 已完成（待与真实客户端联调）

目的：尽早让 `fg-service` 被第二个调用方（外部 agent）验证，暴露抽象问题。
进程内 `RunOptions::agent` 不是引擎级只读租约（它只去掉确认并携带身份），所以传输层自己补上 [mcp-security.md](mcp-security.md) 列出的检查：
`fastgrid-cli mcp --stdio` 用 `ServiceOptions::force_read_only` 启动 Service，该进程里**每条连接**都以只读打开（引擎会话只读 + 语句分类只读，覆盖仅存在内存、不写回存储）。

- [x] `fastgrid-cli mcp --stdio`：无头运行，与 GUI 共用同一个加密存储（`FASTGRID_DATA_DIR` 可覆盖）。
      **没有用 `rmcp`**：需要的只有 `initialize` / `ping` / `tools/list` / `tools/call` 四个请求，手写 JSON-RPC 与 `fg-lsp` 同一思路，策略检查集中在一处，也避免为它引入一整棵依赖树
- [x] 工具：`list_connections`、`list_namespaces`、`list_tables`、`describe_table`、`run_query`（强制只读 + 行数上限 + 超时；单元格里的二进制只给大小；回复超过 256 KiB 截断并说明）
- [x] 标为生产（含未知环境）的连接默认不暴露；隐藏的连接与不存在的连接回复完全一样，agent 无法探测
- [x] 行数上限：默认 200，连接的 `mcp.max_rows` 与硬上限 5000 取最小，agent 只能调低不能调高；超时默认 30s，取连接的 `mcp.statement_timeout_secs`
- [x] 审计：`run_query` 由 `run` 自己记一行（含被拒绝的）；其余工具和在 `run` 之前被拒绝的调用由工具层记一行；审计写不进去时调用失败而不是放行
- [ ] 与 Claude Desktop / Claude Code 联调并记录配置示例（**还没有真实客户端跑过**，目前验证的是协议层的进程测试和 `fg-mcp` 的集成测试）

**验收**：外部 agent 能完成「列出表 → 查看结构 → 查询」（`fg-mcp/tests/mcp.rs::an_agent_can_list_describe_and_query`）；
写入被拒绝，8 种写法（含多语句、可写 CTE、`PRAGMA`、`ATTACH`）逐条测试，表内容不变；
变异验证：把 `force_read_only` 关掉，`writes_are_refused_and_nothing_changes` 必然失败。
引擎层只读本身沿用 SQLite 驱动已有的只读会话测试；PG / MySQL 的引擎层只读随 M4 / M5。

---

## M3 暂存变更与数据编辑（4w）

对应调研报告 §1.2、§4.4.4、§4.6、§5.2、§12.2。这是体验上最接近 TablePlus 的核心。

- [ ] Changeset 覆盖层：基线快照不可变；`modified_cells` / `inserted_rows` / `deleted_row_ids`；
      行标识用主键，无主键的表只读并说明原因
- [ ] 渲染合成：修改（黄）/ 新增（绿）/ 删除（红 + 删除线）；行号槽左侧 3px 状态条；`O(1)` 查表，不影响滚动性能
- [ ] 行内编辑：双击 / `Enter` / 直接键入；`Tab` / `⇧Tab` / `Enter` / `Esc` 流转；枚举与外键字段的下拉
- [ ] 行操作：插入、克隆（自增主键清空）、删除标记 / 恢复；`Set NULL` / `Set Empty` / `Set Default`；单点还原
- [ ] 撤销 / 重做栈（纯内存）
- [~] DML 编译：`fg-sql::compile_changes` 基于 `Dialect` 生成 `INSERT` / `UPDATE` / `DELETE`（全部带参数，标识符经方言引用）；`UPDATE` / `DELETE` 只按主键定位并声明 `expect_rows = 1`，空键 / NULL 键 / 重复列被拒绝；`SET c = DEFAULT` 在 SQLite 上返回 `Unsupported`（方言能力 `supports_default_keyword`）。*用 `RETURNING` 或回读获取默认值和自增主键待做；基于属性的测试待做*
- [~] 提交：`OpenConnection::commit_changes`，单会话单事务；只读连接拒绝；无主键表、键不等于完整主键的更新 / 删除在租会话前拒绝；任一语句出错或影响行数不为 1 → 整体 `ROLLBACK`，错误指出第几条（`fg-service/tests/commit.rs` 5 个，去掉回滚后 2 个必然失败）。*不写历史行；L1 批量确认由界面负责；1000 条混合变更的验收数据待测*
- [ ] 顶栏暂存胶囊（计数）、预览 SQL 弹窗（`⌘⇧P`）、提交（`⌘S`，单事务）、放弃全部（二次确认）
- [ ] 提交失败：整体回滚并定位到出错的那一行
- [ ] 行详情检查器（`Space`）：Text / JSON 模式，JSON 校验与格式化；与网格单元格双向同步
- [ ] 可编辑的查询结果：基于 `ColumnMeta.origin` 与主键检测；无法安全定位则只读
- [ ] 安全模式与提交联动：L1 在批量提交前确认

**验收**

- 混合 1000 个修改 / 新增 / 删除后提交：单事务、语句正确；任一失败则全部回滚（测试覆盖）
- 在 100 万行表上存在暂存变更时滚动仍为 60fps
- 撤销 / 重做 / 放弃不产生任何数据库往返
- 对每类 DML 有基于属性的测试：生成的 SQL 执行后，表内容与预期变更集一致

---

## M4 PostgreSQL（3w）

在已验证的抽象与界面上新增驱动。

- [ ] `fg-driver/src/postgres/`：`tokio-postgres` + rustls；连接池由 `fg-service` 统一管理
- [ ] 异步流式发送辅助函数（补齐 M0 遗留的「已知缺口」），复用 `Batcher`
- [ ] 取消：cancel token；只读：`SET TRANSACTION READ ONLY` / `default_transaction_read_only`
- [ ] 元数据：`pg_catalog`（schema、表、视图、物化视图、列、主键、索引、外键、注释、行数估算 `reltuples`）；`pg_get_*` 生成 DDL
- [ ] 类型解码矩阵：整数 / 浮点 / `numeric`（保持精度）/ `text` / `bytea` / `bool` / `date` / `time` / `timestamp(tz)` / `interval` /
      `uuid` / `json(b)` / 数组 / 枚举 / 范围 / `inet` / `bit` / 几何 / 未知类型（走 `Value::Other`）
- [ ] `fg-service`：SSH 隧道（`russh`，密码 / 私钥 / agent）与 TLS（Disable ~ Verify-Full、客户端证书）—— 驱动无感知
- [ ] 连接配置界面（网络型）：host / port / user / password / database、环境色标、SSL、SSH、连接 URL 粘贴自动解析、测试连接（显示延迟与精确错误）
- [ ] 数据库切换（`⌘K`）：PG 通过重连实现
- [ ] 方言细节：`$n` 占位符、`RETURNING`、事务性 DDL、`EXPLAIN`
- [ ] 共享的行为测试（与 SQLite 同一组场景）通过 testcontainers 在 PG 12–17 上运行

**验收**

- 行为测试在 PG 12 / 14 / 16 / 17 全部通过
- 类型矩阵每种类型都有「读 → 显示 → 编辑 → 提交」的往返测试
- 通过 SSH 隧道 + `verify-full` TLS 能连接并浏览；取消一个 `pg_sleep(60)` 在 1 秒内生效
- 网格性能指标与 SQLite 相当（同等行数下滚动 60fps）

---

## M5 MySQL / MariaDB（2.5w）

- [ ] `fg-driver/src/mysql/`：`mysql_async` + rustls
- [ ] 取消：旁路连接执行 `KILL QUERY`；只读：`START TRANSACTION READ ONLY`
- [ ] 元数据：`information_schema`；`SHOW CREATE TABLE`；数据库即命名空间（`list_databases` == `list_namespaces`，`USE` 切换无需重连）
- [ ] 类型解码：有符号 / 无符号整数、`decimal`、`tinyint(1)` 布尔约定、`enum` / `set`、`json`、`bit`、
      日期时间（含 `0000-00-00` 零值日期）、`blob` 系列、字符集与排序规则
- [ ] 多语句与多结果集（`CLIENT_MULTI_STATEMENTS`）
- [ ] 方言细节：反引号、`?` 占位符、`LIMIT` 写法、无 `RETURNING`（MariaDB 有）→ 回读策略；DDL 隐式提交的提示
- [ ] 复用 M4 的 SSH / TLS / 连接界面
- [ ] 行为测试在 MySQL 5.7 / 8.0 / 8.4 与 MariaDB 10.6 / 11 上通过

**验收**

- 与 M4 相同的行为测试集与类型往返测试全部通过
- 无符号 `bigint` 最大值、零值日期、`utf8mb4` 表情字符的往返无损
- 在一次提交中执行 DDL 时，UI 明确提示「MySQL 的 DDL 无法回滚」

---

## M6 表结构管理 + 导入导出（3w）

对应调研报告 §6、§9。

- [ ] 数据 / 结构双模切换（`⌘⌃[` / `⌘⌃]`）
- [ ] 结构查看：列、索引、外键、DDL 原文
- [ ] 结构编辑（进入同一套暂存机制）：增删改列、索引、外键；按方言生成 `ALTER TABLE` 并预览。
      SQLite 的限制（需要重建表）要明确处理并向用户说明
- [ ] 创建 / 删除 / 重命名表
- [ ] 导出向导（`⌘⇧E`）：CSV / JSON / SQL；表或查询结果；仅结构 / 仅数据；gzip；**流式，内存占用与行数无关**
- [ ] 加密备份界面：导出时设置密码（范围：仅连接 / 全部；是否包含密码），导入时输入密码；错误密码 / 非备份文件 / 新版本文件给出明确提示（后端已实现）
- [ ] CSV 导入：预览前 10 行、列映射矩阵、错误策略（中止 / 跳过并记录）、批量事务
- [ ] 反向引用：在主键单元格上列出全库引用它的子表，并一键带条件打开（基于 `list_relations`）

**验收**

- 导出 1000 万行表到 CSV：内存峰值 < 200MB（有基准测试）
- 三个引擎上的列增删改往返测试通过，生成的 DDL 有快照测试
- 导入 100 万行 CSV 成功，错误行被记录

---

## M7 MCP 完整版（3w）

详见 [mcp-security.md](mcp-security.md)。

- [ ] 本地 Streamable HTTP：仅监听 127.0.0.1，随机 bearer token，GUI 中可轮换
- [ ] 复用 GUI 已建立的会话与连接（可选）
- [ ] 工具补全：`explain`、`sample_rows`、`list_relations`、`find_references`（反向引用）、受控的 `execute_write`
- [ ] 资源：按连接导出 schema 快照
- [ ] 写入门控：连接级开关 + 每条语句需 GUI 确认；无头模式默认拒绝；超时自动拒绝
- [ ] 统一策略引擎：GUI 的安全模式与 MCP 的限制合并到 `fg-service` 一处实现（只读、行数 / 超时上限、生产连接可见性）
- [ ] 审计日志：落本地，GUI 内可查看 / 过滤 / 导出（agent、连接、语句、结果、耗时）
- [ ] 提示注入防护：工具返回内容明确标注为「数据」；超长结果截断并提示

**验收**

- 安全测试集：试图绕过只读（多语句、注释、CTE 写入、`SELECT ... INTO`、存储过程）全部被拒绝
- 写入未经 GUI 确认时一定失败；审计日志与实际执行一一对应
- 用 Claude Desktop / Cursor / Claude Code 分别完成一次端到端演示

---

## M8 打磨与发布（3w）

- [ ] 快捷键体系定稿（见 [shortcuts.md](shortcuts.md)）与命令面板；Escape 逐级退出
- [ ] 崩溃恢复、日志与诊断包导出、遥测（默认关闭）
- [ ] 无障碍与键盘可达性检查；中英文界面
- [ ] 三端打包：macOS（签名 + 公证，dmg）、Windows（msi，签名）、Linux（AppImage + deb）；自动更新
- [ ] 性能回归进入 CI（解码基准、滚动 / 内存固定数据集）
- [ ] 文档站：快速开始、各引擎连接指南、MCP 配置、贡献指南、驱动开发指南
- [ ] Beta 发布 → 收集反馈 → v1.0

**发布门槛**

- 三端 CI 全绿；三个引擎的行为测试全绿
- 无已知的数据丢失 / 误写类缺陷
- 第一次启动到浏览第一张表 < 1 分钟（有真人试用记录）

---

## M9 加密跨端同步（4w，v1.x）

设计见 [encryption-and-sync.md](encryption-and-sync.md)。**不运营服务器**，使用用户自己的存储。本地加密、记录版本号、墓碑、脏标记、合并规则、导入导出所用的同一套原语在 M1 已经实现并测试，M9 只补后端与协议。

- [ ] 存储后端抽象：`list / get / put(可选条件写) / delete`；实现 **本地文件夹**（顺带支持 iCloud Drive / Dropbox / Syncthing）、**S3 兼容**、**WebDAV**
- [ ] 同步密码：设置同步密码 → Argon2id 包装随机同步密钥存放在远端 `keys/`；修改密码只重新包装；新设备输入密码入网；「重置同步」
- [ ] 协议：每台设备只写自己的 `devices/<id>/seg-N`（同步密钥加密、不可变）；本地 `sync_state` 记录游标；定期全量快照与旧分段清理
- [ ] 推送 / 拉取：`dirty_records` → 分段 → `mark_clean`；拉取分段 → `apply_remote`；失败可重试、可中断，不丢改动
- [ ] 冲突：`rev` → `updated_at` → `device_id` 的确定性合并；文本草稿冲突保留「冲突副本」；已删除记录不被旧版本复活
- [ ] 保留策略：历史 / 审计的保留期统一按时间判定，避免被裁剪的条目又被其他设备拉回
- [ ] 设置界面：开关、选择后端与凭据（后端凭据作为 secret，默认只存本机）、同步状态与最近错误、立即同步
- [ ] 设备管理：列出已知设备；撤销设备（轮换同步密钥）
- [ ] 测试：多设备模拟（交错写入、离线后合并、同时删除与修改、时钟偏差、网络中断）；用内存后端和本地文件夹后端做确定性测试；S3（MinIO）与 WebDAV 用容器做集成测试

**验收**

- 两台设备离线各自修改后联网，数据收敛一致，且与操作顺序无关（属性测试）
- 存储方的所有对象中搜不到任何明文（与本地加密的回归测试同一套方法）
- 修改同步密码后，其他设备用新密码入网，旧密码失效，远端数据未被整体重写

---

## 贯穿所有里程碑的要求

| 方面 | 要求 |
|---|---|
| 性能预算 | 首屏 < 200ms；滚动 60fps；UI 线程不做 IO；结果集流式，内存有上限 |
| 稳定性 | 生产路径不使用 `unwrap` / `expect`；所有错误可展示且不崩溃；长任务可取消 |
| 测试 | 每个驱动跑同一组行为测试；解码做属性测试；关键 UI 流程用 `gpui_kit::test` |
| 安全 | 所有连接数据加密落盘，钥匙串只存设备密钥；日志不含凭据；MCP 默认只读 |
| 跨平台 | 每个里程碑结束前在三端各构建一次，不攒到最后 |
| 文档 | 抽象变更必须同步 `driver-abstraction.md`；安全相关变更同步 `mcp-security.md` |

## 一期明确不做

其他数据库引擎、托管的云服务与账号体系（M9 的同步使用自带存储，不需要账号）、团队协作、插件系统、应用内 AI 对话（NL2SQL，计划二期）、
分屏与标签拖出为新窗口、图片 / 日期选择等检查器模式、分区与触发器管理、用户与权限管理、ER 图、数据对比。

## 主要风险

| 风险 | 影响 | 应对 |
|---|---|---|
| gpui-kit 的 Table 缺少范围选区 / 覆盖层 / 状态条等能力 | M1.3、M3 返工 | M1.3 开头先逐项验证；必要时向上游贡献或在本仓库封装 |
| 类型兼容的长尾问题（尤其 MySQL 零值日期、PG 范围 / 几何） | 数据显示错误甚至误写 | 类型往返测试矩阵；未知类型一律走 `Value::Other` 且只读 |
| 暂存提交的行标识（无主键、复合主键、主键被修改） | 误改数据 | 无主键只读；提交前校验影响行数 == 1，否则回滚 |
| 三端差异（字体渲染、钥匙串、打包签名） | M8 集中爆雷 | 每个里程碑三端构建；M1 起就接钥匙串 |
| 钥匙串丢失且无同步 / 无备份时数据不可恢复 | 用户丢数据 | 首次启动引导导出加密备份；`KeyUnavailable` 给出明确恢复路径；不修改原文件、不覆盖旧密钥 |
| MCP 写入是最大风险面 | 数据被 agent 误改 | 门控 + 双层只读 + 审计 + 专项安全测试，不允许绕过 |
| 排期乐观 | 延期 | 每个里程碑末尾复盘并重估；「不做」清单严格执行 |
