# slowcode

一个用 Rust 实现的认知协作式 coding agent。核心命题是：人创造未知，AI 实现已知。

## 协作模型

**认知协作 (Cognitive Collaboration)**:
人类保留未知领域的创造与判断，Agent 承担已知领域的模板化工作。
_Avoid_: AI 辅助编程、结对编程

**代码失控**:
用户无法解释 AI 生成代码的行为与理由，导致心智模型与实际代码脱节。这是本项目要解决的一等问题。
_Avoid_: 技术债、bug 增多

**心智模型**:
用户对代码为什么这样写、在什么条件下会失败的内在解释。
_Avoid_: 理解程度、熟悉度

**已知域 / 半已知域 / 未知域**:
按认知缺口划分的三层，不按任务难度划分。已知域是 Agent 能依靠仓库、对话与通用知识产出核心逻辑或决策，并能指出仓库证据；半已知域是有已知模式但仍需领域权衡或用户决策；未知域是核心逻辑或决策依赖仓库与对话之外的知识、价值或约束，只能由用户提供。不确定时向上归类。只有已知域可以进入 Execute；半已知域必须先决策并重新分类；未知域必须先 scaffold。
_Avoid_: 简单任务 / 中等任务 / 困难任务

**认知缺口 (Cognitive Gap)**:
Agent 无法从代码库推断、只有用户具备的领域知识或判断。
_Avoid_: 需求不明确、信息不足

**角色 (Role)**:
slowcode 装载的职责单元，只有导航者与驾驶员两种。
_Avoid_: explorer、distiller、verifier

**能力档 (Capability profile)**:
导航者之上的一组只读能力组合，当前有 coach 与 auditor。能力档不是角色，不出现在 `/role` 里，也不改变用户可见角色；提示能力临时启用时写 `capability.override`，不写 `role.switched`。
_Avoid_: 第三种角色、子角色

**角色协商**:
按任务粒度动态决定谁驾驶、谁导航的过程，而不是一次性分配固定角色。
_Avoid_: 权限切换、模式切换

**导航者 (Navigator)**:
做方向性决策的一方：选方案、调约束、标记认知缺口。
_Avoid_: 审阅者、审批者

**驾驶员 (Driver)**:
执行具体动作的一方。已知域由 Agent 驾驶，未知域由用户驾驶。
_Avoid_: 执行者

**最小深度链路 (Minimum-depth path)**:
从输入到输出、只保留一条核心逻辑、剥离分支与边界处理的端到端最窄路径；核心实现留给用户。
_Avoid_: MVP、最小可行产品

**证据包**:
探索阶段产出的、带最小验证实验证据的候选方案集合，用于支撑方向决策。
_Avoid_: 计划书、方案文档

**认知任务分配表**:
Plan 阶段 Navigate 的产出，按任务列出认知属性、判定依据、Agent 行为、用户角色、最小深度链路与验收。
_Avoid_: 待办列表、任务清单

**判定依据**:
认知任务分配表中记录分类理由的列。已知域必须引用文件、API 或既有模式；指不出证据时降级。
_Avoid_: 备注、理由（过于宽泛）

**执行状态**:
认知任务分配表中表示执行门槛的列。可执行只用于已知域且无未决依赖；待决策用于半已知域，或被未决决策阻塞的已知域；待用户实现只用于未知域。
_Avoid_: 优先级、进度

**半已知域决策**:
Plan 阶段为半已知域任务列出的选项、推荐与待答问题；回答并重新分类前不得执行。
_Avoid_: 审批、确认

**决策记录**:
Plan 之后追加的决策与重新分类表；原计划表保持不变，后续命令读取最新有效状态。
_Avoid_: 审批记录、会议纪要

**分类批准 (Classification approval)**:
Review 阶段用户执行 `/approve`，把当前 `state_version` 与任务表 hash 绑定为已批准状态。之后任何行变化都会使该行重新待确认，未批准时不得进入 Scaffold 或 TestFix。
_Avoid_: 计划审批、门禁放行

**工作流日志 (Workflow journal)**:
workflow 的事实源，由 `.scratch/workflows/<workflow_id>/state.json`、`events.jsonl`、用户输入快照与测试输出组成。与会话 JSONL 分开，artifact 只是它的渲染视图。
_Avoid_: 会话记录、memory、缓存

