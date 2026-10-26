# PostgreSQL 连接指南

## 表单字段

- **Host / Port**：默认端口 5432。
- **User / Password**：密码只在表单里输入一次，保存后加密
  落盘，启动页不会把它读回显示。
- **Database**：可空；不填时连到服务器的默认库。
- **Socket**：填 Unix socket 目录时走本地 socket 而不是 TCP。
- **TLS**：模式默认 Prefer（有 TLS 就用，没有就明文）。
  可选 Disable / Require / VerifyCa / VerifyFull；
  VerifyCa / VerifyFull 需要 CA 证书路径；需要时还可填
  客户端证书与密钥、以及覆盖证书里校验的 server 名称。
- **SSH 隧道**：可配跳板机 host、用户与认证方式
  （Password / Private key / SSH agent），走隧道时上面
  的 Host / Port 指的是隧道远端看到的地址。

## 连接串

启动页的 URL 框接受 `postgres://`（或 `postgresql://`）
连接串，粘贴后解析进表单，例如：

```text
postgres://user:pass@db.example.com:5432/app?sslmode=verify-full
```

`sslmode` 的取值与 TLS 模式一一对应
（disable / prefer / require / verify-ca / verify-full）；
密码里的特殊字符要做 URL 百分号编码。MySQL 侧的
`sslMode=VERIFY_IDENTITY` 拼法对 postgres 串同样接受。

## 行为说明

- 切换数据库：网络型连接在连接内切换数据库时服务会重连
  （USE-without-reconnect 尚未实现），切换后正在跑的查询
  与暂存编辑不会带过去。
- 本地验证过的版本是 PostgreSQL 16；驱动走 rustls，
  不依赖 OpenSSL。
