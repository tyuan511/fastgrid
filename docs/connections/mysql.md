# MySQL / MariaDB 连接指南

## 表单字段

- **Host / Port**：默认端口 3306。
- **User / Password**：密码只在表单里输入一次，保存后加密
  落盘，启动页不会把它读回显示。
- **Database**：可空；不填时连到服务器的默认库。
- **TLS**：与 PostgreSQL 同一套模式（默认 Prefer），
  证书路径与 server 名称覆盖的填法相同。
- **SSH 隧道**：与 PostgreSQL 相同（Password / Private key /
  SSH agent），Host / Port 指隧道远端看到的地址。

## 连接串

启动页的 URL 框接受 `mysql://` 连接串，粘贴后解析进表单，
例如：

```text
mysql://user:pass@db.example.com:3306/app?sslMode=VERIFY_CA
```

注意参数名是 `sslMode`（MySQL 侧的大小写），取值如
`DISABLED` / `PREFERRED` / `REQUIRED` / `VERIFY_CA` /
`VERIFY_IDENTITY`。两种拼法（`sslmode=verify-full` 与
`sslMode=VERIFY_IDENTITY`）对两个 scheme 都接受，表单不
区分是谁写的 URL。

## 行为说明

- 本地验证过的是 MariaDB 10.11；MySQL 8.x 走同一协议，
  但没有逐版本跑过行为测试，遇到方言差异请先按
  MariaDB 10.11 的表现排查。
- **DDL 不可回滚**：MySQL / MariaDB 的 DDL 语句一旦执行
  就不能随事务回滚。结构模式的预览里会明确提示，
  点确认前请先核对预览的 SQL。
- 切换数据库：与 PostgreSQL 相同，连接内切换时会重连。
- 驱动走 rustls，不依赖 OpenSSL。
