# TablePlus 全量交互设计与界面布局深度调研报告

> **目标与背景**：本调研报告旨在全方位拆解现代顶级原生数据库客户端 **TablePlus** 的全部交互设计、界面空间拓扑、几何尺寸规范、组件微交互细节、安全防护机制以及全量快捷键体系，为基于 Rust + GPUI 构建的现代化、高性能数据库客户端 **FastGrid** 提供工业级的交互设计规格说明书（UX Spec）与架构落地方案。

---

## 目录
1. [核心设计哲学与交互心智模型](#1-核心设计哲学与交互心智模型)
2. [工作区与连接管理交互](#2-工作区与连接管理交互)
3. [主界面空间拓扑、几何尺寸与分栏系统](#3-主界面空间拓扑几何尺寸与分栏系统)
4. [核心区域组件布局与微交互全景拆解](#4-核心区域组件布局与微交互全景拆解)
   - 4.1 [统一顶栏与环境感知胶囊 (Unified Header & Environmental Pill)](#41-统一顶栏与环境感知胶囊-unified-header--environmental-pill)
   - 4.2 [多标签栏系统与多窗格拆分交互 (Tab Strip & Split Panes)](#42-多标签栏系统与多窗格拆分交互-tab-strip--split-panes)
   - 4.3 [左侧对象树高密度布局与导航 (Schema Browser)](#43-左侧对象树高密度布局与导航-schema-browser)
   - 4.4 [数据网格微观几何与渲染流水线 (Data Grid Micro-Geometry)](#44-数据网格微观几何与渲染流水线-data-grid-micro-geometry)
   - 4.5 [抽屉式过滤栏与底部停靠状态栏 (Filter Drawer & Docked Status Bar)](#45-抽屉式过滤栏与底部停靠状态栏-filter-drawer--docked-status-bar)
   - 4.6 [右侧行详情检查器抽屉 (Row Detail Inspector)](#46-右侧行详情检查器抽屉-row-detail-inspector)
   - 4.7 [悬浮卡片、对话框与无模态弹窗 (Modals, Popovers & Dialogs)](#47-悬浮卡片对话框与无模态弹窗-modals-popovers--dialogs)
5. [数据网格核心交互与编辑状态流转](#5-数据网格核心交互与编辑状态流转)
6. [表结构编辑器 (Structure Editor) 交互](#6-表结构编辑器-structure-editor-交互)
7. [SQL 查询编辑器 (Query Editor) 交互](#7-sql-查询编辑器-query-editor-交互)
8. [环境标定与安全生产防护 (Safe Mode)](#8-环境标定与安全生产防护-safe-mode)
9. [导入与导出向导交互 (Import & Export)](#9-导入与导出向导交互-import--export)
10. [视觉设计系统规范 (Design Tokens & Typography)](#10-视觉设计系统规范-design-tokens--typography)
11. [TablePlus 全量快捷键映射表 (macOS / Win / Linux)](#11-tableplus-全量快捷键映射表-macos--win--linux)
12. [对 FastGrid (Rust + GPUI) 的落地方案与组件树蓝图](#12-对-fastgrid-rust--gpui-的落地方案与组件树蓝图)

---

## 1. 核心设计哲学与交互心智模型

TablePlus 在现代数据库客户端中独树一帜，其成功的底层逻辑建立在以下几个关键心智模型之上：

```mermaid
flowchart TD
    A["原生瞬时响应 (Native Performance)"] --> D["流畅度 (60/120 FPS 无感知延迟)"]
    B["暂存态与显式提交 (Pending Changes Buffer)"] --> E["绝对可控 (防误触 / 可视化 Diff)"]
    C["全键盘驱动工作流 (Keyboard-First Flow)"] --> F["极高效率 (双手不离键盘)"]
    G["上下文双模平滑切换 (Data ⇋ Structure)"] --> D
    H["生产环境安全拦截 (Safe Mode & Environment Badges)"] --> E
```

### 1.1 原生瞬时响应（Native & Instant）
* **拒绝 Web/Electron 技术栈的拖沓**：macOS 采用 Objective-C/Swift (Cocoa/AppKit)，Windows 采用 C#/WPF，Linux 采用 C++/GTK。UI 渲染始终锁定在 60/120fps，点击与键盘输入无延迟反馈。
* **分片延迟加载（Lazy Chunking）**：庞大的 Schema 与百万级数据表不会阻塞 UI 主线程，网格默认按视口分片（如 300 条），统计总行数（`COUNT(*)`）后台异步执行。

### 1.2 暂存态与显式提交（Pending Changes & Staging Buffer）
* **与传统“失去焦点即保存”的区别**：传统工具在离开单元格时立刻下发 `UPDATE`，不仅极易由于手滑误改生产数据，还会造成繁碎的网络调用与难以撤回。
* **Git 式 Staging 机制**：
  * UI 上所有行修改、新建、删除以及表结构变更均暂存到 **本地内存变更区（Changeset Buffer）**。
  * 强烈的色彩分级标识（黄色=修改，绿色=新增，红色=删除）。
  * 具备单点撤销（Revert）、全盘放弃（Discard）与 **SQL 差异预览（Preview SQL）**，最终由用户显式按下 `⌘ + S` 批量落库。

### 1.3 键盘第一优先（Keyboard-First Architecture）
* 双手不离键盘即可完成：打开连接、模糊查找表（`⌘ + P`）、行/列快速过滤（`⌘ + F`）、行内编辑（`Tab`/`Enter`）、快速预览侧边栏（`Space`）、外键跳转、SQL 编辑与执行（`⌘ + Return`）。

### 1.4 上下文双模一体化（Data ⇋ Structure Dual-Mode）
* 数据网格视图与表结构定义视图共存于同一个 Tab 下，通过 `⌘ + ⌃ + [` 与 `⌘ + ⌃ + ]` 极速切换，保持用户心智处于当前实体，而无需在新 Tab 中繁琐寻找定义。

---

## 2. 工作区与连接管理交互

```
+-------------------------------------------------------------------------+
| TablePlus - Connections                                     [🔍 Filter] |
+-------------------------------------------------------------------------+
| [ + Create a new connection... ]                                        |
|                                                                         |
| 📁 Production (2)                                                        |
|   🔴 [MySQL] Primary-DB-Cluster (prod.aws.com:3306)                     |
|   🔴 [PG]    Billing-Service (pg-prod.internal:5432)                    |
|                                                                         |
| 📁 Staging / Test (3)                                                   |
|   🟡 [PG]    Staging-API (10.0.1.25:5432)                                |
|   🔵 [Redis] Cache-Staging (10.0.1.30:6379)                             |
|                                                                         |
| 📁 Local (1)                                                            |
|   🟢 [SQLite] ~/code/fastgrid/dev.db                                    |
+-------------------------------------------------------------------------+
```

### 2.1 启动器 / 连接列表页（Launcher Window）
1. **视觉布局**：
   - 扁平现代卡片列表，支持分组目录（Folders / Groups），按项目、环境（Production, Staging, Local）折叠与展开。
   - 每个连接卡片展示：数据库引擎 Logo、连接别名、主机地址与端口、环境色标圆点、最后访问时间。
2. **分组与拖拽**：
   - 支持直接拖拽连接卡片调整顺序或移入/移出文件夹。
   - 右键上下文菜单：`Connect`、`Edit Connection...`、`Duplicate`、`Delete`、`Export Connection (Encrypted)`、`Copy Connection URL`。
3. **模糊搜索**：顶部搜索框即时过滤连接名、主机名或数据库类型。
4. **快捷启动**：键盘上下键选中连接，按下 `Return` 即可直接建立连接并唤起工作区窗口。

### 2.2 连接配置弹窗（Connection Config Modal）
1. **引擎选择器**：提供直观的数据库图标网格（PostgreSQL, MySQL, SQLite, Redis, ClickHouse, MariaDB, SQL Server, CockroachDB, Snowflake, Cassandra, DuckDB 等）。
2. **连接凭证配置区**：
   - **Name**：自定义连接展示名称。
   - **Environment Tag**：环境色标选择器（Red, Orange, Yellow, Green, Blue, Purple, Gray），决定主窗口的高亮警示基调。
   - **Host / Port / User / Password / Database / Socket**。
   - **Connect via SSH Tunnel**（高频企业级交互）：
     - 勾选即展开：SSH Host, SSH Port, SSH User, Authentication Type（Password / Private Key / SSH Agent）。
     - 支持导入 `id_rsa` / `id_ed25519` 私钥文件及 Passphrase。
   - **SSL 配置**：SSL Mode（Disable, Prefer, Require, Verify-CA, Verify-Full），支持加载 Client Certificate、Client Key、CA Certificate。
3. **连接字符串直接解析（URL Parsing）**：支持从剪贴板粘贴诸如 `postgresql://user:pass@host:5432/dbname?sslmode=require`，TablePlus 会自动将 URL 拆解并填入各对应输入框中。
4. **操作反馈控制**：
   - **Test Button**：点击后显示旋转加载动画，测试成功显示绿色勾号并标注延迟毫秒数（如 `OK (18ms)`）；若失败，显示精确错误详情（如 SSH 握手失败、密码错误、端口不通）。
   - **Save** / **Connect**：保存连接或直接发起连接。

---

## 3. 主界面空间拓扑、几何尺寸与分栏系统

TablePlus 采用统一的主窗口体系，所有核心面板严格基于直角边界与高响应分割器构建，杜绝多窗口凌乱跳转。

```
+-----------------------------------------------------------------------------------------------------------------------+
| 38px [🔴][🟡][🟢] [☰] [🕒] [⚡] | [🔴 PROD: fastgrid_prod ▼] | [● 2 Updates ● 1 Del] [Preview] [Commit] [🔒] [🔍]  |  Unified Titlebar
+-------------------+-----------------------------------------------------------------------------------+---------------+
| SCHEMA BROWSER    | [Tab 1: users (Data)] [Tab 2: orders] [Tab 3: SQL 1] [+]                          | ROW INSPECTOR |  Tab Strip (32px)
| Default: 220px    +-----------------------------------------------------------------------------------+ Default:320px |
| Min: 140px        | #   | ☰ ID   | ☰ Name           | ☰ Email          | ☰ Role_ID [↗] | ☰ Status    | Min: 240px    |  Header (28px)
| Max: 450px        +-----+--------+------------------+------------------+---------------+-------------+ Max: 600px    |
|                   | 1   | 1001   | Alice Johnson    | alice@corp.com   | 10 [↗]        | active      |               |
| 📁 TABLES (18)     | 2 🟡| 1002   | Bob Smith 🟡     | bob_new@corp.com | 20 [↗]        | pending     | [Text | JSON] |
|   users           | 3 🔴| 1003   | Charlie Brown 🔴 | charlie@test.com | 10 [↗]        | banned      | Field: data   |  Data Grid
|   orders          | + 🟢| 1004 🟢| [New Record...]  |                  |               |             | Type: jsonb   |  Row Height:
|   products        +-----------------------------------------------------------------------------------+               |  Compact: 22px
| 📁 VIEWS (4)       | [✓] [status ▼] [ = ▼ ] ['active']  [+]                       | [Raw SQL Mode]     | {             |  Standard:26px
|   v_active_users  +-----------------------------------------------------------------------------------+   "role": 1   |  Filter Drawer
|                   | [Filters]  [Columns (5/8)]  [ 300 ▼ ]  Showing 1-300 of 14,200    | [Export]  [▤] | }             |  Status (28px)
+-------------------+-----------------------------------------------------------------------------------+---------------+
```

### 3.1 空间几何与尺寸约束规范
* **窗口边界约束**：
  - 最小尺寸限制：`800px × 500px`；
  - 默认启动尺寸：`1280px × 800px` 或记忆上一次关闭时的屏幕坐标与分辨率。
* **分栏宽度与弹性系数**：
  - **左侧边栏（Left Sidebar）**：
    - 默认宽度：`220px`；
    - 可调整范围：`min: 140px, max: 450px`；
    - 折叠态：宽度 `0px`（隐藏），通过顶栏图标或 `⌘ + B` 触发，动画时长 `150ms ease-in-out`。
  - **主内容区（Main Workspace）**：
    - 布局策略：`flex-grow: 1, flex-shrink: 1`，自动占据窗口扣除左右分栏后的所有可用空间。
  - **右侧行检查器（Right Inspector）**：
    - 默认宽度：`320px`；
    - 可调整范围：`min: 240px, max: 600px`；
    - 展开状态：抽屉式停靠（Docked Drawer），敲击 `Space` 展开/折叠，平滑滑动动画 `120ms ease-out`。
  - **垂直高度规范**：
    - 统一顶栏（Titlebar & Toolbar）：固定 `38px`（macOS Big Sur+ 风格）或 `44px`；
    - 标签栏（Tab Strip）：固定 `32px`；
    - 表格列头栏（Grid Header）：固定 `28px`；
    - 数据单元格行高：紧凑型 `22px`，标准型 `26px`，舒适型 `32px`；
    - 底部状态栏（Bottom Action Bar）：固定 `28px`；
    - 展开式过滤抽屉（Filter Drawer）：每行条件固定 `36px`。

### 3.2 分割线交互动力学（Splitter Resizer Mechanics）
1. **视觉与判定区分离（Hit-Testing Area）**：
   - 视觉上分割线为极其锐利的 `1px` 物理细线（浅色模式 `#E5E5E5`，深色模式 `#2D2D2D`）；
   - 鼠标事件捕获区（Hit-Test Target）向左右各外扩 `3px`，总共形成一个宽 `7px` 的隐形拖拽热区。
2. **光标反馈**：进入热区时，光标瞬时切换为 `col-resize`（水平双向调节箭头）。
3. **双击自适应（Double-Click Auto-Reset）**：
   - 双击左边栏分割线：若当前宽度不等于默认值，立即弹簧复位至 `220px`；若已是默认值，则触发完全折叠收起。
   - 双击右边栏分割线：自动折叠收起行检查器。
4. **多分屏（Split View）分割管理**：
   - 左右垂直分屏（Split Vertically）：主工作区分割为左右两个等宽面板（各 50%），中间由 1px 可拖拽 Splitter 分隔；
   - 上下水平分屏（Split Horizontally）：主工作区分割为上下两个等高面板（各 50%）；
   - 活动窗格高亮（Active Pane Halo）：当前拥有焦点的 Pane 在顶栏或边框有 1px 的主题强调色（Accent Blue）高亮线，键盘命令只作用于活动 Pane。

---

## 4. 核心区域组件布局与微交互全景拆解

### 4.1 统一顶栏与环境感知胶囊 (Unified Header & Environmental Pill)

顶栏高度固定为 `38px`，承载全局状态与最高频的上下文动作。

```
+-----------------------------------------------------------------------------------------------------------------------+
| [🔴][🟡][🟢] [☰]  [🕒 ⌘Y] [⚡ ⌘E]  |  [ 🔴 PROD: fastgrid_prod ▼ ⌘K ]  |  [● 2 Updates ● 1 Del] [⎘ Preview] [Commit ⌘S] [🔒] [🔍 ⌘P] |
+-----------------------------------------------------------------------------------------------------------------------+
  0-72px      左侧导航动作区              中间环境标定与数据库切换胶囊              右侧暂存提交区与安全/搜索
```

1. **左侧控制区**：
   - `0 ~ 72px`：留白避让 macOS 交通灯按钮；
   - `[☰]`：侧边栏切换按钮，点击平滑折叠/展开左侧 Schema 树；
   - `[🕒 ⌘Y]`：历史记录按钮（History Popover）；
   - `[⚡ ⌘E]`：新建/聚焦 SQL 查询标签页。
2. **中间居中区：环境标定与数据库切换胶囊（Connection/DB Pill, `⌘ + K`）**：
   - **尺寸与排版**：高度 `26px`，圆角 `6px`，内边距 `4px 10px`，居中对齐；
   - **环境色标环（Environment Glow）**：
     - 若该连接被标记为 **Production**（生产环境），胶囊呈现亮红色背景边框（`#E02020`），并带有鲜艳的 `[PROD]` 红色标签；
     - 若为 **Staging**，呈现黄色/橙色边框；
     - 若为 **Development**，呈现绿色边框；
   - **点击弹出数据库切换浮层（Database Switcher Popover）**：
     - 点击胶囊或按 `⌘ + K`，在胶囊正下方弹出浮动列表；
     - 内置搜索框，键盘输入字符即时过滤同实例下的其他 Database / Schema；
     - 上下键切换，按回车直接在当前窗口热切换数据库，无需重连或重新打开窗口。
3. **右侧暂存提交区（Staging Capsule & Actions）**：
   - **脏状态摘要小胶囊（Dirty Status Capsule）**：
     - 显示彩色圆点与改动统计：`● 2 Updates`（黄）、`● 1 Delete`（红）、`● 1 Insert`（绿）；
     - 无改动时文字变灰显示 `No pending changes`；
   - **预览变更按钮（Preview Changes, `⌘ + ⇧ + P`）**：图标为代码分支或 SQL 卷标，点击唤起 SQL 差异弹窗；
   - **放弃变更按钮（Discard, `⌘ + ⇧ + ⌫`）**：点击弹出确认并清空未落库数据；
   - **提交按钮（Commit Button, `⌘ + S`）**：
     - 状态机：无改动时置灰禁用（`opacity: 0.4`）；有暂存变更时瞬时激活为高亮蓝色/绿色主按钮，并伴随一次轻微微缩放动效（Scale 1.05 -> 1.0），视觉反馈极其抓眼。
4. **安全模式与搜索图标**：
   - `[🔒]`：安全模式锁图标，点击切换防护级别（Level 0 / Level 1 / Level 2）；
   - `[🔍 ⌘P]`：Quick Open 唤起图标。

---

### 4.2 多标签栏系统与多窗格拆分交互 (Tab Strip & Split Panes)

标签栏位于顶栏正下方，高度 `32px`，承载工作区多任务流转。

```
+---------------------------------------------------------------------------------------------------+
| [▤ users ✕]  [▤ orders ● ✕]  [⚡ Query 1 ✕]  [+ ▼]                                   [◫ Split ▼] |
+---------------------------------------------------------------------------------------------------+
```

1. **单个 Tab 几何与状态排布**：
   - 高度：`28px`，底部留 `4px` 形成与主工作区的层级落差；
   - 宽度自适应：最小 `110px`，最大 `220px`，超出部分文本采用省略号截断（`text-overflow: ellipsis`）；
   - 内部元素：
     - 左侧小图标（`14px × 14px`）：表格为网格图标 `▤`，视图为眼睛图标 `👁`，SQL 为闪电图标 `⚡`；
     - 标题文本：表名或查询文件名（`12px` Regular）；
     - **未保存脏圆点（Dirty Dot）**：若该 Tab 下有暂存修改，标题右侧出现直径 `6px` 的橙黄色圆点 `●`；
     - 关闭按钮（`✕`）：默认透明度 0.4，鼠标悬停时变为高亮实体并伴随圆形浅灰底色。
2. **Tab 拖拽与重排物理手势**：
   - **水平平移重排**：鼠标按住 Tab 横向拖拽，被拖动的 Tab 产生浅浅的浮起投影，两侧 Tab 产生平滑弹簧位移动画腾出空位；
   - **撕下为新窗口（Window Detachment）**：将 Tab 拖拽出标签栏边界以外松手，瞬间解离为一个完全独立的原生系统窗口；
   - **从外部窗口拖回（Window Merging）**：将外部窗口的 Tab 拖拽回当前标签栏，无缝合并为新 Tab。
3. **多分屏（Split Panes）交互流转**：
   - 点击最右侧 `[◫ Split ▼]` 菜单或通过快捷键：
     - `Split Vertically`（垂直左右并排）；
     - `Split Horizontally`（水平上下并排）；
   - **跨窗格拖拽打开**：从左侧对象树拖拽表名，直接释放到右侧窗格，即可在右侧窗格打开该表，便于两表对比校验。

---

### 4.3 左侧对象树高密度布局与导航 (Schema Browser)

左侧对象树承担着全库成百上千对象的检索与导航，讲究极高的信息密度与丝滑的键盘流转。

```
+-------------------------------------------+
| [🔍 Filter tables, views... (⌘⇧F)       ] |  Height: 28px
+-------------------------------------------+
| 📁 TABLES (18)                            |  Section Header (24px)
|   ▤ users                          14.2k  |  Item Row (24px, Indent: 16px)
|   ▤ orders 🟡                        890  |  Dirty Dot (🟡) / Row Count Muted
|   ▤ order_items                     2.3k  |
|   ▤ products                         450  |
| 📁 VIEWS (4)                              |
|   👁 v_active_users                        |
|   👁 v_monthly_revenue                    |
| 📁 FUNCTIONS (12)                         |
|   ƒ calculate_tax                         |
+-------------------------------------------+
```

1. **顶部即时过滤框（Filter Input）**：
   - 高度 `28px`，背景为浅底圆角矩形，右侧带清除按钮 `(✕)`；
   - 按下快捷键 `⌘ + ⇧ + F` 焦点立即切入该输入框；
   - 输入字符即时以拼音/子串模糊过滤树节点，非匹配项毫秒级折叠隐藏。
2. **分组折叠头（Section Accordion Header）**：
   - 高度 `24px`，字体为 `10px` 粗体全大写（如 `TABLES (18)`、`VIEWS (4)`），文字颜色为弱化中灰；
   - 左侧为极简的旋转三角箭头：展开为 `▼`，收起为 `▶`，旋转动画 `100ms`；
   - 单击折叠头整行即可切换展开/收起。
3. **对象条目行（Item Row）排布**：
   - 行高：紧凑型 `24px`；
   - 缩进深度：固定每级 `16px`；
   - 语义化图标体系：
     - 数据表（Table）：四宫格图标 `▤`；
     - 视图（View）：眼睛图标 `👁`；
     - 物化视图（Materialized View）：带实心底色的眼睛图标；
     - 函数/过程（Function）：斜体 `ƒ` 图标；
     - 触发器（Trigger）：闪电/铃铛图标；
   - 右侧辅助信息：表条目右侧以灰色弱化小字显示行数预估（如 `14.2k`）；若该表有未提交变更，右侧显示黄色小圆点 `🟡`；
   - Hover 态：底色微弱灰显（`rgba(0,0,0,0.04)`）；Active 选中态：高亮主题色浅底（Accent 15% 透明度）。
4. **键盘树状流转（Tree Keyboard Navigation）**：
   - 焦点在树上时：
     - `↑` / `↓`：在可见条目间逐行移动；
     - `→`：展开当前分组；
     - `←`：收起当前分组，若已收起则跳回父级节点；
     - `Return`：在主工作区以 Tab 打开当前选中的表；
     - `⌥ + Return`：在分屏（Split Pane）中打开该表。

---

### 4.4 数据网格微观几何与渲染流水线 (Data Grid Micro-Geometry)

数据网格是 TablePlus 的灵魂组件，其微观几何设计对单元格的边界、间距、字体与颜色有着极其严苛的规范。

```
+-----+----------------+------------------+------------------+---------------+-----------------------+
|  #  | ☰ ID (int8)    | ☰ Name (varchar) | ☰ Email (text)   | ☰ Role_ID [↗] | ☰ Created_At          |  Header: 28px
+-----+----------------+------------------+------------------+---------------+-----------------------+
| 1   | 1001           | Alice Johnson    | alice@corp.com   | 10 [↗]        | 2026-01-01 10:00:00   |
| 2 🟡| 1002           | Bob Smith 🟡     | bob_new@corp.com | 20 [↗]        | 2026-01-02 11:20:00   |  Standard: 26px
| 3 🔴| 1003           | Charlie Brown 🔴 | charlie@test.com | 10 [↗]        | 2026-01-03 09:15:00   |  (Compact: 22px)
| 4   | 1004           | David Lee        | NULL             | NULL          | 2026-01-04 14:00:00   |
| + 🟢| 1005 🟢        | [New Record...]  |                  |               |                       |
+-----+----------------+------------------+------------------+---------------+-----------------------+
  ▲      ▲                  ▲                                   ▲
  │      │                  │                                   └─ 外键指示图标 [↗] (单击穿透跳转)
  │      │                  └─ 单元格脏状态 (黄色背景 / 黄色小圆点)
  │      └─ 表头：列名 + 类型徽章 + 排序图标 + 拖拽手柄
  └─ 行号状态指示槽 (动态宽度 36~48px，最左侧带 3px 纵向脏状态条)
```

#### 4.4.1 表头行（Header Row）几何与交互
* **尺寸规范**：固定高度 `28px`，背景色略深于数据区（浅色 `#F5F5F7`，深色 `#1E1E1E`），底部有 `1px` 实线分割；
* **表头单元格内容排列**：
  1. 列名：`12px` Medium，字重 500；
  2. 数据类型小徽章（Type Badge）：紧随列名之后，显示如 `int8`、`varchar`、`jsonb`，字体为 `10px` 浅灰，带淡淡的圆角灰框；
  3. 外键标示：若该列为外键，右侧带有细微的小箭头图标 `[↗]`；
  4. 排序指示器：排序列显示 `▲`（升序）或 `▼`（降序）。在多列联合排序时，右下角展示排序优先级数字（如 `▲ 1`、`▼ 2`）；
* **列宽拖拽与 Auto-fit 机制**：
  - 分割线热区：列头右边界带有 `4px` 拖拽热区，光标变为 `col-resize`；
  - **双击自动适宽（Auto-fit on Double Click）**：双击分割线时，引擎快速扫描当前屏幕视口内的单元格文本宽度，取最大值并附加上左右内边距（`8px × 2 = 16px`）+ 排序图标留白，瞬间设定为最经济列宽，绝不卡顿；
* **列拖拽重排（Column Reordering）**：
  - 按住表头横向拖拽，表头浮起变为半透明预览；
  - 目标列之间出现一条宽 `2px` 的亮蓝色纵向参考线（Insertion Marker），松手后完成列顺序交换；
* **冻结列（Pinned Columns）**：
  - 支持将前 N 列冻结固定在左侧；
  - 冻结列与滚动列之间有一条带有 `2px` 渐变半透明投影的纵向阴影分割线，横向滚动时冻结列绝对静止。

#### 4.4.2 行号与状态指示槽（Row Gutter & Status Strip）
* **宽度动态计算**：依据当前结果集最大行数位数动态调整（如 1~999 行宽 `36px`，10,000+ 行宽 `48px`），文字右对齐，右边距 `6px`；
* **左侧 3px 垂直脏状态指示条（3px Dirty Indicator Strip）**：
  - **修改过的行（Update）**：行号槽最左侧边缘显示一条 `3px` 宽的明亮黄色坚条；
  - **新增的行（Insert）**：显示 `3px` 宽明亮绿色坚条；
  - **删除的行（Delete）**：显示 `3px` 宽明亮红色坚条，且该行整行文字附加删除线（Strikethrough），整行透明度降为 50%；
* **整行选中交互**：点击行号槽直接全选整行，支持 `Shift + 点击` 连选，按 `Space` 直接呼出该行的详情检查器。

#### 4.4.3 数据单元格排版与视觉编码（Cell Typography & Visual Tokens）
* **内边距规范**：水平内边距固定 `8px`，垂直内边距 `2px`；
* **字体排印严格分级**：
  - **数值类（ID, INT, FLOAT, DECIMAL）、UUID、日期时间（TIMESTAMP）、IP 地址**：必须采用 **等宽字体（Monospace）**（`SF Mono`、`Menlo`、`Consolas`），保证十进制数按位对齐，极具技术质感；
  - **字符串、文本、枚举**：采用系统默认无衬线字体（`SF Pro Text`、`Segoe UI`）；
* **特殊空值与布尔值渲染（绝对严防混淆）**：
  - `NULL` 值渲染：**绝不直接渲染为文本 `"NULL"`**！而是以灰色斜体文本 `NULL` 或带微弱底色的圆角药丸标签呈现，与用户输入的实际字符串 `"NULL"` 形成严格的视觉区隔；
  - 空字符串 `""`：渲染为完全留白；
  - 布尔值（Boolean）：渲染为微型 Checkbox 开关，或浅色背景彩色胶囊（`TRUE` 绿底，`FALSE` 灰底）；
  - 大字段摘要胶囊（Pills）：超长文本或 JSON 单元格展示为 `[JSON 4 keys]`、`[BLOB 45 KB]`，双击在侧边栏完整展开；
* **活动单元格聚焦框（Active Cell Cursor）**：
  - 当前聚焦单元格环绕一条宽 `2px` 的高亮主题色实线边框（Accent Blue，如 `#007AFF`）；
  - **克制设计**：TablePlus 不设置 Excel 式右下角拖拽填充柄，避免在数据库编辑场景中因误触发生全量覆写事故。

#### 4.4.4 行内输入框浮层（Inline Editor Overlay）
* **触发机制**：双击单元格、聚焦按 `Enter`、或聚焦后直接键入字符；
* **挂载与定位**：一个原生文本编辑控件（Native Input / GPUI TextInput）以绝对定位方式，精准覆盖在目标单元格之上，外扩 `1px` 避免边框跳动；
* **进入行为**：进入编辑态时自动全选当前内容，支持键入新字符直接覆盖，或按方向键定位光标；
* **下拉菜单型挂载**：针对 Enum 或外键字段，输入框下方自动呼出悬浮选项列表（Dropdown Menu），支持键入过滤。

---

### 4.5 抽屉式过滤栏与底部停靠状态栏 (Filter Drawer & Docked Status Bar)

```
+---------------------------------------------------------------------------------------------------+
| [✓] [status ▼] [ = ▼ ] ['active']  [+]                       | [Switch to Raw SQL Mode]           |  Filter Drawer
| [✓] [created_at ▼] [ >= ▼ ] ['2026-01-01']                   |                                    |  (Height: 36px/row)
+---------------------------------------------------------------------------------------------------+
| [Filters ● 2]  [Columns (5/8)]  [ 300 ▼ ]  Showing 1-300 of 14,200 (12ms)       | [Export]  [▤]   |  Status Bar (28px)
+---------------------------------------------------------------------------------------------------+
```

1. **底部状态栏（Docked Status Bar）**：
   - 固定高度 `28px`，背景与主网格同色系微弱深色，四边 1px 细线包裹；
   - **左侧**：
     - `[Filters ● 2]` 按钮：若有正在生效的过滤规则，显示蓝色高亮徽章及数量；点击平滑折叠/展开上方过滤抽屉；
     - `[Columns]` 列管理按钮：点击弹出列勾选小弹窗；
     - `[ 300 ▼ ]` 分页容量选择器（100, 300, 1000, All）；
     - `Showing 1-300 of 14,200 rows`：行数统计与耗时（`12ms`）；
   - **右侧**：`[Export]` 导出按钮与 `[▤]` 侧边栏检查器开关。
2. **抽屉式过滤栏（Filter Drawer）展开交互**：
   - 按下 `⌘ + F`，过滤栏从底部状态栏上方以 `120ms` 向上推开网格视口；
   - 每行过滤条件固定高度 `36px`；
   - **GUI 规则组件流**：
     - `[✓]` 单条规则启用复选框（无需删除即可临时停用某条件）；
     - `[列名下拉胶囊]` + `[操作符下拉胶囊]` + `[值输入框]` + `[✕ 删除按钮]`；
     - 右侧 `[+]` 按钮点击新增一行条件；
   - **单条执行 vs 全量执行**：在某规则输入框内按 `Return` 只应用当前条，按 `⌘ + Return` 组合所有打勾条件全量过滤；
   - **切换到 Raw SQL 模式**：点击右上角开关，整行组件平滑过渡为一个单行 SQL 语句框，允许直接键入 `status = 'active' AND (score > 90 OR is_vip = true)`。

---

### 4.6 右侧行详情检查器抽屉 (Row Detail Inspector)

```
+-------------------------------------------------------+
| Row Inspector                                   [ ✕ ] |  Header: 36px
+-------------------------------------------------------+
| Field: metadata (jsonb)                               |
| Mode: [ Text | JSON | Hex | Image ]   [Beautify]      |
|-------------------------------------------------------|
| 1 | {                                                 |  Editor Body
| 2 |   "user_id": 1001,                                |  Line Numbers
| 3 |   "roles": ["admin", "editor"],                   |  Syntax Highlight
| 4 |   "settings": {                                   |  Tree Folders
| 5 |     "theme": "dark"                               |
| 6 |   }                                               |
| 7 | }                                                 |
|-------------------------------------------------------|
| [✓ Valid JSON]                                        |  Validation Bar
+-------------------------------------------------------+
```

1. **抽屉停靠几何**：固定停靠在主工作区右侧，默认宽度 `320px`（支持拖拽扩展至 `600px`），敲击 `Space` 展开或收起；
2. **顶部控制栏（高度 36px）**：
   - 当前字段名与数据类型指示（如 `metadata (jsonb)`）；
   - 分段控制器（Segmented Control）：`[ Text | JSON | Hex | Image ]`；
   - 右侧关闭按钮 `[ ✕ ]`；
3. **多模态编辑器核心体验**：
   - **JSON 模式**：
     - 完整的语法高亮与行号；
     - 节点折叠箭头，点击展开/折叠复杂嵌套对象；
     - 顶栏配备 `[Beautify]`（格式化排版）与 `[Minify]`（单行压缩）；
     - 底部实时校验条：合规显示绿色 `[✓ Valid JSON]`，若非法显示红色 `[✕ Syntax error at line 3]`；
   - **Text 模式**：带行号的多行大文本编辑器，支持软换行（Word Wrap）；
   - **Image 模式**：对于 BLOB/BYTEA 存储的二进制图片数据，以透明棋盘格为背景居中直观渲染出真实图片，并标注分辨率（如 `1920×1080`）与文件体积；
   - **DateTime 模式**：可视化日期时间选择器，支持 Unix 秒/毫秒时间戳与本地时间/UTC 时间互转；
4. **实时双向数据同步**：在检查器中对 JSON 或大文本做出的任何编辑，对应网格单元格立即联动变黄加入暂存态（Pending Update）。

---

### 4.7 悬浮卡片、对话框与无模态弹窗 (Modals, Popovers & Dialogs)

#### 4.7.1 Quick Open 全局快速搜索面板（`⌘ + P`）
```
+-------------------------------------------------------+
| 🔍 search tables, views, functions...                 |  Input: 42px, 16px Font
+-------------------------------------------------------+
| ▤ users                         public         TABLE  |  Item: 38px
| ▤ user_tokens                   public         TABLE  |  (Max 8 items visible)
| 👁 v_active_users                public          VIEW  |  Fuzzy Match Bold
| ƒ get_user_orders               public      FUNCTION  |
+-------------------------------------------------------+
```
* **几何规范**：宽度固定 `560px`，垂直居中偏上（距窗口顶部约 15% 黄金分割处悬浮）；
* **阴影与质感**：高阶弥散阴影（`0 16px 32px rgba(0,0,0,0.25)`），无系统窗口标题栏；
* **输入框**：高度 `42px`，大字号 `16px`，自动夺取第一焦点；
* **结果列表项**：每项高度 `38px`，左侧对象类型图标，中间匹配高亮文本（命中字符加粗变色），右侧弱化显示所属 Schema（如 `public`）和对象类型胶囊；
* **全键盘流转**：`↑`/`↓` 循环滚动选中，`Return` 瞬间打开，`Esc` 瞬间退出。

#### 4.7.2 SQL 差异预览弹窗（Preview Changes, `⌘ + ⇧ + P`）
* **几何规范**：尺寸 `720px × 500px` 模态居中窗口；
* **标题栏**：显示待提交语句统计，如 `Review Pending Changes (3 statements)`；
* **代码视口**：只读原生 SQL 语法高亮查看器，带行号，显示即将发往数据库的完整事务性 DML 语句：
  ```sql
  BEGIN;
  UPDATE "public"."users" SET "email" = 'bob_new@corp.com' WHERE "id" = 1002;
  DELETE FROM "public"."users" WHERE "id" = 1003;
  INSERT INTO "public"."users" ("name", "status") VALUES ('David Lee', 'active');
  COMMIT;
  ```
* **底栏动作组**：
  - 左侧：`[Discard All]` 危险动作按钮（红色边框）；
  - 右侧：`[Copy SQL]` 复制按钮、`[Commit Changes (⌘+S)]` 主按钮。

#### 4.7.3 Safe Mode 拦截二次确认弹窗
* 当处于 Level 1 或 Level 2 安全模式下触发危险 SQL（如无 WHERE 的 UPDATE/DELETE、DROP TABLE）时强制唤起；
* 呈现高饱和度黄色/红色感叹号警示图标，醒目标出受影响表名与危险语句；
* Level 2 严格模式下呈现 macOS Touch ID 指纹认证对话框或数据库密码验证输入框。

---

## 5. 数据网格核心交互与编辑状态流转

### 5.1 选区模型（Selection Model）
1. **单单元格（Single Cell）**：方向键自由游走，高亮单边框；
2. **区域矩形框选（Range Selection）**：`Shift + 方向键` 或鼠标拖拽，多单元格形成半透明矩形高亮选区。按 `⌘ + C` 复制时自动输出为标准的制表符分隔文本（TSV），可直接贴入 Excel；
3. **整行选中（Row Selection）**：点击行号槽全选整行，支持 `Shift` 连选与 `⌘` 离散多选；
4. **全选（Select All）**：`⌘ + A`。

### 5.2 行内编辑与暂存生命周期
* **流转循环**：
  - `Tab`：提交当前单元格修改，光标右移至下一个单元格并保持编辑态；行末自动折返至下一行首列；
  - `Shift + Tab`：提交并左移至上一个单元格；
  - `Enter`：提交并下移至下一行同列单元格；
  - `Escape`：取消本次输入，恢复单元格原值，退出编辑态。
* **特殊值快捷注入**：右键菜单或快捷键支持一键设为 `Set NULL`、`Set Empty String`、`Set Default`。
* **行增删快捷键**：
  - `⌘ + I`：网格末尾插入新行（绿色标示）；
  - `⌘ + D`：以当前行为模板完整克隆一行（自增主键自动清空）；
  - `⌘ + ⌫`：标记删除当前行（整行红底加删除线）；再次按下恢复。
* **单点撤销（Revert Individual Cell）**：右键黄色单元格选择 `Revert Change`，单点恢复原始值，不影响其他未保存单元格。

### 5.3 外键超链接穿透跳转（Foreign Key Navigation）⭐
* 具备外键关系的字段右侧常驻显示小箭头 `[↗]`；
* **单击该箭头**：TablePlus 绝不打断当前心智，直接弹出微型悬浮预览卡片展示关联行；或者以新 Tab 自动打开目标引用的表，并**自动附加 Filter 筛选到该外键对应主键 ID 的行**。这是 TablePlus 最受好评的高能生产力交互之一。

---

## 6. 表结构编辑器 (Structure Editor) 交互

```
+---------------------------------------------------------------------------------------+
| [Data (⌘⌃[)]  [*Structure (⌘⌃])]                                     [Commit (⌘+S)]   |
+---------------------------------------------------------------------------------------+
| [Columns]  [Indexes]  [Foreign Keys]  [Partitions]  [Triggers]                        |
+---------------------------------------------------------------------------------------+
| # | Column Name   | Type        | Default     | PK  | Not Null | AutoInc | Comment    |
+---+---------------+-------------+-------------+-----+----------+---------+------------+
| 1 | id            | bigint      |             | [✓] | [✓]      | [✓]     | Primary ID |
| 2 | name          | varchar(255)| ''          | [ ] | [✓]      | [ ]     | User name  |
| 3 | email 🟡      | text        |             | [ ] | [✓]      | [ ]     | User email |
| + | [New Column]🟢|             |             | [ ] | [ ]      | [ ]     |            |
+---------------------------------------------------------------------------------------+
| DDL Preview: ALTER TABLE "users" ALTER COLUMN "email" TYPE text;                      |
+---------------------------------------------------------------------------------------+
```

1. **双模无缝切换**：通过 `⌘ + ⌃ + ]` 切换到 Structure，按 `⌘ + ⌃ + [` 切换回 Data；
2. **结构选项卡体系**：
   - **Columns（列定义）**：字段名、数据类型（支持模糊下拉补全引擎支持的全部原生类型）、默认值、主键（PK）、非空约束（Not Null）、自增（AutoInc）、字段注释（Comment）、生成列计算表达式；
   - **Indexes（索引定义）**：索引名、索引类型（BTREE, HASH, GIN, GIST）、组合列勾选列表、是否唯一（Unique）、部分索引 WHERE 条件；
   - **Foreign Keys（外键约束）**：约束名、本表列、目标表及目标列、`On Delete` / `On Update` 级联行为（`CASCADE`、`SET NULL`、`RESTRICT`）；
3. **结构变更暂存与 DDL 生成**：在界面增删改字段或索引后同样进入暂存态（黄/绿/红），按 `⌘ + ⇧ + P` 查看生成的 `ALTER TABLE` 语句，按 `⌘ + S` 提交 DDL。

---

## 7. SQL 查询编辑器 (Query Editor) 交互

```
+---------------------------------------------------------------------------------------+
| [Tab: Query 1]                                      [Env: 🟢 DEV]  [Format] [Run (⌘↵)]|
+---------------------------------------------------------------------------------------+
| 1 | -- Calculate active user orders                                                   |
| 2 | SELECT u.id, u.name, COUNT(o.id) as total_orders                                  |
| 3 | FROM users u                                                                      |
| 4 | LEFT JOIN orders o ON o.user_id = u.id                                            |
| 5 | WHERE u.status = 'active'                                                         |
| 6 | GROUP BY u.id, u.name;                                                            |
+---------------------------------------------------------------------------------------+
| Results (1)  |  Messages / Logs (12ms)  |  Query Execution Plan (EXPLAIN)             |
+---------------------------------------------------------------------------------------+
| id  | name          | total_orders                                                    |
| 1   | Alice Johnson | 42                                                              |
| 2   | Bob Smith     | 18                                                              |
+---------------------------------------------------------------------------------------+
```

1. **分号感知执行（Smart Statement Execution）**：
   - 快捷键 `⌘ + Return`（运行当前语句）：无需手动划选整行，编辑器自动探测当前光标前后的分号（`;`）边界，在背景上轻微高亮当前独立语句块并下发执行；
   - 快捷键 `⌘ + ⇧ + Return`：运行当前编辑器的全部 SQL；
   - 若用户主动划选了文本，则优先只运行选中文本；
2. **智能代码补全（IntelliSense）**：
   - 动态推荐优先级：上下文关联表所包含的**列名** > 数据库已存在的**表名与视图** > SQL **关键字** > 常用代码片段（Snippets）；按 `Tab` 或 `Return` 快速上屏；
3. **多光标编辑（Multi-Cursor）**：按住 `Option` 垂直框选，或按住 `⌘` 在多处单击生成多光标同步键入；
4. **可编辑的查询结果网格（Editable Result Grid）⭐**：
   - 若查询属于单表且结果集**包含该表的主键字段（Primary Key）**；
   - 结果集网格直接具备与数据表相同的双击行内编辑与暂存提交能力，按下 `⌘ + S` 可直接落库！若无法安全定位主键则进入只读模式。

---

## 8. 环境标定与安全生产防护 (Safe Mode)

```mermaid
flowchart TD
    A["执行 SQL 或提交变更 (⌘+S / ⌘+↵)"] --> B{"当前环境配置"}
    B -->|Development| C["直接执行"]
    B -->|Staging| D{"危险操作告警检测"}
    B -->|Production| E{"安全模式 (Safe Mode) 校验"}
    
    D -->|常规 SELECT/INSERT| C
    D -->|DROP / TRUNCATE / 无 WHERE 的 UPDATE| F["弹出黄色确认弹窗"]
    F -->|确认| C
    F -->|取消| G["中止执行"]
    
    E -->|只读模式| H["直接拦截报错 (Read-Only)"]
    E -->|严格安全模式| I["输入数据库密码 / Touch ID 指纹认证"]
    I -->|认证成功| C
    I -->|认证失败| G
```

1. **视觉环境感知**：生产环境红标、测试环境橙标、本地开发绿标，常驻顶栏；
2. **安全阶梯（Safe Mode Levels）**：
   - **Level 0 (Normal)**：普通模式，直接放行；
   - **Level 1 (Alert)**：拦截危险操作（`DROP`、`TRUNCATE`、无 `WHERE` 子句的 `UPDATE`/`DELETE`），弹出告警弹窗核对影响行数与语句；
   - **Level 2 (Strict Safe)**：任何写入落库与 DDL 执行前必须通过 **macOS Touch ID 指纹认证** 或输入数据库密码；
   - **Read-Only Mode**：完全禁用任何变更与写指令。

---

## 9. 导入与导出向导交互 (Import & Export)

1. **CSV 可视化映射导入向导**：
   - 选定本地 `.csv` 文件后，向导先加载前 10 行做结构预览；
   - 双列对齐矩阵：左侧 `CSV Header`，右侧 `Database Column`，智能按名称对齐，允许手动调整或跳过特定列；
   - 错误策略：中止导入或跳过错误行写入日志；
2. **全能导出向导（`⌘ + ⇧ + E`）**：
   - 目标可选：单表、SQL 查询结果、批量多表或全库；
   - 格式选项：CSV、SQL Dump、JSON、Excel；
   - 配置项：仅结构（Structure Only）、仅数据（Data Only）、压缩（Gzip）。

---

## 10. 视觉设计系统规范 (Design Tokens & Typography)

### 10.1 色彩语义规范（Semantic Color Tokens）
| Token 类别 | 语义描述 | Light Mode | Dark Mode | 运用场景 |
|---|---|---|---|---|
| `bg-app` | 应用背景底色 | `#FFFFFF` | `#1E1E1E` | 主工作区基底 |
| `bg-sidebar` | 侧边栏背景色 | `#F5F5F7` | `#181818` | 左侧对象树面板 |
| `border-divider` | 分割线与边框 | `#E5E5E5` | `#2D2D2D` | 1px 面板与单元格分割 |
| `accent-primary` | 主强调交互色 | `#007AFF` | `#0A84FF` | 选区框、Commit 激活态 |
| `state-update` | 暂存修改态 | `#F5A623` (底色: 15% 浅黄) | `#E6A23C` | 修改过的单元格背景、小圆点 |
| `state-insert` | 暂存新增态 | `#27AE60` (底色: 15% 浅绿) | `#2ECC71` | 新增的行底色、小圆点 |
| `state-delete` | 暂存删除态 | `#EB5757` (底色: 15% 浅红) | `#E74C3C` | 删除行底色、删除线 |
| `token-null` | 空值弱化色 | `#8C8C8C` (Italic) | `#6E6E6E` (Italic) | `NULL` 专用斜体 |
| `env-prod` | 生产环境警示 | `#E02020` | `#FF453A` | 生产环境胶囊与边框 |

### 10.2 字体排印规范（Typography Scale）
| 字号等级 | 字号 / 行高 | 字重 | 字体家族 | 适用组件 |
|---|---|---|---|---|
| `font-badge` | 10px / 12px | 600 Semi-Bold | Sans-Serif | 列数据类型徽章、Section 折叠头 |
| `font-caption`| 11px / 14px | 400 Regular | Sans-Serif | 行数统计、辅助元数据、耗时信息 |
| `font-grid-data`| 12px / 20px | 400 Regular | Monospace (`SF Mono`) | 数字、UUID、日期、数值单元格 |
| `font-grid-text`| 12px / 20px | 400 Regular | Sans-Serif (`SF Pro`) | 普通字符串、枚举单元格 |
| `font-header` | 12px / 16px | 500 Medium | Sans-Serif | 表头列名、左侧树节点条目 |
| `font-tab` | 13px / 18px | 400 Regular | Sans-Serif | 顶部选项卡标题、按钮文本 |
| `font-code` | 13px / 18px | 400 Regular | Monospace (`SF Mono`) | SQL 编辑器正文、Inspector JSON |
| `font-search` | 16px / 22px | 400 Regular | Sans-Serif | Quick Open 悬浮搜索输入框 |

---

## 11. TablePlus 全量快捷键映射表 (macOS / Win / Linux)

| 功能模块 | 操作名称 | macOS 快捷键 | Windows / Linux 快捷键 |
|---|---|---|---|
| **全局与导航** | Quick Open（全局快捷打开对象） | `⌘ + P` | `Ctrl + P` |
| | 快速切换数据库（Database Picker） | `⌘ + K` | `Ctrl + K` |
| | 快速切换连接（Connection Picker） | `⌘ + ⇧ + K` | `Ctrl + Shift + K` |
| | 新建查询选项卡（New Query Tab） | `⌘ + T` / `⌘ + E` | `Ctrl + T` / `Ctrl + E` |
| | 关闭当前选项卡（Close Tab） | `⌘ + W` | `Ctrl + W` |
| | 切换到下一个 Tab | `⌘ + ]` | `Ctrl + Tab` / `Ctrl + PgDn` |
| | 切换到上一个 Tab | `⌘ + [` | `Ctrl + Shift + Tab` / `Ctrl + PgUp` |
| | 切换到第 1~9 个 Tab | `⌘ + 1` ~ `⌘ + 9` | `Alt + 1` ~ `Alt + 9` |
| | 切换上一/下一分屏面板（Pane） | `⌘ + ⌥ + [` / `⌘ + ⌥ + ]` | `Ctrl + Alt + [` / `Ctrl + Alt + ]` |
| | 查看查询历史（Query History） | `⌘ + Y` | `Ctrl + Y` |
| **数据与结构视图** | 显示数据视图（Show Table Data） | `⌘ + ⌃ + [` | `Ctrl + Shift + [` |
| | 显示结构视图（Show Structure） | `⌘ + ⌃ + ]` | `Ctrl + Shift + ]` |
| | 刷新当前数据（Reload / Refresh） | `⌘ + R` | `Ctrl + R` / `F5` |
| **数据网格编辑** | 提交暂存的修改（Commit Changes） | `⌘ + S` | `Ctrl + S` |
| | 放弃所有暂存修改（Discard Changes） | `⌘ + ⇧ + ⌫` | `Ctrl + Shift + Del` |
| | 预览修改生成的 SQL（Preview SQL） | `⌘ + ⇧ + P` | `Ctrl + Shift + P` |
| | 撤销（Undo） | `⌘ + Z` | `Ctrl + Z` |
| | 重做（Redo） | `⌘ + ⇧ + Z` | `Ctrl + Shift + Z` |
| | 插入新行（Insert New Row） | `⌘ + I` | `Ctrl + I` |
| | 克隆/复制行（Duplicate Row） | `⌘ + D` | `Ctrl + D` |
| | 标记删除选中行（Delete Row） | `⌘ + ⌫` | `Ctrl + Backspace` / `Delete` |
| | 切换右侧行详情侧边栏（Toggle Inspector） | `Space` | `Space` |
| | 行内编辑：跳至下一单元格 | `Tab` | `Tab` |
| | 行内编辑：跳至上一单元格 | `⇧ + Tab` | `Shift + Tab` |
| | 行内编辑：确认并跳至下方单元格 | `Return` | `Enter` |
| | 取消行内编辑 | `Escape` | `Escape` |
| **过滤与查找** | 打开行过滤器（Open Row Filters） | `⌘ + F` | `Ctrl + F` |
| | 打开列可见性过滤器（Column Filter） | `⌘ + ⌥ + F` | `Ctrl + Alt + F` |
| | 应用单条选中过滤规则 | `Return` | `Enter` |
| | 应用所有打勾过滤规则 | `⌘ + Return` | `Ctrl + Enter` |
| | 复制当前过滤规则 | `⌘ + I` | `Ctrl + I` |
| | 删除当前过滤规则 | `⌘ + ⇧ + I` | `Ctrl + Shift + I` |
| | 退出过滤器聚焦回到网格 | `Escape` | `Escape` |
| **SQL 编辑器** | 运行当前光标所在 SQL 语句 | `⌘ + Return` | `Ctrl + Enter` |
| | 运行编辑器内全部 SQL 语句 | `⌘ + ⇧ + Return` | `Ctrl + Shift + Enter` |
| | 取消正在执行的查询 | `Escape` / `⌘ + .` | `Escape` |
| | 格式化美化 SQL（Beautify SQL） | `⌘ + I` / `⌘ + ⇧ + F` | `Ctrl + Shift + F` |
| | 注释 / 取消注释行 | `⌘ + /` | `Ctrl + /` |
| **导入与导出** | 导出当前表或数据（Export） | `⌘ + ⇧ + E` | `Ctrl + Shift + E` |

---

## 12. 对 FastGrid (Rust + GPUI) 的落地方案与组件树蓝图

基于上述深度调研，FastGrid（基于 Rust 2024 与 GPUI Kit）可以直接采用以下工程组件拓扑与状态机设计方案：

### 12.1 GPUI 组件层级树（Component Hierarchy Blueprint）
在 GPUI 中，窗口根视图可按照 Flexbox 模式完整组织为：

```rust
// FastGrid 顶层主窗口组件树 (Pseudocode Blueprint)
div()
    .flex()
    .flex_col()
    .size_full()
    .bg(theme.bg_app)
    .text_color(theme.text_primary)
    // 1. 统一顶栏 (Unified Titlebar)
    .child(
        div()
            .h(px(38.0))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px_3()
            .border_b_1()
            .border_color(theme.border_divider)
            .child(LeftWindowControls::new())
            .child(ConnectionEnvironmentPill::new(&self.active_conn))
            .child(StagingControlCapsule::new(&self.pending_changes))
    )
    // 2. 主体三栏分屏区 (Middle Body Split Container)
    .child(
        div()
            .flex()
            .flex_row()
            .flex_grow()
            .size_full()
            .overflow_hidden()
            // 2.1 左侧对象树 (Schema Browser)
            .when(self.sidebar_visible, |d| {
                d.child(
                    div()
                        .w(px(self.sidebar_width))
                        .flex_shrink_0()
                        .child(SchemaBrowserTree::new(&self.schema))
                )
            })
            // 2.2 左侧分割线调节手柄 (Resizer Handle)
            .child(ResizerHandle::vertical("sidebar_split", &self.sidebar_width))
            // 2.3 核心工作区 (Main Tab Workspace)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_grow()
                    .child(TabStrip::new(&self.tabs, self.active_tab_idx))
                    // 数据网格或 SQL 编辑器视口
                    .child(
                        div()
                            .flex_grow()
                            .child(match self.active_tab_content() {
                                TabContent::DataGrid(grid) => DataGridCanvas::new(grid),
                                TabContent::SqlEditor(editor) => SqlEditorView::new(editor),
                                TabContent::Structure(struct_view) => StructureEditorView::new(struct_view),
                            })
                    )
                    // 抽屉式过滤栏 (可折叠)
                    .when(self.filter_drawer_open, |d| {
                        d.child(FilterAccordionDrawer::new(&self.active_filters))
                    })
                    // 底部停靠状态栏
                    .child(BottomStatusBar::new(&self.grid_status))
            )
            // 2.4 右侧分割线调节手柄
            .when(self.inspector_open, |d| {
                d.child(ResizerHandle::vertical("inspector_split", &self.inspector_width))
                 .child(
                     div()
                         .w(px(self.inspector_width))
                         .flex_shrink_0()
                         .child(RowDetailInspector::new(&self.selected_row))
                 )
            })
    )
```

### 12.2 数据集状态机（Changeset Overlay 架构）
```
[Baseline Data (服务端拉取的不可变快照)]
                  │
                  ▼
[Changeset Overlay (本地内存暂存增量)]
  ├── modified_cells: HashMap<(RowId, ColId), (OldValue, NewValue)>
  ├── inserted_rows: Vec<RowData>
  └── deleted_row_ids: HashSet<RowId>
                  │
                  ├──> [Canvas Render Pipeline] -> 黄/绿/红三色实时合成
                  ├──> [Undo / Redo Stack]      -> 极速内存出入栈
                  └──> [SQL DML Compiler]       -> 生成 Preview 与 Commit 事务
```
* **渲染性能无损**：GPUI Canvas 绘制单元格时，直接由 `(RowId, ColId)` 在 `Changeset Overlay` 中进行 `O(1)` 哈希查表。若命中，则叠加黄色背景涂层；若在 `deleted_row_ids` 中，则附加贯穿删除线。
* **零网络开销**：撤销（Undo）、重做（Redo）与全盘放弃（Discard）均纯在内存修改哈希集合，无任何等待延迟。

### 12.3 焦点层级树与快捷键路由（Focus Hierarchy in GPUI）
为彻底杜绝键盘快捷键在多层组件嵌套时“迷失”或“吞键”，需建立严格的 `FocusHandle` 捕获链：

```
[Window Root Focus]
  ├── [QuickOpen Focus (⌘P)] ──── (最高优先级拦截，激活时抢占焦点)
  ├── [ConnectionSwitcher (⌘K)]
  └── [Active Workspace Focus]
        ├── [Filter Drawer Focus (⌘F)]
        ├── [Data Grid Focus]
        │     ├── Arrow Keys ──> 单元格光标平移
        │     ├── Enter ───────> 唤起 InlineEditorFocus
        │     ├── Space ───────> 唤起 InspectorFocus
        │     └── Tab ─────────> 单元格跳步
        └── [Inspector Focus]
```
* **Escape 逐级平滑退出规范**：
  - 1 次 `Escape`：若正处于单元格行内编辑态，退出编辑态回到单元格聚焦；
  - 2 次 `Escape`：若过滤栏或检查器处于展开状态，收起抽屉；
  - 3 次 `Escape`：清空多选选区。

### 12.4 FastGrid 突破点：结合 MCP 的外键图谱创新
* **TablePlus 的局限**：外键箭头只能**单向正向**跳转到父表记录；
* **FastGrid 的超越契机**：
  - 借助 FastGrid 内置的 `fg-mcp` 元数据关系分析引擎；
  - 支持 **反向引用追溯（Incoming References）**：在某主键单元格上右键或点击徽章，一键列出“全库中有哪些子表（如订单表、日志表、审计表）外键引用了当前 ID”；
  - 一键带条件级联打开所有关联子表 Tab，实现比 TablePlus 更强大的关联数据排查体验！

---

## 总结

TablePlus 的成功不仅在于原生的高性能，更在于其**对每一个像素、每一个键盘流转步骤、每一级安全边界的克制打磨**。

本调研文档从**微观几何尺寸、核心组件拓扑、状态机流转、色彩字体 Token** 到 **GPUI 落地代码树** 进行了全链路解析。FastGrid 团队在推进 M1（连接与浏览）、M2（SQL 编辑器）、M3（行内编辑与结构管理）的研发过程中，可直接以本规格说明作为 UI/UX 对齐标准，打造出体验比肩甚至超越 TablePlus 的次世代 Rust 原生数据库客户端。
