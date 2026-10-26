# 诊断、日志与隐私

## 日志

GUI 与 CLI 启动时在 `<data_dir>/logs/fastgrid.log` 写日志（`tracing`，
`FASTGRID_LOG` 可改过滤，默认 `warn` 加各 `fg_*` crate 的 `info`）。
文件按 5 MiB 轮转，保留最近 3 个旧文件（`fastgrid.log.1` 最新）。
日志里不出现密码、连接串与 SQL——这是仓库的硬规则，不是约定。

`<data_dir>`：macOS `~/Library/Application Support/fastgrid`，
Windows `%APPDATA%\fastgrid`，Linux `~/.local/share/fastgrid`，
`FASTGRID_DATA_DIR` 可覆盖。

## 诊断导出

启动页的「诊断」面板可以把一份诊断包写到指定路径，内容只有：

- fastgrid 版本、平台（OS + 架构）
- 遥测开关的当前值
- 按引擎统计的连接数（只有数量，没有连接名）
- 日志尾部（最近 50 行）

不含密码、连接串、SQL、连接名与任何连接配置。日志文件本身不存在时
写明「no log file yet」，不编造内容。

## 遥测

默认关闭。`PrivacySettings.telemetry` 默认 `false`，目前没有任何代码
真的向外发送遥测数据——开关只是为将来保留的显式许可位。同一诊断
面板里可以开关它，改动立即持久化。

## 崩溃恢复

崩溃后重开，恢复分两层（都有测试覆盖）：

- 会话：标签页与侧栏状态每 600 ms 防抖保存，重开时表标签按保存
  的顺序重开，不存在的表跳过，最后回到之前活动的标签。
- 查询草稿：输入时写 journal，保存时清 journal。重开时先读已保存
  的草稿，再比 journal：如果 journal 更新，恢复为未保存状态（脏），
  下一次防抖保存会落盘并清 journal；读不懂的记录只读展示，绝不
  回写覆盖。

表标签上未提交的暂存修改（staged changes）不在会话里，崩溃会丢
失；关标签时有确认横幅，但整个窗口/应用退出时目前没有拦截确认——
这是已知缺口，见路线图 M8 的诚实缺口。
