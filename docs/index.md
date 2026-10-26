# fastgrid 文档

## 使用

- [快速开始](quickstart.md)：安装、第一个连接、日常操作。
- [SQLite 连接指南](connections/sqlite.md)
- [PostgreSQL 连接指南](connections/postgres.md)
- [MySQL / MariaDB 连接指南](connections/mysql.md)
- [快捷键规范](shortcuts.md)
- [诊断、日志与隐私](diagnostics.md)
- [打包与发布](packaging.md)
- [MCP 配置速查](mcp-setup.md)

## 安全

- [MCP 安全模型](mcp-security.md)：只读默认、写审批、审计。
- [加密与同步设计](encryption-and-sync.md)

## 开发

- [架构](architecture.md)
- [数据层抽象与驱动契约](driver-abstraction.md)
- [驱动开发指南](driver-dev.md)
- [数据模型](data-model.md)
- [UI 测试约定](ui-testing.md)
- [贡献指南](contributing.md)
- [路线图](roadmap.md)
- [TablePlus 交互调研](tableplus-interaction-research.md)

> 本目录是文档站的源文件。如果装了 mdbook，
> `mdbook build`（`book.toml` 在仓库根目录）会用
> `SUMMARY.md` 生成静态站；没装也不影响，Markdown
> 本身就是可读的版本。
