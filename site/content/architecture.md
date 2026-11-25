# 架构与目录

slowcode 按“内核 → 角色 → 运行时 → 应用 → 界面”分层。每一层只做一个决定，跨层通过显式接口连接。

## crate 分层

| 路径 | 职责 |
|---|---|
| `crates/slowcode-kernel` | 从 dsh-rs 引入的非 UI 内核：agent loop、模型流、会话、工具。改前先读 `UPSTREAM.md` |
| `crates/slowcode-roles` | navigator/driver 组装、Cordis fiber、会话续接与缓存报告 |
| `crates/slowcode-runtime` | Run 状态机、转移、预算、审批与模型调用归档 |
| `crates/slowcode-cognition` | Plan 校验、认知任务表、artifact 渲染 |
| `crates/slowcode-app` | 会话装配、命令注册、Plan workflow、模型切换 |
| `crates/slowcode-cli` | CLI 入口，二进制 `slowcode` |
| `crates/slowcode-tui` | ratatui 用户界面 |
| `crates/slowcode-context` | AGENTS.md 状态、GLOSSARY 稳定前缀、HANDOFF 指针 |
| `crates/slowcode-mcp` | MCP 配置合并、stdio 与 HTTP 连接、工具调用 |
| `crates/slowcode-skills` | 本地 skill 安装、校验、锁文件与移除 |
| `crates/slowcode-feedback` | 从会话 journal 生成可提交的反馈包 |
| `crates/slowcode-update` | Release 检查与校验后的自替换 |
| `crates/slowcode-audit` | 离线会话审计：token、重复读取、空转重试、计划抖动 |
| `crates/slowcode-docs` | 文档站校验、生成与漂移检查 |
| `crates/slowcode-tests` | 跨 crate 集成测试 |

## 一次 turn 的数据流

```text
TUI / CLI
  -> slowcode-app 选择角色与命令
  -> slowcode-roles 组装 AgentLoop
  -> slowcode-kernel 调用模型、分发工具、写入会话
  -> slowcode-runtime 记录 run 状态、预算与审批
  -> JSONL 会话与 .scratch workflow artifact
```

## 会话与可恢复性

- session id 同时是 traceId；会话是 append-only JSONL。
- run 状态包含 `Queued`、`CallingModel`、`AwaitingApproval`、`ExecutingTool`、`Completed`、`Failed`、`Cancelled`。
- `CallingModel` 与 `ExecutingTool` 在崩溃后视为未知结果：先查幂等记录或外部状态，再决定是否重试。
- 工具结果丢失时写入 `TOOL_OUTCOME_UNKNOWN`，但 turn 的结束原因跟随真正的停止原因，而不是一律 `AGENT_TOOL_EXECUTOR`。

## 公开参考

- `README.md`：用户视角的安装与命令。
- `GLOSSARY.md`：领域术语。
- `CODING_STANDARDS.md`：代码风格与 review 规则。
- `site/tasks.json`：唯一任务状态源。
- `site/dist/index.html`：本页所在的离线文档站。

## 相关

- [Plan workflow 与阶段门禁](workflow.md)
- [路线与主流程](roadmap.md)
