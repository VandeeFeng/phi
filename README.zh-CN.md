# Phi

[Phi](https://en.wikipedia.org/wiki/Phi)（/faɪ/，/fiː/）取自希腊字母 φ。在数学中，它常用来表示黄金比例。

Phi 是一个用 Rust 编写、基于 [Rig](https://github.com/0xPlaygrounds/rig) 构建的轻量编程 CLI。它的 Agent 运行框架可以独立复用，通过公开的 Agent API 执行任务、委派工作和编排流程。你可以直接在本地终端使用，也可以让 Agent 在 Gateway 后台运行，通过 CLI、Web、Telegram 或飞书接入。

Phi 借鉴了 [Claude Code](https://github.com/anthropics/claude-code) 和 [pi](https://github.com/badlogic/pi-mono) 的许多设计，非常感谢！

Phi 最初的想法很简单：每个人都应该有一套属于自己的 Agent 框架和起步工具。它不打算像 Claude Code/Pi 那样全能，而是提供一个可以自由扩展的基础，专注于自己的个性化需求。

Phi 的 70% 代码由 AI 生成，它是我学习和构建 agent 的一个实验，它仍然有一些细小问题或 bug。经过大半年的日常使用和自迭代, it just works！(Linux only)

## 核心能力

- **开箱即用的工具。** 内置 Bash、Read、Write、Edit、Grep 和 Glob，覆盖命令执行、文件编辑与搜索。支持跟踪前台和后台任务，通过权限规则和人工审批（Human-in-the-loop）控制工具调用，并由沙箱规则限制文件系统访问。
- **内置 Agent 与可复用的运行框架。** 将任务委派给拥有独立历史记录的子 Agent，或通过 Teams 的异步消息和共享任务列表协作。Agent Harness 提供公开的 Agent API，可在自己的应用中执行任务、委派工作和编排流程。
- **记忆与导航。** 通过 Rewind 回退到之前的节点，在对话树中切换和探索不同分支。按需启用 Memory，跨会话积累知识；用 Tape 锚点标记工作节点，便于搜索和恢复上下文。
- **按任务切换工作方式。** 将服务商、模型、思考级别和提示词保存为 Switch 预设，随时切换。内置 `default` 和 `plan` 模式分别用于实现和规划；Plan 模式会阻止直接调用 Write/Edit 工具，但并不禁止所有可能的写入方式。
- **扩展工作流。** 加载项目指令、技能、提示词模板和自定义 Agent 定义，接入 MCP 工具、Lua 扩展及生命周期钩子。也可以启用 Codemode，用 JavaScript 编排工具调用。
- **选择接入方式。** 使用[交互式 CLI 或单次输出模式](phi-interactive/README.md)，通过 [`phi --rpc`](phi-interactive/README.md#machine-control) 控制前台子进程，或用 [Gateway](phi-remote/README.md) 托管会话，让任务在客户端断开后继续运行。远程插件需要手动启用。

## 工作区结构

Phi 将可复用的执行库、UI 库与应用工作流、远程通信分开。

| Crate | 职责 |
|---|---|
| [`phi-sdk`](phi-sdk/) | Agent 执行、任务委派、对话记录、工具和生命周期钩子。 |
| [`phi-tui`](phi-tui/) | 可复用的终端 UI 运行时与组件。 |
| [`phi-interactive`](phi-interactive/) | `phi` CLI、运行环境准备、会话持久化和 stdio RPC。 |
| [`phi-remote`](phi-remote/) | Gateway、设备授权，以及 Web、Telegram 和飞书插件。 |
| [`phi-codemode`](phi-codemode/) | 基于 QuickJS 和 Wasmtime 的隔离 JavaScript 执行环境，以及宿主工具桥接。 |

## 快速开始

构建 Phi 需要 Rust 1.97 或更新版本（Wasmtime 50 的要求）。

可选：使用 Web 界面时，需通过 Node.js 22 或更新版本及 pnpm [单独构建 Web 资源](phi-remote/README.md#build-web-assets)，Cargo 不会自动构建。CLI 和 Gateway 的构建与运行均不需要 Node.js。

在仓库根目录执行以下命令，将 Phi 安装到 Cargo 的可执行文件目录（通常是 `~/.cargo/bin`，请确保已加入 `PATH`）：

```bash
cargo install --path phi-interactive --locked
```

Linux 默认沙箱需要安装 `bubblewrap`（`bwrap`）。如果无法使用，请显式将 `sandbox.enabled` 设为 `false`。发送提示词前，先配置服务商和模型。创建 `~/.phi/settings.json`：

```json
{
  "defaultProvider": "anthropic",
  "defaultModel": "your-model-id"
}
```

将 `your-model-id` 替换为服务商支持、且已收录在 Phi 模型目录中的模型 ID。如果使用 API Key 认证，请在启动 Phi 前设置对应的环境变量：

```bash
export ANTHROPIC_API_KEY="your-api-key" # 对应 defaultProvider: anthropic
phi
```

其他服务商使用 `OPENAI_API_KEY`、`GEMINI_API_KEY`、`DEEPSEEK_API_KEY`、`OLLAMA_API_KEY` 或 `OPENROUTER_API_KEY`。建议使用环境变量，而非 `--api-key`，避免凭据暴露在进程参数中。

如果使用 OpenAI Codex 订阅，在本地执行 `/login codex`。授权有效期为 30 天，到期后需要重新登录；刷新 token 不会延长有效期。RPC 不支持发起登录。

Phi 首次运行时会初始化模型目录。设置 `PHI_OFFLINE=1` 可关闭在线发现和更新。

## 常用 CLI 命令

| 命令 | 用途 |
|---|---|
| `phi` / `phi "prompt"` | 在当前目录开启交互式会话，也可以附带初始提示词。 |
| `phi -p "prompt"` | 输出一次回复后退出。 |
| `phi -c` / `phi -r` | 继续最近的会话，或选择一个会话恢复。 |
| `phi --rpc` | 通过标准输入/输出上的 JSONL 协议进行程序控制。 |
| `phi list` | 列出当前工作目录下最近保存的会话。 |

包含空格的提示词需要加引号，让 shell 将其作为一个参数传入，例如 `phi "Explain this repository"`。单个词可以不加引号；但如果它与子命令同名，应使用 `phi -- "list"`。仅加引号并不能将它与子命令区分开。其他选项（包括会话参数）见 `phi --help` 或 `phi <command> --help`。程序控制的用法见[应用指南](phi-interactive/README.md)。

## 配置

Phi 先加载全局配置 `~/.phi/settings.json`，再加载当前目录的 `.phi/settings.json`。项目配置按**顶层字段**覆盖全局配置，只有 `agent` 例外：其中的 `background` 和 `team` 分别合并，其他嵌套对象均整体替换，不做深度合并。`--provider`、`--model` 等 CLI 参数在适用时优先于配置文件。可通过 `PHI_CONFIG_DIR` 更改全局配置目录。配置文件必须使用严格 JSON。

### 基本示例

```json
{
  "defaultProvider": "anthropic",
  "defaultModel": "your-model-id",
  "defaultThinkingLevel": "medium",
  "sandbox": {
    "enabled": true,
    "whitelist": ["~/.phi"],
    "blacklist": []
  },
  "permission": {"bash": "ask"}
}
```

修改配置后，可在正在运行的独立会话中执行 `/reload` 重新加载。

### Switch：可复用的工作预设

Switch 将服务商、模型、思考级别、模式及可选的提示词覆盖内容组合成一个具名预设。无需逐项修改配置，为当前任务选一个预设，完成后再切回来即可。在 `~/.phi/settings.json` 或项目的 `.phi/settings.json` 中添加 `switches`：

```json
{
  "switches": [
    {
      "name": "fast",
      "alias": "f",
      "provider": "anthropic",
      "model": "your-fast-model-id",
      "thinking_level": "low",
      "mode": "default"
    },
    {
      "name": "review",
      "alias": "r",
      "mode": "plan",
      "prompt": "先审查拟议的改动，再进行编辑。"
    }
  ]
}
```

将示例模型 ID 替换为模型目录中已有的 ID。使用 `/switch` 选择或编辑预设，使用 `/switch NAME` 或别名直接切换。在独立交互式会话中，也可以用 Ctrl+↑/↓ 循环切换。在选择器中编辑的预设会保存到全局配置；当前选择则随会话保留。

未填写的字段沿用配置值或当前生效值。使用 `default` 或 `plan` 作为名称，可以覆盖同名内置预设。即使自定义了提示词，Plan 模式仍会阻止直接调用 Write/Edit。

空闲时，可用 Ctrl+←/→ 调整模型的思考级别，不会修改配置或当前 Switch。

### Agent 任务委派

在 `settings.json` 的顶层 `agent` 字段中配置委派行为：

```json
{
  "agent": {
    "background": true,
    "team": false
  }
}
```

普通子 Agent 可以在前台运行（Agent 工具等待结果），也可以在后台运行（先返回启动回执，稍后交付结果）。未指定模式时，主 Agent 发起的委派默认在后台运行，受委派的 Agent 发起的委派默认在前台运行。团队成员始终异步创建，通过消息协作。

`background: true` 会强制后台运行，即使调用方请求前台执行；设为 `false` 或省略时，遵循默认行为和显式选择。`team: true` 会在交互式会话中启用 Teams，但不会自动创建团队成员。两项均默认为 `false`。

全局与项目配置中的 `agent` 子字段分别合并。`/reload` 只影响该 Agent 之后发起的委派，不改变已接纳的任务或现有子 Agent。

### Memory 与 Tape

通过 `"memory": {"enabled": true}` 启用 Memory，并按需配置 `memoryDir.localPath` 和 `tape`。自动注入上下文需要设置 `"injection": "message-append"`；省略或设为 `false` 时不自动注入。上下文会加入第一个符合条件且被接纳的用户提示词，而非系统提示词。

- `/memory-inject [prompt]` 按需注入上下文，即使自动注入已关闭或已使用过，也可以执行。启用 Tape 时，提示词会用于筛选上下文，并与上下文一起提交。
- 不带参数的 `/memory-inject` 会直接构建上下文，不使用提示词查询，也不会凭空补出一个任务。

手动注入还会附带一条提醒，提示 Agent 使用 `memory_search` 和 `tape_read` 继续查找。注入使用当前的 Memory/Tape 数据，不复用缓存内容。RPC 提交使用进程 Host 的 Memory 配置和会话目录。配置与筛选细节见 [Memory/Tape 设计](phi-sdk/src/extension/memory/docs/tape-design.md)。

### Hooks

在全局 `~/.phi/settings.json` 中注册生命周期命令钩子。支持 `SessionStart`、`UserPromptSubmit`、`UserPromptExpansion`、`PreToolUse`、`PostToolUse`、`Stop` 和 `Notification` 事件。

```json
{
  "hooks": {
    "enabled": true,
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "bash \"$HOME/.phi/hooks/check-command.sh\"",
            "timeout": 2
          }
        ]
      }
    ]
  }
}
```

使用前需创建 `~/.phi/hooks/check-command.sh`。脚本通过标准输入接收事件 JSON；`matcher` 精确匹配工具名，`timeout` 的单位为秒。命令以当前进程权限在工具沙箱外执行，不经过 Agent 的人工审批流程。

- `hooks.enabled` 默认为 `true`。设为 `false` 会关闭配置中的命令钩子和 Lua 钩子，不影响内置/Rust 钩子及普通扩展命令和工具。修改后执行 `/reload` 生效。
- 交互式启动时会询问是否信任或禁用当前工作区的钩子。全局 `hooks.trust` 保存精确的绝对路径：`"+/path/to/workspace"` 表示信任，`"-/path/to/workspace"` 表示禁用。存在进程钩子时，RPC 和 Web 启动要求预先保存选择。
- 项目配置不允许注册非空的钩子。顶层 `hooks` 用于注册生命周期处理器，`memory.hooks` 则仅配置 Memory 的 Git 同步。

事件字段、脚本返回格式及信任策略详见[命令钩子](phi-interactive/README.md#command-hooks)。

### 完整配置示例

```jsonc
{
  "defaultProvider": "anthropic",
  "defaultModel": "your-model-id",
  "defaultThinkingLevel": "medium", // 使用模型支持的级别。

  // 可选，仅适用于 OpenAI；其他服务商请省略。
  "openaiReasoning": {
    "effort": "medium",
    "mode": null, // 仅支持的模型可设为 "pro"。
    "context": "auto",
    "summary": "auto"
  },

  // null 表示采用服务商和模型目录提供的值。
  "max_tokens": null,
  "temperature": null,
  "context_window": null,
  "reserve_tokens": 16384,
  "keep_recent_tokens": 20000,
  "compact_enabled": true,
  "no_tools": false,
  "no_context_files": false,

  "switches": [
    {
      "name": "review",
      "alias": "r",
      "provider": "anthropic",
      "model": "your-model-id",
      "thinking_level": "medium", // 预设中的此字段使用 snake_case。
      "mode": "plan", // 阻止直接调用 Write/Edit，而非所有可能的写入。
      "prompt": "先审查改动，再提出编辑建议。"
    }
  ],

  "agent": {
    "background": false, // true 会强制后续委派在后台运行。
    "team": false // 仅在交互式会话中启用 Teams。
  },

  // 具体规则优先于 mode；Deny > Ask > Allow。
  // 会话授权只处理 Ask。Yolo 绕过所有权限规则，
  // 但不绕过沙箱、能力检查或配置校验。
  "permission": {
    "mode": "ask",
    "nonInteractiveAsk": "deny",
    "read": "allow",
    "grep": "allow",
    "glob": "allow", // 仅限内置 Glob；shell 搜索遵循 bash 规则。
    "write": "ask",
    "edit": "ask",
    "bash": {
      "*": "ask",
      "git status*": "allow",
      "git push*": "deny"
    },
    // 使用实际注册的工具名；这些规则本身不会加载工具。
    "tools": {
      "deploy_preview": "ask",
      "delete_database": "deny"
    },
    // 用于能识别路径的工具，使用绝对路径模式；不是 Bash 防火墙。
    // allow 不授予沙箱写入权限；未匹配路径遵循工具或 mode 规则。
    "external_directory": {
      "/home/me/shared/**": "allow",
      "/home/me/private/**": "deny"
    }
  },

  "sandbox": {
    "enabled": true,
    "whitelist": [], // 额外可写路径；工作目录默认可写。
    "blacklist": [] // 隐藏路径，优先于 whitelist。
  },

  // 仅支持全局配置，修改后需重启。true 放宽可选的 RPC 文件保护，
  // 不影响沙箱或权限。配置和认证文件仍受保护；RPC 不支持登录。
  "trust_remote": false,

  "hooks": {
    "enabled": true,
    "trust": [], // 全局工作区信任选择，见上方 Hooks。
    "PreToolUse": [] // 命令仅支持在全局配置中注册。
  },

  "mcp": true, // 服务器定义放在 mcp.json 中。
  "codemode": {
    "enabled": false,
    "inlineBudget": 3000,
    "cache": true // 使用默认缓存目录；false 关闭缓存，路径字符串指定新位置。
  },

  "memory": {
    "enabled": false, // 使用 Memory 或 Tape 前需手动启用。
    "memoryDir": {
      "localPath": "~/.phi/memory", // 本地 Memory 仓库。
      "repoUrl": null, // 可选，用于同步的 Git 远程仓库地址。
      "globalMemory": "global" // 仓库内的共享作用域目录。
    },
    "injection": false, // "message-append" 启用自动上下文注入。
    "skill": true, // 包含内置 Memory 技能指引。
    // Memory 的 Git 同步操作，不是通用命令钩子。
    "hooks": {
      "sessionStart": ["pull"],
      "sessionEnd": [], // 添加 "push" 可在会话结束时同步。
      "userPromptSubmit": []
    },
    "tape": {
      "enabled": false, // 同时要求启用 Memory。
      "onlyGit": true, // 在 Git 仓库以外跳过 Tape。
      "excludeDirs": [], // 禁用 Tape 的目录，使用绝对路径。
      "context": {
        "strategy": "smart", // "smart" 或 "recent-only"。
        "fileLimit": 10,
        "memoryScan": [72, 168], // 活动扫描时间窗：初始/最大小时数。
        "alwaysInclude": [], // 无论排序如何都包含的 Memory 路径。
        "whitelist": [], // 始终包含匹配的文件或目录。
        "blacklist": [] // 排除匹配的文件或目录。
      },
      "anchor": {
        "labelPrefix": "anchor: ", // 锚点标签的显示前缀。
        "mode": "auto", // "manual" 会让修改操作经过权限检查。
        "keywords": {
          "global": ["checkpoint"],
          "project": [] // 关键词用于建议锚点，不会强制创建。
        }
      }
    }
  },

  // 空数组保留自动发现。条目可添加路径或 glob 模式；
  // !pattern 用于排除；精确的 +path/-path 覆盖规则优先。
  // 相对路径以全局 ~/.phi 或项目 .phi 为基准。
  "extensions": [],
  "skills": [],
  "prompts": []
}
```

对于 `codemode`、`memory`、`memory.tape` 和 `sandbox`：空对象 `{}` 表示禁用，填写选项则默认启用，显式的 `enabled` 优先。省略整个配置块时保留默认行为；CLI 覆盖和扩展排除规则仍然生效。

在 RPC 中，如果配置文件尚不存在，其父目录会保持只读，直到你在本地创建文件并重新准备运行环境。权限规则和基础沙箱都无法完全隔离受信任代码或 Git 钩子。

**资源的工具策略。** 提示词中的 `allowed-tools` 为当前轮次预先批准工具调用，`disallowed-tools` 则移除整个工具。Agent 定义中的工具字段只负责选择或移除工具，不授予调用权限。技能只提供上下文；MCP 单独配置。参见[示例与生效范围](phi-interactive/README.md#prompt-and-agent-tool-settings)。

**资源选择。** 使用 `phi config` 搜索、启停技能、提示词和 Lua 扩展。Tab 切换全局/项目作用域，`--local` 直接进入项目作用域。空格键切换全局资源的启用状态，或循环切换项目资源的继承/加载/卸载状态。修改会保存到对应配置文件，重启或执行 `/reload` 后生效。

资源数组支持路径/glob 模式、`!pattern` 排除规则和精确的 `+path`/`-path` 覆盖规则，后者优先级更高。相对路径以 `~/.phi` 或项目 `.phi` 为基准；项目中自动发现的提示词仍位于 `./prompts`。

## `~/.phi/` 目录

Phi 按需创建文件。用户安装的资源目录和可选功能目录可能尚不存在。示例：

```text
~/.phi/                          # 全局配置目录，可通过 PHI_CONFIG_DIR 修改
├── settings.json                # 全局配置
├── mcp.json                     # MCP 服务器配置
├── mcp-cache.json               # MCP 公开元数据缓存
├── models-store.json            # 自动发现的模型目录缓存
├── auth/                        # 服务商登录凭据，请妥善保管
├── agents/                      # 自定义 Agent 定义
├── prompts/                     # 自定义提示词模板
├── skills/                      # 已安装的技能
├── extensions/                  # Lua 扩展
├── memory/                      # 可选的 Memory 数据，位置可配置
├── projects/                    # 按工作目录保存的会话，可通过 PHI_SESSION_DIR 修改
│   └── --path-to-project--/     # 编码后的工作目录路径
│       ├── <timestamp>-<id>.jsonl  # 保存的会话记录
│       └── <timestamp>-<id>.lock   # 活跃会话锁，正常停止时移除
```

OAuth 操作会临时创建 `auth/<provider>.json.lock` 目录，并在完成、失败或取消时删除。

会话文件锁是由操作系统锁机制支持的临时归属标记。正常释放、出错或取消时，会删除持有的锁文件；进程异常终止会释放操作系统锁，但可能留下文件。下一个持有者会自动复用该文件，RPC 启动时也会清理当前工作目录范围内未被持有的会话锁。获取锁时会核对已锁定文件与当前路径是否仍指向同一文件，避免持有旧 inode 的进程在清理或替换发生后误认为自己取得了所有权。

## 文档

- 使用：[应用命令](phi-interactive/README.md) · [TUI 库](phi-tui/README.md) · [MCP 配置](phi-sdk/src/extension/mcp/README.md)
- 架构：[总览](docs/design.md) · [SDK](docs/design-sdk.md) · [交互式应用](docs/design-interactive.md)
- Agent：[AgentInstance、AgentManager 与应用运行时](docs/agent/design-agent-runtime.md) · [并行 Agent](docs/agent/design-parallel-agent.md) · [Git worktree](docs/agent/design-worktree.md)
- TUI：[设计](docs/tui/design-tui.md) · [性能](docs/tui/design-tui-performance.md)
- Memory 与 Tape：[Tape](phi-sdk/src/extension/memory/docs/tape-design.md) · [统一记忆图谱](phi-sdk/src/extension/memory/docs/unified-memory-graph-design.md)
