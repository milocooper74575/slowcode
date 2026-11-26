# slowcode

Rust 写的认知协作式 coding agent。

> 人创造未知，AI 实现已知。

slowcode 不是让 AI 多写代码。它在关键节点停下来，把创造空间留给人类：先拆认知任务，再区分已知域、半已知域与未知域，跳过需要人判断的部分，只实现已经讲清楚的部分。

![status](https://img.shields.io/badge/status-early%20preview-orange)

## 你可能不需要 slowcode

slowcode 主动放慢编码节奏，把一部分实现交回给人。这是一笔成本。如果符合下列任意一条，用别的工具会更合适：

1. 追求绝对的编码速度，而不关心编码质量。
2. 没有因为 AI coding 而感觉到代码失控，或是认知失调。
3. 不使用 AI coding。

这三条里只要中了一条，slowcode 的门禁、访谈和 Teach-back 对你都是纯粹的摩擦。

## 它能做什么

- **Plan workflow**：`/plan` 生成认知任务分配表，逐题访谈，`/approve` 后按分类进入脚手架或测试修复，最后用 `/distill` 做 Teach-back 与理解漏洞报告。
- **阶段门禁**：未批准不能 scaffold，未实现核心不能继续，测试未绿不能 distill，预算耗尽进入可恢复的 Blocked。
- **角色切换**：navigator 只读，driver 读写并运行 shell，同一会话内按阶段或用户命令切换。
- **本地审计站**：`slowcode audit rebuild` 把 workflow 事件、会话、审批与 feedback 还原成离线站点，源文件缺失时降级展示而不是报错。
- **文档站与唯一任务源**：`site/tasks.json` 是仓库唯一的任务状态源，`slowcode-docs check/build` 校验文档层级、任务字段与主流程，`build --check` 用在 CI 防漂移。
- **可追溯的会话**：JSONL 事件与 run journal 都 append-only，session id 即 traceId，支持 `--resume`；模型调用与审批留档，`/feedback` 从会话日志生成可提交 issue 的反馈包。
- **上下文与交接**：AGENTS.md 状态、GLOSSARY 稳定前缀、HANDOFF 指针，`/quit` 输出交接信息。
- **标配能力**：TUI、MCP、本地 Skills 都有，细节见文档站。

## 文档与进度

- 离线文档站：[`site/dist/index.html`](site/dist/index.html)
- 唯一任务源：[`site/tasks.json`](site/tasks.json)
- 文档源与编辑规则：[`site/README.md`](site/README.md)

文档站的源文件在 `site/`，生成产物在 `site/dist/`。任务状态只写在
`site/tasks.json`，`HANDOFF` 只保存交接上下文。改完 `site/` 后运行：

```bash
cargo run -p slowcode-docs -- check
cargo run -p slowcode-docs -- build
```

## 快速开始

### 安装预编译包

macOS 与 Linux 用户不需要安装 Rust，也不提供 dmg/pkg。直接运行安装脚本：

```bash
curl -fsSL https://raw.githubusercontent.com/milocooper74575/slowcode/main/install.sh | sh
```

想先检查脚本再运行，可以下载到本地：

```bash
curl -fsSL https://raw.githubusercontent.com/milocooper74575/slowcode/main/install.sh -o install.sh
less install.sh
sh install.sh
```

固定版本或自定义安装目录，通过环境变量传给脚本：

```bash
SLOWCODE_VERSION=v0.1.0 sh install.sh
SLOWCODE_INSTALL_DIR="$HOME/bin" sh install.sh
```

脚本会校验 sha256，然后安装 `slowcode` 并打印版本。支持：

- macOS Apple Silicon：`aarch64-apple-darwin`
- macOS Intel：`x86_64-apple-darwin`
- Linux x86_64：`x86_64-unknown-linux-musl`
- Linux aarch64：`aarch64-unknown-linux-musl`

fork 或镜像可以用 `SLOWCODE_REPO=owner/slowcode` 覆盖仓库。

Windows 原生包暂未提供。

macOS 预编译包当前未做 Apple 签名与 notarization。通过 `curl` 安装不会带上
Gatekeeper 的 quarantine 属性，所以终端里通常可以直接运行。如果系统仍拦截，
可以在系统设置中允许，或改为从源码构建。签名会在拿到证书后补上。

### 配置 DeepSeek API key

安装后第一次运行不需要提前准备 `.env`。直接启动 TUI，按提示输入 `/login`，
把 key 粘贴进去即可：

```bash
slowcode --workspace /path/to/project
# 在输入框里输入 /login
```

`/login` 会把 key 写进 workspace 的 `.env`（权限 `0600`，临时文件加 rename），
输入过程以 `*` 遮蔽，key 不会出现在对话、日志或 shell history 里。当前进程立即
生效，无需重启。已配置时再执行 `/login` 会先显示当前来源，然后直接进入遮蔽
输入以替换 key；`/logout` 会删除 `.env` 里的那一行。key 只在 `/login` 的遮蔽
输入中被接受，不会被当成普通任务发给模型。

也可以继续用环境变量或手写 `.env`：

```bash
export DEEPSEEK_API_KEY=...
```

workspace `.env` 里的 key 优先于环境变量。headless 模式在没有 key 时不会调用
模型，而是打印上面几种配置方式后退出。

### 卸载

卸载只删除二进制，不会动 workspace 里的会话与工作流数据：

```bash
sh install.sh --uninstall

# 或直接删除，默认安装目录是 ~/.local/bin
rm ~/.local/bin/slowcode
```

如果装到了自定义目录，卸载时传入同一个 `SLOWCODE_INSTALL_DIR`：

```bash
SLOWCODE_INSTALL_DIR="$HOME/bin" sh install.sh --uninstall
```

workspace 内的 `.slowcode/` 与 `.scratch/` 需要手动删除。

### 更新

slowcode 不会在后台静默替换自身。检查更新只是只读的网络请求，真正的替换必须
由用户确认：

```bash
slowcode update --check   # 只报告当前版本与最新版本
slowcode update           # 下载、校验 sha256、替换当前可执行文件
```

release 构建的 TUI 启动后会在后台查询最新 release（2 秒超时），结果缓存 24
小时。发现新版本时只显示一行提示，不弹模态、不打断当前 turn：

```text
update available: 0.2.0 (current 0.1.0); type /update
```

输入 `/update` 会先确认 `y/n`，确认后在一个后台任务里下载并替换，完成时提示
重启。当前进程仍然运行旧代码，必须重启才会生效。`slowcode update` 与 TUI
共用同一条下载、校验与替换路径。模型不能触发更新，更新也不会写入
`.slowcode/`、`.scratch/` 或 workspace。

启动检查是默认开启的，但以下情况会跳过：

- `SLOWCODE_UPDATE_CHECK=0`（也接受 `false`、`no`、`off`）
- 设置了 `CI`
- debug 构建（`cargo run` / `cargo build`）
- `SLOWCODE_VERSION` 固定版本时抑制更新提示，且 `slowcode update` 会拒绝执行

`slowcode update --check` 是显式命令，不受启动检查的开关影响，但仍复用 24 小时
缓存。包管理器安装的路径（Homebrew 的 `/Cellar/` 或 `/opt/homebrew/`）不会
自更新，会提示改用 `brew upgrade slowcode`。安装目录不可写时报明确错误，不会
尝试提权。Windows 原生自替换暂不支持，会提示重新运行安装脚本。

为什么不做自动更新：替换二进制是供应链敏感且不可回滚的动作。sha256 与发布包
同源，只能防止传输损坏，不能防御发布渠道被攻破；签名验证尚未实现。自动检查
更新是低风险、可缓存的只读动作，替换必须留给用户。

### 从源码构建

从源码构建需要 Rust 1.88 或更高。

```bash
git clone <your-repo-url>
cd slowcode

cp .env.example .env
# 在 .env 中填写 DEEPSEEK_API_KEY

cargo build --release -p slowcode-cli
./target/release/slowcode --workspace /path/to/project
```

没有参数、只有 flag 或使用 `tui` 子命令都会进入 TUI。

也可以直接从环境变量读取 key：

```bash
export DEEPSEEK_API_KEY=...
```

默认模型是 `deepseek-flash`，可用 `--model` 覆盖。`--max-tokens` 与 `--context-window` 可覆盖限制。

## 使用方式

```bash
# TUI
cargo run -p slowcode-cli -- --workspace <dir>

# headless 单次任务
cargo run -p slowcode-cli -- run --workspace <dir> "任务"

# 简单 REPL
cargo run -p slowcode-cli -- repl --workspace <dir>

# 写文件与 shell 前审批
cargo run -p slowcode-cli -- --ask --workspace <dir>

# 只读
cargo run -p slowcode-cli -- --read-only --workspace <dir> "只读任务"

# 新建会话并打印 session id
cargo run -p slowcode-cli -- run --workspace <dir> --session new "任务"

# 恢复同一会话
cargo run -p slowcode-cli -- run --workspace <dir> --session <id> --resume "继续"
```

`--ask` 需要可交互终端。非 TTY 环境会以 `approval_required` 退出，而不是静默放行。

## Plan workflow

```text
/plan <task>
  -> Interview：逐个回答半已知域与未知域问题
  -> /approve：批准分类，绑定任务表 hash
  -> /scaffold：生成框架、契约测试与 TODO(user) 核心区
  -> 用户实现核心
  -> /continue：检查标记、文件 hash 与契约翻绿
  -> /test 或 /fix 或 /execute：固定测试命令，只修已知域失败
  -> /distill <explanation>：输出理解漏洞报告
  -> /answer：回答追问或接受剩余缺口，进入 Done
```

常用命令：

| 命令 | 作用 |
|---|---|
| `/plan <task>` | 创建 workflow，生成任务表与问题队列 |
| `/next` / `/status` | 查看下一个问题或当前阶段 |
| `/answer [<text>]` | 回答当前问题，支持 `#id` 与 `@file` |
| `/reclassify <text>` | 主动提出新的未知域 |
| `/approve` | 批准当前分类表 |
| `/scaffold` | 生成脚手架与测试契约 |
| `/continue` | 用户声明核心已完成，触发检查 |
| `/test` / `/fix` / `/execute` | 运行固定测试，修复已知域失败 |
| `/distill <explanation>` | Teach-back 与理解漏洞报告 |
| `/workflow [list\|use <id>\|end]` | 管理 workspace 内的 workflow |

workflow 状态写在目标 workspace 的 `.scratch/workflows/<workflow_id>/`，计划 artifact 写在 `.scratch/plans/`。artifact 是人读视图，`state.json` 加 `events.jsonl` 才是事实源。

## 角色与安全

- navigator 只读，driver 可写。
- MCP 项目级 server 默认 dormant，`/mcp enable` 激活并按配置指纹缓存信任。
- MCP 工具按风险分级。只读注解与用户级 `trusted` 自动放行，其余本会话首次调用询问。
- 密钥从 `.env` 或环境变量读取。`.env`、本地 session、`.scratch/` 与反馈包都不应提交。
- 反馈包可能包含对话内容，发布前需要人工检查。

## 项目结构

| 路径 | 内容 |
|---|---|
| `crates/slowcode-kernel` | vendored 的 dsh-rs 非 UI 内核，上游信息见 `UPSTREAM.md` |
| `crates/slowcode-roles` | navigator / driver 装配、Cordis fiber、会话续接与 cache 报告 |
| `crates/slowcode-runtime` | run 状态、转移表、预算、审批与模型调用归档 |
| `crates/slowcode-cognition` | Plan 输出校验、认知任务分配表与 artifact 渲染 |
| `crates/slowcode-app` | 会话装配、命令注册表、Plan workflow 与模型切换 |
| `crates/slowcode-cli` | CLI 入口，二进制 `slowcode` |
| `crates/slowcode-tui` | ratatui TUI |
| `crates/slowcode-context` | AGENTS.md、GLOSSARY.md 与 HANDOFF.md 指针 |
| `crates/slowcode-mcp` | MCP 配置、连接与工具调用 |
| `crates/slowcode-skills` | 本地 skill 安装、校验与锁文件 |
| `crates/slowcode-update` | 版本比较、更新检查缓存、校验与自替换 |
| `crates/slowcode-tests` | 跨 crate 集成测试 |

## 开发与测试

```bash
cargo fmt --all -- --check
scripts/clippy.sh
cargo test --workspace --exclude slowcode-kernel
```

`slowcode-kernel` 是 vendored 上游代码。改动它之前先读 `crates/slowcode-kernel/UPSTREAM.md`，并单独运行：

```bash
cargo test -p slowcode-kernel --lib
```

真实 provider 测试默认 ignored，需要 key：

```bash
cargo test -p slowcode-cli --test e2e_deepseek -- --ignored --nocapture
cargo test -p slowcode-cli --test plan_flow_deepseek -- --ignored --nocapture
```

端到端脚本：

```bash
scripts/e2e.sh            # 真实模型主链路
scripts/e2e-tui.sh        # macOS PTY 冒烟
scripts/e2e-tui-turn.sh   # 真实模型审批
scripts/e2e-tui-handoff.sh
scripts/e2e-tui-session.sh
scripts/e2e-mcp.sh
```

后几个脚本会消耗 provider token，部分依赖 macOS PTY 与 `python3`。

## 贡献

欢迎 issue 与 pull request。提交前请：

1. 保持改动聚焦，一个 PR 解决一件事。
2. 补上覆盖改动的测试，单元测试放代码旁，跨 crate 测试放 `crates/slowcode-tests/tests/`。
3. 跑 `cargo fmt --all -- --check`、`scripts/clippy.sh` 与 `cargo test --workspace --exclude slowcode-kernel`。
4. 不提交密钥、session JSONL、`.scratch/` 现场或本地 skill 安装目录。

领域术语见 `GLOSSARY.md`，代码风格见 `CODING_STANDARDS.md`。

## Vendored code

`crates/slowcode-kernel` 来自 [`deepseek-harness-rs`](https://github.com/xizheyin/deepseek-harness-rs)，按 MIT 许可证 vendor。改动记录在 `crates/slowcode-kernel/UPSTREAM.md` 与同目录的 `LICENSE`。

## License

MIT，见 [LICENSE](LICENSE)。
