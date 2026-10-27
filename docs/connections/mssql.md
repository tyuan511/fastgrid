# SQL Server 连接指南

## 表单字段

- **Host / Port**：默认端口 1433。命名实例（`host\INSTANCE`）
  不走 `Port`，请填浏览器解析出的 TCP 端口。
- **User / Password**：SQL 登录（`sa` 或自建登录）。Windows
  集成认证不支持——驱动只带 SQL 认证，跨平台也无需额外依赖。
- **Database**：可空；不填时连到登录的默认库。SQL Server 的
  「库」不是 schema：schema（默认 `dbo`）在库内部，对象树里
  显示为命名空间。
- **TLS**：与 PostgreSQL 同一套模式（默认 Prefer）。SQL Server
  在登录阶段总会握手，所以证书问题一定会暴露出来：
  - `Disable` → `EncryptionLevel::NotSupported`
  - `Prefer` → `On`，不校验证书
  - `Require` → `Required`，不校验证书
  - `VerifyCa` → `Required` + `ca_cert_path`
  - `VerifyFull` → 同上，另需 `server_name` 做主机名校验
- **SSH 隧道**：与 PostgreSQL 相同（Password / Private key /
  SSH agent），Host / Port 指隧道远端看到的地址。

## 连接串

启动页的 URL 框接受 `sqlserver://` 或 `mssql://` 连接串：

```text
sqlserver://sa:password@db.example.com:1433/app?sslmode=require
```

`sslmode` 与 `sslMode` 两种拼法都接受，取值同 PostgreSQL /
MySQL（`disable` / `prefer` / `require` / `verify-ca` /
`verify-full`）。

## 行为说明

- **只读模式靠语句分类，不是引擎事务**：SQL Server 没有会话级
  只读开关（登录记录里的 `ApplicationIntent=ReadOnly` 只是提示，
  不阻止写入）。`SafeMode::ReadOnly` 下驱动在发送前拒绝任何可能
  写入的语句（返回 `PolicyDenied`），`Capabilities::read_only_session`
  为 `false`，所以 UI 不会声称引擎层只读。
- **取消用 `KILL`**：TDS 的 Attention 机制在这个驱动里不可用
  （实测会让连接失步并静默返回 0 行），所以取消是另开一条连接发
  `KILL <spid>`。这需要 sysadmin 或 `ALTER ANY CONNECTION` 权限；
  低权限登录取消会失败，此时会话被标记为不可复用并丢弃，而不是
  立刻停下。语句超时（`timeouts.statement_secs`）同样走这条路。
- **DDL 可回滚**：SQL Server 的 DDL 是事务性的，结构模式的提交在
  一个事务里，失败整体回滚（与 PostgreSQL 一致，与 MySQL 相反）。
- **事务语句只走批处理路径**：驱动内部对 `BEGIN TRANSACTION` /
  `COMMIT` / `ROLLBACK` 使用 Batch 请求，因为走 RPC（sp_executesql）
  会报「事务计数不匹配」（code 266）。这对外不可见。
- **`GO` 是客户端分隔符**：脚本里的 `GO`（必须独占一行）只用来
  分批，从不下发给服务器。编辑器与导入都按它拆分。
- **分页必须有 `ORDER BY`**：SQL Server 的 `OFFSET/FETCH` 不接受
  没有排序的查询，网格在没有排序时自动补 `ORDER BY (SELECT NULL)`。
- **EXPLAIN 不支持**：SQL Server 没有 `EXPLAIN`，执行计划来自
  `SET SHOWPLAN_XML ON`（会话开关，会让会话不再执行语句）。编辑器
  的解释按钮在 SQL Server 上直接报「不支持」，请用 SSMS 看计划。
- **可编辑查询结果不支持**：结果列没有来源表信息（TDS 元数据只有
  列名与类型），`Capabilities::column_origin` 为 `false`。
- 驱动走 rustls，不依赖 OpenSSL。
- 本地验证过的是 SQL Server 2022（Linux 容器，CU27）；2017 / 2019
  与 Windows 实例未逐版本跑过行为测试。