**Run 级状态 (Run state)**:
一次可执行单元的生命周期，由 `slowcode-runtime` 的 `RunState` 表示。状态转移显式写死，checkpoint 落在 run 日志；不包含 Plan 阶段、问题与门禁。
_Avoid_: workflow 状态、任务状态

**Run 日志 (Run journal)**:
run 级事实源，位于 `.scratch/workflows/<workflow_id>/run-states/<run_id>/`，包含 `state.json` 与 `events.jsonl`。通过 `workflow_id` 关联工作流，但 runtime 不解析这个值。
_Avoid_: 工作流日志、会话日志

**活跃 workflow (Active workflow)**:
一个 workspace 当前可继续操作的 workflow。同一 workspace 同时只允许一个，索引在 `.scratch/workflows/index.json`，用 `/workflow [list|use <id>|end]` 管理。`ended` 是显式终止，不复用 `Done`。
_Avoid_: 当前计划、打开的计划

**测试运行器适配 (Test runner adapter)**:
把项目测试命令、测试枚举与选择器差异封装起来的一层。v1 支持 cargo 与 pytest，其它项目退化为整体退出码，不伪造逐个测试的结果。
_Avoid_: 测试插件、测试驱动

**测试执行器 (TestExecutor)**:
系统级的固定测试执行能力，只运行 `test_contract` 里的命令与选择器，不接受模型临时拼的命令。用于 Review baseline、AwaitUser 的 `/continue` 与 TestFix 的 `/test`，不属于任何角色的通用 shell。
_Avoid_: 测试工具、shell 测试

**测试契约 (TestContract)**:
TestFix 固定使用的测试命令、选择器、期望失败与期望通过测试，以及被锁定的框架与测试文件 hash。模型不能改写契约，契约变化按 `Blocked` 或回到 Review 处理。
_Avoid_: 测试配置、测试命令

**测试选择器 (Test selector)**:
从任务引用的文件推导出的 package 或目录范围，用于把 `/test` 限定在相关测试。cargo 用多个 `-p`，pytest 用公共目录；推不出时在 Review 生成 `test_selector` 问题。
_Avoid_: 测试过滤、测试路径

**失败分类 (Failure classification)**:
`/fix` 对每个失败给出的固定类别：`known_regression`、`test_contract_error`、`unknown_gap`、`environment`、`flaky`。类别决定下一阶段，不由模型单方面决定未知域。
_Avoid_: 失败原因、错误类型

**修复轮次 (TestFix round)**:
一次 `/test` 的红测记录。连续相同失败集合与轮次上限触发 `Blocked`，保留最后一次失败证据。预算来自 `.slowcode/workflow.toml`，`/status` 显示生效值与来源。
_Avoid_: 重试次数、迭代

**暂停 workflow (Paused workflow)**:
不在当前操作焦点、但保留完整状态的 workflow。`/workflow use` 把旧 active 置为 paused，允许同一 workspace 保留多个 paused 但最多一个 active。
_Avoid_: 关闭的计划、后台计划

**模型调用记录 (Model call record)**:
`model.called` 摘要事件与 `runs/<turn_id>/` 下的完整请求响应。摘要进事件日志，全文默认保留 30 天，写入前脱敏。
_Avoid_: prompt 日志、聊天缓存

**只读 shell**:
受限的 shell 能力，只允许白名单子命令，拒绝管道、重定向与写文件参数，限制在 workspace 内并记录 `shell.readonly` 事件。导航者、coach 与 auditor 可用。
_Avoid_: 受限模式、安全 shell

**脚手架清单**:
Scaffold 阶段输出的表，列出任务、文件、留空位置与验收命令。
_Avoid_: 文件列表、TODO 列表

**执行报告**:
Execute 阶段输出的表，列出任务、结果与证据。
_Avoid_: 完成报告、总结

**未知域暂停**:
遇到未知域任务时停止执行，列出需要用户决策的问题，等待用户回答后再进入 scaffold。
_Avoid_: 阻塞、等待确认

**设计对抗 (Design Debate)**:
出 ADR 前，用户粗稿与 AI 盲稿的冲突对照和一轮抗辩，用于补充认知任务分配表。
_Avoid_: 评审会、辩论赛

**盲稿 (Blind AI Draft)**:
AI 在未看用户粗稿的情况下独立写出的设计版本。
_Avoid_: AI 版、机器稿

