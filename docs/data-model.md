# 数据模型与演进规则

目标：**前期把需要持久化、需要对外暴露的结构设计完整，避免以后反复做数据迁移。**

迁移的成本有两种：

- **内存里的类型**（`Value`、`ColumnMeta`……）改起来只是重构，编译器会帮忙；
- **已经落盘或对外的格式**（本地存储、导出文件、同步日志、MCP 返回值）一旦发布就有用户数据，改起来要写迁移。

所以下面的规则主要针对后者。

## 1. 存储：一张通用的 `record` 表

[`fg-store/migrations/0001_init.sql`](../crates/fg-store/migrations/0001_init.sql) 只有三张表：`meta`、`record`、`sync_state`。用户产生的所有数据都是 `record` 里的一行：

```
(kind, id) 主键 · parent_id · sort_ts · rev · device_id · updated_at · deleted_at · dirty · key_id · enc
```

新增一种数据 = 新增一个 `kind` 字符串，**不改表结构**。加密、墓碑、版本号、同步、导入导出对所有种类通用。

| kind | id | parent_id | 同步 | 说明 |
|---|---|---|---|---|
| `connection` | UUIDv7 | — | ✓ | `ConnectionConfig` |
| `group` | UUIDv7 | — | ✓ | `ConnectionGroup` |
| `secret` | `<连接ID>:<种类>` | 连接 | ✓ | 密码、SSH 口令（`SecretRecord`） |
| `snippet` | UUIDv7 | 连接（可空，空则全局） | ✓ | 片段。明文 `sort_ts` = `updated_at`。可读行的列表顺序是载荷里的 `sort_order` 升序，再 `updated_at` 降序，不靠 SQL 排这一列 |
| `document` | UUIDv7 | 连接 | ✓ | 编辑器草稿。`sort_ts` = `updated_at`（调用方拥有时间戳，存储不改写） |
| `journal` | UUIDv7（与草稿同 id） | 连接 | ✗ | 崩溃日记：草稿的最新未保存内容。每次编辑即写、保存成功后清；与 `document` 同载荷、不同 kind。本地专用，不同步不导出。并发写入以 `updated_at` 严格更新者胜，写读同一锁内完成 |
| `history` | UUIDv7 | 连接 | ✓ | 查询历史，`sort_ts` = 执行时间（`executed_at`） |
| `audit` | UUIDv7 | 连接（可空） | ✓ | 审计日志，只追加，连接删除后保留。`sort_ts` = `at`。插入拒绝重复 id，含已墓碑的 id |
| `workspace` | `<连接ID>:<设备ID>` | 连接 | ✓ | 每台设备各自的布局 |
| `table_view` | 随机 UUID | 连接 | ✓ | 过滤、排序、列宽；表名在密文里 |
| `setting` | 设置组名 | — | ✓ | 主题、网格、编辑器、快捷键、`exec`（行数上限、语句超时、历史保留条数）等 |
| `mcp_client` | UUIDv7 | — | ✗ | 单设备的 MCP 令牌哈希 |

规则：

- **ID 一律由客户端生成**（UUIDv7，按时间有序），绝不用自增整数。多设备产生的记录才能合并而不冲突；历史与审计的 ID 也是 UUID，所以能直接并集。
- **时间一律是 Unix 毫秒（UTC）**，类型 `UnixMillis`。
- **删除一律是墓碑**（`deleted_at` + 清空 `enc`），以便同步；清理在压缩时进行。
- **明文列只放结构信息**，其余全部在 `enc` 里（见 [encryption-and-sync.md](encryption-and-sync.md)）。

## 2. 载荷（`enc` 内部的 JSON）怎么演进

载荷是 `fg-core` 里的 serde 类型。五条规则（实现与测试在 `fg-core/src/compat.rs`）：

1. **只增不改**：新增字段一律带 `#[serde(default)]`；不重命名、不删除，只能弃用。
2. **不用 `deny_unknown_fields`**：新版本写的多余字段，旧版本读取时忽略。
3. **单元枚举带 `Unknown` 变体**（`#[serde(other)]`）：新版本写的新值在旧版本降级，而不是整条记录失败。`Unknown` 的语义取**保守方向**（未知环境按生产对待，未知安全模式按只读对待）。
4. **带数据的枚举（`ConnectionParams`、`TabKind`）无法降级**：包含它们的列表用 `lenient_vec`，无法解析的元素被跳过而不是丢掉整个列表。
5. **做不到以上四点的变更才需要迁移**（见 §4）。

