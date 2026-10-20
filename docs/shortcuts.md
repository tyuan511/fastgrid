# 快捷键规范

基于调研报告 §11 整理。原报告把 `⌘I` 同时分给「插入行 / 复制过滤规则 / 格式化 SQL」，`⌘⇧F` 同时分给
「聚焦对象树过滤框 / 格式化 SQL」，且 Windows/Linux 只是把 ⌘ 机械替换为 Ctrl。这里按**焦点上下文**重新分配：
同一组合键允许出现在不同上下文，**同一上下文内不得重复**。落地时对应 GPUI 的 `key_context`。

约定：⌘=Cmd，⌃=Control，⌥=Option，⇧=Shift。Win/Linux 列只写与「⌘→Ctrl」不同的项。

## 全局（任何焦点）

| 操作 | macOS | Win / Linux |
|---|---|---|
| Quick Open（跳转到表 / 视图） | `⌘P` | `Ctrl+P` |
| 切换数据库 / 命名空间 | `⌘K` | `Ctrl+K` |
| 切换连接 | `⌘⇧K` | `Ctrl+Shift+K` |
| 新建查询标签 | `⌘T` | `Ctrl+T` |
| 关闭标签 | `⌘W` | `Ctrl+W` |
| 下一个 / 上一个标签 | `⌘]` / `⌘[` | `Ctrl+Tab` / `Ctrl+Shift+Tab` |
| 第 1–9 个标签 | `⌘1`–`⌘9` | `Alt+1`–`Alt+9` |
| 显示 / 隐藏侧栏 | `⌘B` | `Ctrl+B` |
| 查询历史 | `⌘Y` | `Ctrl+Y` |
| 刷新 | `⌘R` | `F5` |
| 导出 | `⌘⇧E` | `Ctrl+Shift+E` |
| 切换到数据 / 结构视图 | `⌘⌃[` / `⌘⌃]` | `Ctrl+Alt+[` / `Ctrl+Alt+]` |

## 对象树（`tree`）

| 操作 | macOS | Win / Linux |
|---|---|---|
| 聚焦过滤框 | `⌘⇧O` | `Ctrl+Shift+O` |
| 上下移动 / 展开 / 收起（再按跳父级） | `↑↓` / `→` / `←` | 同 |
| 在新标签打开 | `Return` | `Enter` |

## 数据网格（`grid`，未处于编辑态）

| 操作 | macOS | Win / Linux |
|---|---|---|
| 提交暂存变更 | `⌘S` | `Ctrl+S` |
| 预览将执行的 SQL | `⌘⇧P` | `Ctrl+Shift+P` |
| 放弃全部变更（需确认） | `⌘⇧⌫` | `Ctrl+Shift+Delete` |
| 撤销 / 重做 | `⌘Z` / `⌘⇧Z` | `Ctrl+Z` / `Ctrl+Shift+Z` |
| 插入行 | `⌘I` | `Ctrl+I` |
| 克隆行 | `⌘D` | `Ctrl+D` |
| 标记删除 / 恢复行 | `⌘⌫` | `Delete` |
| 打开 / 关闭行详情 | `Space` | `Space` |
| 开始编辑 | `Return` 或直接键入 | `Enter` 或直接键入 |
| 复制选区（TSV） | `⌘C` | `Ctrl+C` |
| 打开过滤抽屉 | `⌘F` | `Ctrl+F` |
| 列可见性 | `⌘⌥F` | `Ctrl+Alt+F` |

> `Space` 只在网格有焦点且**不在编辑态**时切换行详情；键入任何可打印字符（包括除 `Space` 外的所有字符）进入编辑。
> 需要以空格开头的值，先按 `Return` 进入编辑再输入。

## 行内编辑（`grid.editing`）

| 操作 | 键 |
|---|---|
| 确认并右移（行末折返下一行） | `Tab` |
| 确认并左移 | `Shift+Tab` |
| 确认并下移 | `Return` |
| 取消，恢复原值 | `Escape` |

## 过滤抽屉（`filter`）

| 操作 | macOS | Win / Linux |
|---|---|---|
| 仅应用当前规则 | `Return` | `Enter` |
| 应用所有勾选规则 | `⌘Return` | `Ctrl+Enter` |
| 复制当前规则 | `⌘I` | `Ctrl+I` |
| 删除当前规则 | `⌘⇧I` | `Ctrl+Shift+I` |
| 回到网格 | `Escape` | `Escape` |

## SQL 编辑器（`editor`）

| 操作 | macOS | Win / Linux |
|---|---|---|
| 运行当前语句（选中则运行选中） | `⌘Return` | `Ctrl+Enter` |
| 运行全部 | `⌘⇧Return` | `Ctrl+Shift+Enter` |
| 取消查询 | `Escape` / `⌘.` | `Escape` |
| 格式化 SQL | `⌥⇧F` | `Alt+Shift+F` |
| 注释 / 取消注释 | `⌘/` | `Ctrl+/` |
| 多光标 | `⌥` 拖选 / `⌘` 点击 | `Alt` 拖选 / `Ctrl` 点击 |

## Escape 逐级退出

1. 行内编辑中 → 退出编辑，回到单元格聚焦
2. 过滤抽屉 / 行详情展开 → 收起
3. 存在多选 → 清空选区
4. 查询运行中且焦点在编辑器 → 取消查询（优先级高于 2、3）

## 已知需要再确认的点

- `⌘[` / `⌘]` 与编辑器的缩进快捷键在 `editor` 上下文内冲突：编辑器内缩进使用 `Tab` / `Shift+Tab`，标签切换保持全局。
- Win/Linux 的 `Ctrl+K`、`Ctrl+B` 与部分输入法或终端习惯可能冲突，需要三端实机验证后调整。
- 所有快捷键必须可在设置中重绑定（M8），并在命令面板中可搜索到。
