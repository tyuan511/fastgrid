# fastgrid

规范以 [`AGENTS.md`](AGENTS.md) 为准（项目规范、crate 边界、代码/测试约定、Git 提交信息格式、安全红线）。
本文件只做转发，不要在这里新增规则，避免两处漂移。

Quick start:

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p fg-app
```