读到「看不懂」的记录时：

- 记录**原样保留**，在 UI 里显示为「不可用（由新版本创建）」，不会被静默删除或覆盖（`Loaded::Unreadable`，含解密后的原始 JSON）。
- 导出备份时同样不会丢。
- `document` / `snippet` 把「没有这条记录」（从未写入或已墓碑）和「有记录但本版本读不懂」分开：`Store::document` 对缺失返回 `None`，对不可读返回 `StoredDocument::Unreadable`；`snippets` 同样列出 `StoredSnippet::Unreadable`，不把它当成不存在。`save_document` / `save_snippet` 用 `put_json_if_readable`，检查和写入在同一事务里，拒绝覆盖不可读行。
- 连接删除会墓碑其子记录：`secret`、连接作用域的 `snippet`、`document`、`history`、`workspace`、`table_view`。`parent_id` 为空的全局片段不在其中。`audit` 不级联删除。
- 普通设置（外观、网格）走 `setting()`：缺失或不可读都回默认值，以免坏行阻止启动，且不覆盖原行。安全相关的 `ExecSettings`（键 `exec`）走 `setting_strict`：缺失仍是默认值；不可读则 `Error::Storage`，调用方不得把它当成默认的 100_000 行上限。

## 3. 其他面向外部的结构

| 结构 | 稳定性约定 |
|---|---|
| `Value` / `ColumnMeta` / `DataType` / `Event` | `#[non_exhaustive]`，只增不改。`Value` 自带 `Other{type_name,text}` 兜底，未知类型不丢数据；大字段是 `Truncated`（见 `ExecOptions::max_cell_bytes`） |
| `ObjectKind` / `TypeCategory` / `FilterOp` 等 | `#[non_exhaustive]` + `Unknown`/`Other` |
| 元数据结构（`ColumnInfo`、`IndexInfo`、`TableDetail`……） | 字段全部带默认值；引擎私有属性放 `extra: BTreeMap`，被两个以上引擎需要时再提升为正式字段 |
| 备份文件 | `format_version`，新版本写的备份旧版本拒绝导入并说明原因 |
| 加密封装 | 版本号 + 算法号 + `key_id` 都在认证头里 |
| MCP 返回值（M2.5、M7） | 沿用上述类型与 serde 表示；变更需同步 `mcp-security.md` |
| 钥匙串条目 | 名称 `device-key:<store_id>` 固定不变 |

## 4. 迁移规则（`fg-store/src/migrations.rs`）

- 前向迁移，版本号存在 `PRAGMA user_version`，每个迁移与版本号更新在**同一个事务**里。
- **发布过的迁移永远不修改**：测试会固定它的校验和。有错就新增一个迁移修正。校验和算前会剔除 `\r`，
  所以 Windows 检出 CRLF 不影响它（文本文件同时由 `.gitattributes` 钉成 LF）。
- 有待执行的迁移时，先把文件备份为 `store.db.bak-v<N>`（`VACUUM INTO`，WAL 下也一致）。
- 存储版本**高于**本程序 → 拒绝打开并原样保留，旧版本不猜测新格式。
- 迁移失败 → 整体回滚，停在原版本（有测试）。
- 载荷是加密的，SQL 无法改写；需要改载荷形状的迁移用 `Step::Code`，在迁移里用设备密钥解密 → 转换 → 重新加密。这类迁移必须带上一版本的固定样本测试。
- **优先加性变更**（新增 `kind`、新增 JSON 字段）：不需要迁移。因为存储是通用 `record` 表，大多数需求属于这一类。

## 5. 什么时候才需要真正的迁移

- 修改明文列的含义（比如 `sort_ts` 改用别的时间基准）；
- 把一个 `kind` 拆成两个或合并；
- 改变 ID 的构造方式；
- 更换加密算法或密钥层级（会同时升级封装格式的版本号）；
- 载荷里重命名/重构字段。

遇到这些情况再写迁移；其余情况都应该能用「加字段 / 加 kind」解决。
