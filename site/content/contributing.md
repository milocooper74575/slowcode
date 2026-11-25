# 参与与验证

## 构建与测试

```bash
cargo fmt --all -- --check
scripts/clippy.sh
cargo test --workspace --exclude slowcode-kernel
```

快速循环，安装 `cargo-nextest` 后并行跑测试二进制：

```bash
scripts/test-fast.sh                 # 不含 vendored kernel
scripts/test-fast.sh slowcode-tui    # 单 crate
scripts/test-fast.sh --full          # 含 kernel
```

Kernel 改动单独跑：

```bash
cargo test -p slowcode-kernel --lib
```

真实 provider 测试是 `#[ignore]`，需要 `DEEPSEEK_API_KEY`：

```bash
cargo test -p slowcode-cli --test e2e_deepseek -- --ignored --nocapture
cargo test -p slowcode-cli --test plan_flow_deepseek -- --ignored --nocapture
```

没跑过相关测试，不要说一个改动已经验证。

## 代码规范

- Rust 2024，`rust-version = 1.88`，不引入新 warning。
- 一个决定只放一处；优先用枚举和 newtype 让非法状态不可表示。
- 外部数据在入口校验一次：CLI 参数、配置、JSONL、MCP 响应、skill frontmatter。
- 持久化写入使用临时文件加 `fs::rename`，失败可回滚。
- 错误用 `thiserror`，不引入 `anyhow`。
- 单测放在代码旁的 `#[cfg(test)]`；跨 crate 测试放在 `crates/slowcode-tests/tests/`。
- 小步提交，说明为什么改、改了什么、怎么证明。

## 反馈回归

每个 `/feedback` 问题在关闭前必须变成回归覆盖：

1. 一个针对失败 seam 的单测。
2. 一个覆盖用户可见工作流的端到端测试。

只做手工验证不算完成。

## 文档站与任务源

文档源在 `site/`，生成站点在 `site/dist/`。`site/tasks.json` 是唯一任务状态源，`HANDOFF` 只写交接上下文。

```bash
cargo run -p slowcode-docs -- check        # 校验层级、任务源、主流程
cargo run -p slowcode-docs -- build        # 重新生成 site/dist
cargo run -p slowcode-docs -- build --check  # CI 漂移门禁
```

编辑规则：

- `site/content/*.md` 每一篇都必须挂到 `docs.json`。
- 任务必须写 `docs` 与 `acceptance`；`done` 必须有 `evidence`；`deprecated` 必须有 `reason`；`in_progress` 必须有 `next_action`。
- 依赖必须指向已存在的任务 ID。
- `roadmap.md` 有且只有一个主流程标记段落。
- 改完 `site/` 后重建并提交 `site/dist`，否则 CI 会失败。

## 安全

- key 来自 `.env` 或 `DEEPSEEK_API_KEY`，不打印、不提交。
- MCP 项目级 server 在 `/mcp enable` 前保持 dormant，工具按风险分级审批。
- 反馈包可能包含对话内容，分享前先检查。
- 模型不会安装 skill，也不会批准自己的写入。
