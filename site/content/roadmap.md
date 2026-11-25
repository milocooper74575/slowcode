# 路线与主流程

<!-- docs:main-flow:start -->
plan -> approve -> scaffold -> continue -> distill -> feedback
<!-- docs:main-flow:end -->

主流程只有一个：先把未知域问清楚，再让 agent 写已知域，最后用 Teach-back 检查理解。

## 阶段划分

| 阶段 | 主题 | 退出条件 |
|---|---|---|
| P0 基座 | kernel、runtime、roles、只读工具 | 单测、集成测试与恢复语义稳定 |
| P1 Plan workflow | 认知任务表、访谈、门禁、distill | 从 `/plan` 到 `/distill` 能闭环 |
| P2 使用与发布 | TUI、CLI、文档站、MCP、skills、update | 用户可安装、可恢复、可审计 |
| P3 运营与生态 | 审计指标、skill 生态、多模型路由 | 有可重复的运营与评估流程 |

## 主流程每一步

1. **plan**：`/plan <task>` 在一个只读 turn 内完成 Explore 与 Navigate，输出证据包、认知任务分配表与半已知域问题。
2. **approve**：用户逐题回答，系统记录决策并把任务重新分类；`/approve` 批准分类表并绑定 hash。
3. **scaffold**：只为已知域生成脚手架与契约测试；未知域留出 `TODO(user)` 空白核心。
4. **continue**：用户实现核心后 `/continue` 检查标记、文件 hash 与契约，未完成不放行。
5. **distill**：`/distill <explanation>` 做 Teach-back，输出理解漏洞报告与一次修复尝试。
6. **feedback**：`/feedback` 把会话整理成可回归的问题包；修复必须同时补单测与用户路径测试。

## 当前焦点

文档站与唯一任务源已经落地：`site/tasks.json` 是任务状态的唯一来源，`site/dist` 是可离线打开的静态站。下一步是把同一套离线站能力扩展为产品内的会话与 workflow 本地审计站（`AUDIT-001`），让用户随时回看节点、证据与 feedback。详见 [会话与 workflow 本地审计站](session-audit-site.md)。