**Plan 模式**:
管理用户认知负荷的决策层，而不是等待批准的安全门禁。
_Avoid_: 计划审批、只读模式

**规划四阶段 (Plan phases)**:
探索 (Explore) → 导航 (Navigate) → 执行 (Execute) → 沉淀 (Distill)。

## 认知机制

**认知负荷**:
任务对工作记忆的负担。内在负荷来自任务本身的复杂度；外在负荷来自与学习无关的干扰；关联负荷用于构建心智模型。
_Avoid_: 工作量大、难度

**认知卸载 (Cognitive Offloading)**:
把本应形成心智模型的思考交给 AI 完成，导致表面理解。
_Avoid_: 提效、自动化

**生成效应 (Generation Effect)**:
自己生成答案比识别答案带来更好的学习与长期保持。

**合意困难 (Desirable Difficulties)**:
训练期看似增加困难、长期却增强保持与迁移的安排，例如间隔练习、交错练习、以测试代替呈现。
_Avoid_: 刻意加难、用户体验优化

**Teach-back**:
用户向 Agent 解释自己的实现决策，Agent 据此生成理解漏洞报告。
_Avoid_: 代码审查、验收测试

**理解漏洞报告**:
Teach-back 的产物，指出用户解释不清或未覆盖的边界条件。
_Avoid_: 评分、考试结果、理解缺口报告

**追问 (Follow-up)**:
理解漏洞报告里唯一的具体问题。系统把它生成为一个知识问题，用户必须回答或显式接受剩余缺口后 workflow 才能完成。
_Avoid_: 评分问题、补充作业

**剩余缺口 (Remaining Gap)**:
用户选择不继续回答追问时显式接受的未澄清部分。接受会写 `distill.gaps_accepted` 审计事件并结束 workflow。
_Avoid_: 忽略、跳过、已解决

**提示请求 (Hint)**:
用户在当前 workflow 内要求一次只读分析。`/hint` 以 coach 能力档运行一次，写 `capability.override`，不改变阶段、角色或工作区。
_Avoid_: 代做、自动修复、隐藏提示

**只读 shell (Read-only Shell)**:
只接受严格 argv 的受限命令工具，当前允许 `git status`、`git diff`、`git log`、`git show` 与 `ls`。不解析 shell 字符串，不允许管道、重定向、写文件参数或越界路径。
_Avoid_: bash、任意命令、白名单外命令

**认知档案**:
内部、多维、按领域区分的理解状态信号，只用于调节任务难度，不对外评价。
_Avoid_: 分数、等级、能力评级

## 知识沉淀

**孵化器 (Incubator)**:
捕捉对话副产品中的半成型想法，按日扫描、按周连接。
_Avoid_: 收件箱、待办列表

**文档站**:
存放筛选后解释性知识的结构化空间，回答“为什么是这样”。
_Avoid_: wiki、全量对话记录

**技能库**:
存放可复用操作模式的库，回答“怎么做”。
_Avoid_: 代码片段库、prompt 集合

**Learn-from-success**:
把成功任务的轨迹提取、参数化、验证后写入技能库的闭环。
_Avoid_: 经验总结、复盘

**活索引**:
随新增内容自动更新、记录条目之间连接的知识索引。
_Avoid_: 目录、静态索引

## 工程

**轮子**:
复用的现成实现或参考项目。
_Avoid_: 依赖、第三方库

**能力簇 (Capability cluster)**:
slowcode 的能力坐标轴，A–I 九簇：Agent 内核、认知协作 Plan 模式、角色/能力运行时、技能体系、文档站 / 技能库 / 孵化器、前缀缓存友好的 prompt 装配、Subagent、代码图谱、隐私与审计。

**角色装载器**:
动态加载与卸载角色及其能力的那一层运行时。

**角色 fiber**:
一个角色在 Cordis 运行时中的生命周期单元，负责 apply 时激活能力包、dispose 时释放。
_Avoid_: 角色实例、插件进程

**会话续接**:
角色切换时把同一个 Session 交给新装配的 AgentLoop，不复制也不重建对话历史。
_Avoid_: 上下文迁移、会话转移

**会话持久化**:
把一次运行的 Session 事件写入 kernel 的 JSONL 日志，使下一次进程能用同一 session id 恢复。
_Avoid_: 聊天记录、历史缓存

