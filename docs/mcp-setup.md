# MCP 配置速查

fastgrid 内嵌 MCP server，让外部 AI 客户端安全地查库。
完整安全模型（只读默认、写审批、审计）见
[mcp-security.md](mcp-security.md)，这里只写接线。

## stdio（推荐，本地客户端）

```sh
fastgrid-cli mcp --stdio
```

- Claude Code（终端）：
  `claude mcp add fastgrid -- /path/to/fastgrid-cli mcp --stdio`
- Claude Desktop / Cursor（MCP 设置文件）：
  ```json
  { "mcpServers": { "fastgrid": {
    "command": "/path/to/fastgrid-cli",
    "args": ["mcp", "--stdio"]
  } } }
  ```
- 数据目录默认与 GUI 共用，`FASTGRID_DATA_DIR` 可覆盖。
- 无头模式下 `FASTGRID_KEYCHAIN=file` 可避免钥匙串不可用
  （没有系统钥匙串的机器上需要）。

## HTTP（回环）

```sh
fastgrid-cli mcp --http [--port N]
```

- 只绑 `127.0.0.1`；启动时把 URL 与 bearer token 印到
  stderr，GUI 启动页的 MCP 面板里也能看到与轮换 token。
- 客户端配置：`url` 填 `http://127.0.0.1:PORT/mcp`，
  header 带 `Authorization: Bearer <token>`。
- token 存在设置里，每次请求重读，GUI 里轮换后旧 token
  立即失效。

## 写操作的三道门

默认全部只读。写要同时满足：

1. 连接的 `mcp.allow_write` 开了；
2. 语句本身不在只读白名单外（分类层先拦一遍）；
3. GUI 里逐条批准（5 分钟超时，超时按拒绝算）。

无头模式没有 GUI 可批准，所以一律拒绝写——这是设计，
不是缺功能。每次调用（不论读写）都会写审计日志，
启动页审计面板可看最近 200 条、过滤并导出 CSV。

## 工具一览

`list_connections`、`list_namespaces`、`list_tables`、
`describe_table`、`run_query`、`explain`、`sample_rows`、
`list_relations`、`find_references`、`execute_write`，
外加每连接的 schema resource（256 KiB 上限，超限截断
后仍是合法 JSON）。生产连接默认不对 MCP 暴露；
一个连接在 MCP 里“不存在”和“被隐藏”返回同样的错误，
无法从外部区分。
