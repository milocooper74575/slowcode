# 会话与 workflow 本地审计站

> 状态：`AUDIT-001` 已实现。生成在 `slowcode-app::workflow::audit_site`，
> 每次 workflow state 写入后刷新；显式重建用 `slowcode audit rebuild [<id>|--all]`。

slowcode 的会话不是一段聊天记录，而是一条 workflow。用户需要能审计、回顾、定位问题，并且随时知道自己在哪个节点、系统做了什么决定。当前 TUI 里的文字版 workflow 只能顺序阅读，节点一多就失去结构；会话历史也散在 JSONL 与终端滚动里。

这份设计把已经落地的文档站能力扩展到产品内部：**每次创建 workflow 时，在 workspace 的专用目录生成一个只读静态审计站**，作为会话与 workflow 的投影。它不是第二个状态源，任务与决策仍以 workflow state、run journal 和 `site/tasks.json` 为准。

## 目标

- 本地可打开、离线可用，不依赖服务器或前端框架。
- 一页看清当前节点、阶段门禁、预算、审批与未决问题。
- 能回放会话历史、工具调用、模型调用与错误，便于定位问题。
- 把 feedback 包作为 workflow 的补充材料放进站内，而不是另起一套记录。
- TUI 底部常驻入口，用户随时知道审计站在哪里。

## 产物布局

建议落在 workspace 的专用目录，按 workflow 分区：

```text
<workspace>/.slowcode/audit/<workflow_id>/
  index.html        # 总览：当前节点、阶段、门禁、预算、待决问题
  workflow.html     # 节点时间线、状态转移、审批与证据
  session.html      # 会话与工具调用，可分段或分页
  feedback.html     # feedback 包与关联的节点/轮次
  assets/style.css  # 复用 slowcode-docs 的离线样式
  assets/data.json  # 站内搜索/筛选用的只读投影
```

`index.html` 是稳定入口；workflow id 变化时旧目录保留，便于回看，清理策略单独决定。

## 页面内容

### 总览

- workflow id、任务标题、当前 phase 与节点。
- 阶段门禁：已批准、待用户实现、测试是否翻绿、distill 是否完成。
- 预算：已用 token、剩余预算、是否 Blocked。
- 审批记录：谁批准、参数 hash、策略版本。
- 未决问题与下一步。

### workflow 节点

- 节点时间线：`plan -> approve -> scaffold -> continue -> distill -> feedback` 的实际发生顺序与时间。
- 每次状态转移的原因与证据引用。
- 任务表 hash、测试契约 hash、artifact 路径。
- 失败与重试：工具结果未知、恢复动作、补偿结果。

### 会话

- 用户消息、assistant 文本与推理、工具调用与结果。
- 模型调用归档：请求/响应引用、token、缓存命中、脱敏记录。
- 错误与取消：错误码、停止原因、是否重试。
- 默认只展示摘要，展开后加载详情，避免单页过大。

### feedback

- 每个 feedback 包一页，带问题描述、期望、实际、复现步骤与附件。
- 关联到具体 workflow 节点与会话轮次，方便从问题回到现场。
- feedback 可能包含对话内容，站内只做本地展示，分享前仍需用户确认。

## 生成时机

1. `/plan` 创建 workflow 时生成骨架、入口与当前节点页。
2. 每次 workflow 事件或 run 状态提交后，更新受影响的页面；写入使用临时文件加 rename，避免半页。
3. 生成失败不阻塞主流程，但在 TUI 显示可恢复的告警，并保留上一次完整版本。
4. 提供显式命令 `slowcode audit rebuild [<workflow_id>|--all]` 重新生成整站。它不是日常写入路径，只在下面几类情况使用：
   - 渲染器或数据结构升级：旧目录需要按新格式重生成，增量更新回填不了历史页面。
   - 上次生成失败留下的缺口：失败事件当时没有投影，需要从 durable 源对齐。
   - 脱敏规则变更：必须用新规则重写所有旧页面，才能清除已写入 HTML 的敏感内容。
   - 目录迁移或导入：workspace 换路径、从备份拷回 `.scratch/workflows`，但没带 `audit/`。
   - 一致性校验：全量重建后与现有 HTML 对比，可作为漂移检测手段。

## 还原契约

审计站 HTML 是纯投影，不是源。还原能力由这一点决定：

- 唯一源是 `.scratch/workflows/<workflow_id>/state.json`、`events.jsonl`、会话 JSONL、plan artifact、model call archive 与本地 `feedback/`。
- 日常增量生成与显式重建走同一个纯函数 `render(state, events, sessions, feedback)`，显式重建不引入第二条代码路径。
- 手工编辑 HTML 无意义，下一次生成会覆盖；任何状态修正都改源，不改进站。
- 与文档站的区别：`site/dist/` 是 committed 的，源在 `site/content/*.md`，可 checkout 后重建；审计站的源全部 gitignored 且只在本机，**HTML 丢失后只能在源仍在的同一台机器或同一份备份上还原**。`.scratch` 与会话 JSONL 都丢失时无法还原。
- 会话 JSONL 位于平台 application-state 目录（`DSH_SESSION_ROOT`），不在 workspace，`state.json` 只记录 `session_id`。只拷 `.scratch` 而不拷会话日志时，workflow 时间线可还原，但会话页只能降级。

### 缺源降级

每个页面必须显式报告源状态，不允许静默留白或伪装完整：

- 页面顶部显示 `sources: state ✓ / events ✓ / session ✗ / feedback ✓`。
- 缺会话日志时 `session.html` 显示 `source missing: session journal <id>`, 其余页照常生成。
- `events.jsonl` 只有有效前缀时，时间线显示 `recovered prefix: N events`, 并标注之后可能有缺口。
- 缺 feedback 包时 `feedback.html` 显示空态，而不是报错中止。
- 任一源缺失都不应让整站生成失败；只有 workspace 与 workflow 目录都不可写时才告警。

## TUI 变化

- 底部常驻一行入口，例如 `audit: .slowcode/audit/<workflow_id>/index.html`。
- 删除完整的文字版 workflow 节点列表，只保留当前节点、阶段与门禁的紧凑摘要。
- `/status` 继续输出精简状态；需要完整视图时打开审计站。
- 终端不支持点击时，至少提供可复制的绝对路径或打开命令。

## 非目标

- 不做托管服务，不做多用户共享。
- 不复制状态：审计站只读投影，不能修改 workflow。
- 不把密钥、完整环境变量或未脱敏的 feedback 写入站内。
- 不替代 `site/tasks.json`；产品任务仍只记录在那里。

## 验收

- 创建 workflow 后，专用目录存在可离线打开的 `index.html`。
- 总览、节点、会话、feedback 四类信息都能从入口到达。
- TUI 底部常驻入口；文字 workflow 只保留精简摘要。
- 审计站内容与 workflow state、run journal、会话 JSONL 一致。
- 断网可打开，站内链接全部可解析。
- 生成失败有告警且不阻塞 workflow。