**会话恢复 (Resume)**:
从已有 JSONL 重新扫描、校验并提交恢复后缀，得到可继续使用的 Session。结构性损坏报错，不重建。
_Avoid_: 重放、重新开始

**角色过渡提示 (Role transition notice)**:
角色切换后第一轮注入的插件通知，说明当前角色与工具边界，并把历史中关于工具可用性的陈述限定为上一个角色。
_Avoid_: 切换消息、系统提示

**验收文档**:
记录一个里程碑怎么验收、用户实际怎么用、运行证据与已知限制的文档，随发布版本维护。
_Avoid_: 测试报告、发布说明

**Trace ID**:
一次可回放运行的稳定标识。slowcode 里等于 session id，格式为 `session-<uuid>`。
_Avoid_: 请求 ID、日志 ID

**反馈现场包**:
由 `scripts/feedback.sh` 生成的目录，含 issue-ready 的 `issue.md`、原始 `session.jsonl`、`metadata.json` 与可选 Plan artifact。
_Avoid_: 错误日志、截图

**脚手架契约 (Scaffold contract)**:
由系统从 scaffold manifest 与 workspace 推导的机器可读约定，记录核心区、框架文件 hash、测试文件 hash、固定测试命令、期望失败测试与期望通过测试。模型只能产文件与 manifest，不能自证契约。
_Avoid_: 模型自述契约、手写 JSON 契约

**核心留空 (Core marker)**:
用户在 AwaitUser 阶段必须实现的代码位置，统一标记为 `TODO(user)`。一个核心文件只允许一个标记，`/continue` 要求标记全部消失。
_Avoid_: 待办、占位实现

**Baseline**:
scaffold 或 Review 批准前对项目测试的一次快照运行。baseline 已失败的测试算 pre-existing failure，不进入 `expected_failing_tests`。
_Avoid_: 起点测试、初始结果

**系统级测试执行器 (TestExecutor)**:
只接受契约里的固定命令与已校验 selector 的测试运行入口。Review baseline、`/continue` 与 `/test` 共用它，角色不需要临时获得通用 shell 权限。
_Avoid_: 测试脚本、模型拼命令

**技能根目录 (Skill root)**:
角色可贡献的、workspace 相对的 Skill 扫描目录。共享目录是 `.slowcode/skills` 与 `.agents/skills`，角色专属目录是 `.slowcode/roles/<role>/skills`；角色根优先于共享根，目录外的绝对路径与 `..` 逃逸一律拒绝。
_Avoid_: 技能路径、插件目录

**能力 (Capability)**:
一个角色可向 kernel 贡献的一组工具、提示片段与技能来源。
_Avoid_: 插件、Service

**能力提供者 (CapabilityProvider)**:
以一组能力代表某个角色的对象；角色 fiber 通过它把能力交给 kernel。
_Avoid_: 插件接口、服务注册表

**前缀缓存装配**:
把稳定内容放在前缀、易变内容放在后缀的 prompt 组织方式，目的是提高 provider 缓存命中。
_Avoid_: 提示词优化

**一等公民**:
设计中的第一性原则。本项目的一等公民是认知负荷与代码失控，其余原则服务于它。
_Avoid_: 核心功能

**MCP 安装简写 (MCP source shorthand)**:
`/mcp add <owner>/<repo>` 形式。解析仓库对应的 npm 包，锁定精确版本，生成 stdio 配置，不 clone、不构建。
_Avoid_: MCP 插件市场、MCP 一键安装

**MCP 来源元数据 (MCP source metadata)**:
`mcp.json` 中 server 项下的 `source` 字段，记录仓库、包、版本、bin 与 integrity，用于展示来源和将来更新。
_Avoid_: MCP 安装记录、MCP 缓存

**MCP 激活 (MCP activation)**:
把 dormant 的项目级 MCP server 连接起来并加入当前会话工具快照的动作。由人通过 `/mcp enable` 触发，首次确认按配置指纹缓存。
_Avoid_: MCP 启动、MCP 加载配置

**MCP 工具授权 (MCP tool grant)**:
已激活 server 内某个工具的会话级允许记录。只读注解和用户级 trusted 自动允许，其余首次调用询问一次。
_Avoid_: MCP 审批开关、全局信任
