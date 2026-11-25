# Plan workflow 与阶段门禁

Plan workflow 是 slowcode 的主路径。它不是“让模型一次写完整份计划”，而是把认知任务分开：已知域由 agent 执行，半已知域先决策，未知域留给用户实现。

## 命令与阶段

| 命令 | 作用 |
|---|---|
| `/plan <task>` | 创建 workflow，收集证据并生成认知任务分配表 |
| `/next` / `/status` | 查看下一个问题或当前阶段 |
| `/answer [<text>]` | 回答当前问题，支持 `#id` 与 `@file` |
| `/reclassify <text>` | 主动提出新的未知域 |
| `/approve` | 批准当前分类表 |
| `/scaffold` | 生成脚手架与测试契约 |
| `/continue` | 用户声明核心已完成，触发检查 |
| `/test` / `/fix` / `/execute` | 固定测试命令，只修已知域失败 |
| `/distill <explanation>` | 输出理解漏洞报告 |
| `/feedback` | 生成可回归的反馈包 |

## 阶段门禁

- 未 `/approve` 不能 scaffold 或进入测试修复。
- 用户未实现核心、文件 hash 或契约未翻绿，不能 `/continue`。
- 测试未绿不能 `/distill`。
- 测试契约锁定框架与测试文件；改动它们会阻塞 workflow。
- `/distill` 必须包含 `## 理解漏洞报告` 的四个部分，并完成一次修复尝试。
- 预算耗尽进入可恢复的 Blocked，而不是继续调用模型。

## 角色边界

- **navigator**：只读。负责 Explore、Navigate、证据收集与问题生成。
- **driver**：读写并运行 shell。只在已知域执行，不能替用户决定未知域。
- 模型不能安装 skill，不能批准自己的写入，也不能把自然语言的“已完成”当成事实。

## 只读工具的错误语义

`readonly_shell` 只接受 argv，绑定工作区，命令限制在 `git status`、`git diff`、`git log`、`git show` 与 `ls`。

参数或路径不合法属于模型错误，返回模型可见的工具结果，让模型改正；它不会把整个 turn 变成 `TOOL_OUTCOME_UNKNOWN`。超时与进程级失败的分类仍是待决策项，见任务源里的 `FIX-002`。

## 相关

- [架构与目录](architecture.md)
- [参与与验证](contributing.md)
