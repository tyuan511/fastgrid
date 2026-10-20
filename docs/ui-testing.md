# 界面测试

`fg-app` 的视图用 GPUI Kit 自带的无头测试验证：在不弹出真实窗口的情况下渲染真实组件、做真实命中测试的点击和按键，再读取元素状态。**它验证行为，不验证画面**——间距、颜色、字体需要人看。

## 结构：为什么视图要「笨」

```
fg-app/src/
  model/        纯 Rust，不依赖 GPUI：单元格格式化、分页 / 排序 / 过滤状态、模糊匹配、标签、对象树
  views/        GPUI 视图：只渲染数据，用户操作变成事件
  controller.rs 把视图事件接到 fg-service，再把结果灌回视图
```

视图拿到数据、发出事件（`LauncherEvent::Open`、`TableTabEvent::Reload`、`WorkspaceEvent::TableOpened`），不直接调用服务。这样：

- 视图可以用合成数据完整测试，不需要数据库、不需要异步；
- 控制器 + 视图 + 真实 SQLite 的整条链路另有端到端测试（`tests/workspace_e2e.rs`）。

## 测试文件

| 文件 | 内容 |
|---|---|
| `src/model/*.rs`（单元测试） | 纯逻辑 |
| `tests/launcher_view.rs` | 启动页：选择、双击、按钮、消息 |
| `tests/grid_view.rs` | 网格、状态栏、分页、排序、过滤抽屉、复制 |
| `tests/workspace_view.rs` | 工作区：侧栏、过滤、标签、Quick Open、快捷键 |
| `tests/workspace_e2e.rs` | 真实 SQLite + 加密存储 + 服务 + 控制器 + 视图 |

## 必踩的坑

1. **不要 `use gpui_kit::*`**。它会把 `gpui_kit::test` 一起导入，`#[gpui_kit::test]` 展开出的 `#[test]` 就会指向它自己，报 *recursion limit reached*（有时是编译器 SIGBUS）。只导入具体名字。
2. **事件在一次更新结束后才送达**。`cx.subscribe` 的回调在 `update_window` 闭包返回之后才执行：在同一个闭包里读事件日志永远是空的。把「动作」和「断言事件」分到两个 `update_window` 里。
3. **`try_find(..).is_none()` 可能是空洞的**。只有调用了 `.test_support()` 的元素才会被注册；没注册时「找不到」恒成立。每个会被断言「不存在」的元素，都要有一个它「存在」的正向断言（本仓库已经因此抓到过两处）。
4. **`role` / `selected` 读不到，`label` 可以**。快照里 `aria_label` 可靠；`aria_selected` / `role` 在我们的元素上读不回来。需要断言选中状态时，读视图自己的状态（公开访问器），而不是读快照。
5. **调度器是确定性的，服务线程不是**。端到端测试里服务有自己的线程，必须在测试开头 `cx.executor().allow_parking()`，并用「`run_until_parked` + 真实 sleep」的轮询等待结果。
6. **定时器是虚拟时间**。防抖（会话保存的 600ms）在测试里永远不会自己到点：用 `cx.executor().advance_clock(..)` 推进。
7. **编辑代码时别用不带断言的字符串替换**。`cargo fmt` 会改写排版，替换悄悄不生效，代码照样编译通过（抽屉不显示的 bug 就是这样来的）。

## 手动运行应用

```bash
# 真实窗口，数据放进临时目录，设备密钥只放内存（退出后数据不可再读，不碰系统钥匙串）
FASTGRID_DATA_DIR=$(mktemp -d) FASTGRID_MEMORY_KEYCHAIN=1 cargo run -p fg-app
```

## 性能测量

```bash
cargo test -p fg-service --release --test e2e -- --ignored --nocapture perf_
```
