# TokenScope 产品需求文档（PRD）

## 1. 产品概述

### 1.1 产品名称
TokenScope —— macOS 菜单栏 Claude CLI 用量仪表盘

### 1.2 一句话定位
一个常驻 macOS 菜单栏的小工具，实时展示 Claude CLI 的 Token 用量、调用统计和使用花费，让用户对自己的 AI 编码消耗"心中有数"。

### 1.3 目标用户
- 频繁使用 Claude CLI / Claude Code 的开发者
- 关心 Token 消耗、订阅性价比、工具使用习惯的个人用户
- 希望了解自己 AI 工作流中哪些 MCP / Skill 真正在被使用的人

### 1.4 解决的问题
- Claude CLI 自带的 `/cost` 只能看当前会话，无法纵向看每日/每周/每月趋势
- 不知道自己装的 MCP、Skill 哪些在用、哪些是"装了不用"占 context
- 没有按项目、按模型维度的消耗洞察
- 缺少常驻、随时可见的用量提醒入口

---

## 2. 核心功能

### 2.1 菜单栏常驻入口
- macOS 菜单栏右上角图标 + 当日 Token 消耗数（如「今日 1.2M tokens」，自动按 K / M 缩写）
- 仅展示 Token 数，不在菜单栏直接显示花费金额
- 点击展开浮窗 / 打开详细仪表盘
- **浮窗与普通窗口并存**（托盘右键菜单 → *Open in Window* 勾选项）：
  - **popover（始终可用）**：左键点击菜单栏图标始终开关浮窗；macOS 为 NSPanel 浮窗（可浮在全屏应用之上），点击外部/切换应用自动隐藏；Windows 为无边框浮窗，锚定在托盘图标所在屏幕右上角
  - **detached window**：勾选后额外打开一个普通应用窗口 —— 带标题栏、可缩放、不置顶；关闭窗口 = 隐藏（应用继续留在菜单栏/托盘）；打开期间 macOS 出现在 Dock、Cmd-Tab 与应用菜单（Dock 点击可唤回窗口），Windows 出现在任务栏；位置与大小持久化到数据目录，下次启动还原
  - 两者互不影响，勾选/取消即时生效（无需重启）；是否打开持久化于 `window-mode.json`（`window` = 打开），几何存于 `window-geom.json`
- 后台轮询日志，准实时更新

### 2.2 用量仪表盘（核心）

#### 时间维度
- 今日 / 本周 / 本月（自然日、自然周、自然月）
- **5 小时滚动窗口**：计划额度（session 窗口）按滚动 5 小时计，跨零点不清零
- 时段趋势图（5H 按 20 分钟 / Day 按小时 / Week 按天 / Month 按天）

#### 核心指标
| 指标 | 说明 |
|------|------|
| 会话数 | 按 sessionId 去重计数 |
| 消息数 | assistant message 数（按 message.id 去重） |
| Token 用量 | input / output 总量 |
| 估算花费 | 基于 models.dev + LiteLLM 公开价格表估算（USD，UI 始终带「est.」标识），订阅用户应理解为「等效消费价值」而非真实账单；缓存写入按 5 分钟 / 1 小时两档 TTL 分别计价（Anthropic 为 1.25× / 2× input）；未在价格表内的模型仅显示 Token 数，不计入花费 |

#### 多维度切片
仪表盘核心四个切片维度：

- **按 agent** 分布（Claude Code / Codex / opencode / Oh My Pi / pi）—— 看 token & 请求量落在哪个工具上
- **按模型** 分布（Opus / Sonnet / Haiku，以及第三方模型如 GLM、DeepSeek）—— 看 token & 花费在不同模型上的分布
- **按 MCP 调用** 分布 —— 用户安装的 MCP 各自被调用了多少次
- **按 Skill 调用** 分布 —— 用户安装的 Skill 各自被调用了多少次

#### 计划额度（Plan limits，服务商侧）
- 展示 5 小时滚动 / 每周 / 每月窗口的**已用百分比**与**重置倒计时**（opencode Go/Zen、Claude 等）
- 数据来自服务商接口（见 §3.5），非日志推算；请求失败时显示上一次已知值，并在 hover 中标注来源与观测时间
- 与时间维度无关（当前状态），因此在 5H/Day/Week/Month 任一视图下都显示

### 2.3 工具调用统计（**只展示用户自定义安装的**）

> **明确策略：仅展示用户自己安装的 MCP 和 Skill，Claude 内置工具（Bash/Read/Edit 等）和 Anthropic 自带 MCP（Claude_Preview 等）一律过滤。**

#### 用户 MCP 调用
- 数据源：各 agent 自己的 MCP 配置（见 §3.3）
- 展示：按 server 聚合调用次数排行（不下钻到具体工具）
- 价值：发现高频 MCP、识别"装了没用"的 MCP

#### 用户 Skill 调用
- 数据源：各 agent 的 skill 目录（见 §3.3）
- 展示：按 skill 名称排行
- 提取方式：Claude Code 取 `tool_use.name == "Skill"` 的 `input.skill`，以及斜杠命令的 `<command-name>`；Oh My Pi / pi 取读取 skill 本身的 tool call（`arguments.path == "skill://<id>"`）

---

## 3. 数据来源与采集

### 3.1 主数据源
**各 coding agent 的会话日志（全部只读）**

| Agent | 路径 | 格式 |
|-------|------|------|
| Claude Code | `~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl` | JSONL，每行一个事件 |
| Codex CLI | `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl`（默认 `~/.codex/...`） | JSONL，`turn_context` 给模型、`event_msg/token_count` 给用量 |
| opencode（新） | `~/.local/share/opencode/opencode.db` → 表 `message` 的 `data` 列 | SQLite + JSON 文档 |
| opencode（旧） | `~/.local/share/opencode/storage/message/<sessionID>/<msgID>.json` | 每消息一个 JSON 文件 |
| Oh My Pi | `~/.omp/agent/sessions/**/*.jsonl`、`~/.omp/profiles/<profile>/agent/sessions/**/*.jsonl` | JSONL，`type=message` 的 assistant 消息带 `usage` |
| pi | `$PI_SESSIONS_DIR`（默认 `~/.pi/agent/sessions/<cwd>/*.jsonl`） | JSONL，`type=message` 的 assistant 消息带 `usage{input, output, cacheRead, cacheWrite, reasoning}`，session 取自文件名中的 uuid |

- **Claude Code** 关键事件类型：
  - `user` —— 用户消息（含 timestamp / cwd / gitBranch / version）
  - `assistant` —— 模型响应（含 model / usage / content）
  - `attachment` —— 附加内容（如 skill_listing）
- 未安装的 agent：目录不存在即跳过，不报错
- 同一会话可能分散在多个文件（Oh My Pi 的 `__advisor.*.jsonl`、subagent 日志）→ 归入同一 session
- 五个 token 口径差异：Codex 的 `input_tokens` **包含** cached（未缓存部分取差值）、无 cache-write；opencode 的 `reasoning` 计入 output；Oh My Pi 与 Claude Code 一致；pi 与 opencode 同口径（`reasoning` 计入 output，`cacheWrite` 按 5 分钟缓存计）

### 3.2 Assistant 消息核心字段
```json
{
  "type": "assistant",
  "message": {
    "model": "claude-opus-4-7",
    "id": "msg_xxx",
    "usage": {
      "input_tokens": 10,
      "output_tokens": 817,
      "cache_creation_input_tokens": 42582,
      "cache_read_input_tokens": 0
    },
    "content": [
      { "type": "thinking", "thinking": "..." },
      { "type": "text", "text": "..." },
      { "type": "tool_use", "name": "Bash", "input": {...} }
    ]
  },
  "timestamp": "2026-06-03T15:23:03.366Z",
  "sessionId": "...",
  "cwd": "/Users/.../project",
  "gitBranch": "main",
  "version": "2.1.160"
}
```

### 3.3 配置数据源（用于"用户自定义"过滤）
- MCP：`~/.claude.json` 的 `mcpServers` 与 `projects[*].mcpServers`；各 agent 自己的 `mcp.json`（`~/.omp/agent`、`~/.omp/profiles/*/agent`、pi 的 agent 目录）；Codex 的 `~/.codex/config.toml` 中 `[mcp_servers.*]` 表
- Skill：`~/.claude/skills/`、各 agent 目录的 `skills/`、Oh My Pi 插件 skills（`~/.omp/plugins/*/skills/`）—— 扫描目录得到用户安装的 Skill 名单
- 跨 agent 取**并集**：同一台机器上任一 agent 安装的 MCP / Skill 都算「用户自己的」——只在 Oh My Pi 里装过的 server，被 Claude Code 调用时同样计入
- 匹配做归一化：server 名忽略大小写与 `-`/`_` 差异并按最长前缀匹配（Oh My Pi 会把 server 与工具名压平成一个名字）；skill id 允许 `plugin:skill` → `plugin-skill` 形式对上目录名

### 3.4 数据采集策略
- 监听上述所有数据源目录的文件变化（fs.watch / FSEvents），写入后 ~1s 内刷新；30s 轮询兜底
- 增量解析新增行（按文件 size/mtime/offset manifest），避免全量重读；SQLite 源按 `time_updated` 水位线增量读取
- 按 message id 去重：Claude Code 的同一消息可能跨多行，且流式生成时会反复重写整条行、`output_tokens` 逐次变大（合并其 tool_use，token 读数取最新一条带 usage 的行）；Codex / opencode / Oh My Pi 会在流式过程中改写同一条消息（以最新一次为准）
- 本地持久化（Caches 目录下的本地 JSON），加速重启与历史查询；仅保留最近 ~26 周
- **落盘节奏与刷新解耦**：内存中保留一份解析结果，刷新只做增量 ingest；缓存**每 15 分钟**落盘一次，外加进程内首次变更与退出时各一次 —— 而不是每次刷新都重写整份缓存。刷新由日志写入驱动（可能每秒 1~2 次），每次刷新重写 4 MB 缓存会把写入量放大到每天数 GB，触发 macOS 的 per-process disk-writes 限额后刷新线程卡在 `write()` 中，面板彻底停止更新。落盘时同时清理已删除日志文件的 manifest 条目，避免 manifest 无限增长

### 3.5 计划额度（服务商侧）
- **opencode Go/Zen**：`GET https://opencode.ai/zen/go/v1/usage`，`Authorization: Bearer <api_key>`
  - 返回 `{ usage: { rolling|weekly|monthly: { percent, status, resetsAt } } }`
  - API key 来源：环境变量 `OPENCODE_GO_API_KEY`/`OPENCODE_API_KEY` → `~/.omp/**/agent.db`（`auth_credentials`，provider=`opencode-go`）→ `~/.local/share/opencode/auth.json`
- **Claude Code**：`GET https://api.anthropic.com/api/oauth/usage`，`Authorization: Bearer <oauth access token>` + `anthropic-beta: oauth-2025-04-20`
  - 返回 `five_hour` / `seven_day` / `seven_day_opus` / `seven_day_sonnet`（`utilization` 为 0–100 百分比 + `resets_at`），以及 `limits[]`（`kind`/`group`/`percent`/`severity`/`resets_at`/`scope.model.display_name`）——「7 Day (Fable)」这类带模型名的窗口就来自 `limits[]`
  - token 来源（依次试用，接口接受的第一个生效）：环境变量 `CLAUDE_CODE_OAUTH_TOKEN` → macOS 钥匙串 `Claude Code-credentials*`（服务名带账号哈希后缀，用 `dump-keychain` 发现，经 `/usr/bin/security` 读取——授权归属于该系统二进制而非本应用，重新构建不会重复弹框）→ `~/.claude/.credentials.json`（`claudeAiOauth.accessToken`）→ `~/.omp/**/agent.db` 的 `auth_credentials`（provider=`anthropic`, type=`oauth`）
  - token 过期/失效 → 请求失败 → 依次试下一个来源；全部失败才回落到 Oh My Pi 的快照值，并把该窗口标记为 stale
- **其他服务商**：读 Oh My Pi 轮询结果 `~/.omp/**/agent.db` → `usage_history`（按 provider + limit 取最新一条）
- 合并规则：同一窗口（provider + window + label）以观测时间最新者为准；请求失败则保留上一次已知数值，但 UI 标记为 `stale · read HH:MM`（重置时刻已过则显示 `window reset`），不再当作当前值
- 刷新：后台每 15 分钟一次，本地缓存 `limits.json`，重启后立即有值
- 安全：key/token 只用于请求头，**不落盘、不打日志、不下发到前端**

---

## 4. 分类与过滤规则

### 4.1 工具调用分类逻辑
```
tool_use.name 判定（各 agent 的工具调用都会被分类）：
  1. 在内置工具黑名单中 → 过滤，不展示
  2. MCP 调用（命名约定按 agent 不同）：
     - Claude Code / Codex：mcp__<server>__<tool>
     - Oh My Pi / pi：xd_mcp__<server>_<tool>（server 与 tool 被压平，靠归一化最长前缀对上配置）
     - server 在用户 MCP 配置中（§3.3，跨 agent 并集）→ 展示为「用户 MCP」
     - 否则 → 过滤（各 agent 自带 / 托管的 MCP，如 Claude Code 的 claude-in-chrome）
  3. Skill 调用：
     - Claude Code：tool_use.name == "Skill" 的 input.skill，或斜杠命令 <command-name>/<skill>
     - Oh My Pi / pi：读取 skill 本身的 tool call（arguments.path == "skill://<id>"；读 skill 内的引用文件不计）
     - id 在用户 skill 名单中（§3.3，允许 plugin:skill → plugin-skill）→ 展示为「用户 Skill」
     - 否则 → 过滤（bundled skill）
  4. 其他 → 过滤
```
> 每次改动提取逻辑都要提升 `STORE_VERSION`（`src-tauri/src/store.rs`），否则增量 manifest 会跳过已读字节，旧缓存继续按老规则计数。

### 4.2 内置工具黑名单（硬编码）
```
Bash, Read, Edit, Write, Glob, Grep, Agent,
TaskCreate, TaskUpdate, TaskList, TaskGet, TaskStop, TaskOutput,
TodoWrite, NotebookEdit, WebFetch, WebSearch,
ExitPlanMode, EnterPlanMode, Skill, ToolSearch, AskUserQuestion,
EnterWorktree, ExitWorktree, ScheduleWakeup,
CronCreate, CronDelete, CronList
```

### 4.3 花费计算

#### 价格数据源
- **主源**：models.dev（`https://models.dev/api.json`）—— 使用 bare 模型名，与 CLI 日志一致
- **回退**：LiteLLM 官方维护的开源价格表
  - URL：`https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json`
  - 额外提供 1 小时缓存写入价：`cache_creation_input_token_cost_above_1hr`
- **兜底**：打包内置的 LiteLLM 快照 → 内置旗舰模型表（首启 + 无网络 + 无缓存时仍能计价）
- 覆盖：Anthropic / OpenAI / Google / opencode Go·Zen / 主流第三方模型（含 GLM、DeepSeek、Kimi、MiniMax 等）
- 字段：`input` / `output` / `cache_write`（5 分钟缓存写入）/ `cache_read`
- **不提供**用户自定义价格表配置入口，避免维护成本与配置错误

#### 价格表分发策略
- 应用打包时内置一份 LiteLLM 价格表快照（保证离线可用）；刷新快照时**保留**上游已移除的旧模型条目，否则离线查看历史用量会失去价格
- 启动时尝试拉取最新版本（失败则使用内置快照）
- 每 24 小时后台自动刷新一次（ETag 条件请求，未变更时零下载）；托盘菜单的 Refresh 可手动强制刷新（30s 冷却）

#### 计算公式
```text
cost = input_tokens          × price.input
     + output_tokens         × price.output
     + cache_write_5m_tokens × price.cache_write       # 5 分钟缓存写入
     + cache_write_1h_tokens × price.cache_write_1h    # 1 小时缓存写入
     + cache_read_tokens     × price.cache_read        # 命中 / 刷新
```

**缓存写入必须按 TTL 分开计价**：Anthropic 的倍率是 5 分钟写入 1.25× input、**1 小时写入 2× input**、命中 0.1× input。Claude Code 用 `cache_creation.ephemeral_5m_input_tokens` / `ephemeral_1h_input_tokens` 上报两档，Oh My Pi 用 `cttl.ephemeral1h`；Claude Code 的缓存默认 1 小时 TTL，因此绝大多数缓存写入属于 1 小时档，若一律按 5 分钟价计会显著低估花费。models.dev 只公布 5 分钟价，故 Claude 模型的 1 小时价按 2× input 推导（LiteLLM 若给出 `..._above_1hr` 则以上游数值为准）；非 Anthropic 模型没有 1 小时缓存，沿用单一 cache_write 价。

#### 模型匹配规则
- 精确 id → 归一化 id（去 vendor 前缀、`.`↔`p`，如 `glm-5.1`⇄`glm-5p1`）→ 去 provider 命名空间（`anthropic.claude-opus-5` → `claude-opus-5`）；同一模型被多家转售时，一方官方（first-party）价格优先，其次取带缓存价、非命名空间的条目
- 匹配不到的模型：UI 标记为「未知模型」，不计入花费，但 token 数仍统计
- 不做模糊匹配 / 别名兜底，避免错算

#### 估算性质说明
- 明确告知用户：花费为「按官方公开价格估算」
- Pro/Max 订阅用户实际为包月固定支出，仪表盘展示的金额可理解为「等效消费价值」
- UI 上始终带 "估算 (est.)" 字样，避免误读为账单

---

## 5. 数据精度说明

| 指标 | 精度 |
|------|------|
| 会话数 | ✅ 精确（sessionId 去重） |
| 消息数 | ✅ 精确（message.id 去重） |
| Token 用量 | ✅ 精确（来自 API 返回的 usage） |
| 用户 MCP 调用次数 | ✅ 精确（命名约定 + 配置白名单） |
| 用户 Skill 调用次数 | ✅ 精确（input.skill + 目录白名单） |
| 模型分布 | ✅ 精确 |
| **花费（USD）** | ✅ 精确（精确 Token 数 × LiteLLM 官方价格；订阅用户为「等效消费价值」） |

---

## 6. 技术方案

### 6.1 技术栈决策：Tauri + React（已定稿）

**最终选型：Tauri + React 前端**

#### 候选对比
| 方案 | 安装包 | 常驻内存 | 跨平台 | 复用现有 HTML | 结论 |
|------|--------|---------|--------|--------------|------|
| **Tauri** ✅ | ~3–10MB | ~30–80MB | ✅ | ✅ 直接用 | **选中** |
| Swift + SwiftUI | ~5–15MB | ~20–50MB | ❌ 仅 macOS | ❌ 需重写 | 排除（不跨平台） |
| Electron | ~80–150MB | ~100–250MB | ✅ | ✅ 直接用 | 排除（太重） |

#### 决策理由（对齐需求约束）
1. **未来可能跨平台** → 排除仅支持 macOS 的 Swift
2. **在意常驻内存，越轻越好** → 排除 Electron（每个 app 打包整个 Chromium，100MB+）；Tauri 复用系统 WebView（macOS 为 WKWebView），体积与内存接近原生
3. **UI 需要滚动 / hover / 点击 / 图表等丰富交互** → Tauri 的 UI 层即系统 WebView，React 生态的图表库（Recharts / ECharts / Chart.js）可直接使用，现有 `dashboard.html` 的样式与交互可迁移复用
4. **开发体验** → UI 用熟悉的 React；仅文件读取与日志监听等少量后端逻辑用 Rust（量小、有现成模板）

#### 前端技术细节
- **框架**：React + TypeScript
- **构建**：Vite（Tauri 默认集成）
- **图表**：Recharts 或 ECharts（按模型 / MCP / Skill 的分布图、趋势图）
- **样式**：可沿用 `dashboard.html` 既有设计，迁移为 React 组件

#### 分工
- **前端（React + TS）**：仪表盘 UI、图表、交互、菜单栏弹窗
- **后端（Rust）**：JSONL 增量解析、FS 文件监听、配置加载、LiteLLM 价格表拉取、聚合计算
- **桥接**：Tauri command / event 在前后端间传递聚合结果

### 6.2 架构概览
```
┌─────────────────────────────────────────┐
│        macOS 菜单栏 UI（壳）           │
├─────────────────────────────────────────┤
│  仪表盘视图层（图表 / 列表 / 排行）    │
├─────────────────────────────────────────┤
│  聚合层（按时间 / 模型 / MCP / Skill）  │
├─────────────────────────────────────────┤
│  内存模型 + 文件指纹缓存（轻量）        │
├─────────────────────────────────────────┤
│  采集层（FS Watcher + JSONL 增量解析）  │
├─────────────────────────────────────────┤
│  配置加载（mcpServers / skills 白名单） │
└─────────────────────────────────────────┘
            ↑ 读取
   ~/.claude/projects/**/*.jsonl
   ~/.claude.json
   ~/.claude/skills/
```

### 6.3 用户安装方式

按技术栈不同，分发与安装方式如下：

| 技术栈 | 产物 | 安装方式 |
|--------|------|---------|
| **Swift + SwiftUI** | `.app` / `.dmg` | 拖入 Applications；或 `brew install --cask tokenscope`（提交到 Homebrew Cask） |
| **Tauri** | `.dmg` / `.app` | 拖入 Applications；或 Homebrew Cask |
| **Electron** | `.dmg` / `.app` | 拖入 Applications；或 Homebrew Cask |

#### 推荐分发渠道（优先级）
1. **Homebrew Cask**（首选）—— 开发者用户习惯 `brew install`，一行命令安装与升级
2. **GitHub Releases**—— 直接下载 `.dmg`，附自动更新（Sparkle / Tauri Updater）
3. **直接构建**—— 开源仓库，开发者可自行 `clone + build`

#### 关键安装注意点
- **代码签名 + 公证（Notarization）**：未签名应用首次打开会被 Gatekeeper 拦截，需 Apple Developer 账号（$99/年）签名公证，否则用户需手动「右键打开」
- **磁盘访问权限**：应用需读取 `~/.claude/` 目录，沙盒化版本需声明对应权限；非沙盒（非 App Store）版本无需特殊授权
- **开机自启**：通过 `ServiceManagement` framework（Swift）或对应插件注册 Login Item，设置中可开关
- **不上架 Mac App Store**（v1）：App Store 沙盒对读取 `~/.claude/` 任意路径限制较多，且审核周期长；优先走 Homebrew / 直接下载

### 6.4 代码签名与公证（Code Signing & Notarization）

为让用户"双击直接打开、无 Gatekeeper 拦截"，并证明发布者身份，需对 macOS 产物做 **Developer ID 签名 + Apple 公证**。

#### 签名层级
| 层级 | 签名方式 | 用户体验 | 成本 |
|------|---------|---------|------|
| 未签名 / Ad-hoc | `codesign -s -` | 首次打开报"无法验证开发者"，需右键→打开 | 免费 |
| 自签名证书 | 自建证书 | 仍报警告（系统不信任自建根） | 免费但**对外无意义** |
| **Developer ID**（正解） | Apple 颁发的 `Developer ID Application` 证书 | 双击直开，Gatekeeper 放行 | **$99/年** |

> macOS 上唯一被系统信任、能"证明开发者身份"的方式，是加入 Apple Developer Program，用 Apple 签发的 Developer ID 证书签名。自签名证书系统不认，等同未签名。

#### 正规流程（签名 → 公证 → 钉票）
现代 macOS（10.15+）仅签名不够，**必须公证**：上传 App 给 Apple 自动扫描，通过后取回票据再"钉"回产物。
```
codesign（Developer ID + Hardened Runtime + 时间戳）
   ↓
打包 .dmg / .zip
   ↓
notarytool submit（上传 Apple，等待 Approved）
   ↓
stapler staple（公证票据钉入 .dmg/.app）
```

#### Tauri 集成
Tauri 原生支持，配好环境变量后 `tauri build` 自动完成签名+公证：
- `tauri.conf.json` → `bundle.macOS`：`hardenedRuntime: true`（公证强制要求）、`signingIdentity`（默认 `-` ad-hoc，被环境变量覆盖）
- 环境变量优先级：`APPLE_SIGNING_IDENTITY` > 配置；未设证书时自动退化为 ad-hoc/未签名，不阻断本地构建

#### CI 自动签名（GitHub Actions）
`release.yml` 的 `tauri-action` 已预置以下 Secret 占位，**未设置时照常出未签名包**，配齐后打 tag 即自动签名+公证：

| Secret | 内容 |
|--------|------|
| `APPLE_CERTIFICATE` | Developer ID `.p12` 的 base64 |
| `APPLE_CERTIFICATE_PASSWORD` | 导出 `.p12` 时设置的密码 |
| `APPLE_SIGNING_IDENTITY` | `Developer ID Application: Name (TEAMID)` |
| `APPLE_ID` | Apple ID 邮箱 |
| `APPLE_PASSWORD` | App 专用密码（appleid.apple.com 生成，非登录密码） |
| `APPLE_TEAM_ID` | 10 位 Team ID |

#### 渐进策略
- **当前（v1 自用/小范围）**：未签名，文档注明"右键→打开"或 `xattr -dr com.apple.quarantine Tokenscope.app`
- **公开分发（Homebrew Cask / 陌生用户下载）**：上 Developer ID（$99/年），填齐 Secret，签名管线一劳永逸

---

## 7. 非功能需求

- **性能**：菜单栏常驻内存 < 100MB，CPU 空闲时 < 1%
- **隐私**：所有数据本地处理，不上传任何日志或统计信息
- **响应**：日志写入后 5 秒内反映到仪表盘
- **稳定性**：日志解析容错（坏行跳过，不崩溃）
- **启动**：开机自启可选

---

## 8. 范围与边界

### 8.1 v1.0 范围（MVP）
- ✅ 菜单栏图标 + 今日用量速览
- ✅ 详细仪表盘（今日/本周/本月切换）
- ✅ Token 用量、估算花费、按模型分布
- ✅ 用户 MCP / Skill 调用统计

### 8.2 不在 v1.0 范围
- ❌ 跨设备同步
- ❌ 团队/多用户聚合
- ❌ Windows / Linux 支持
- ❌ Web 端
- ❌ 修改 Claude CLI 配置（只读）

### 8.3 后续可能扩展
- 月度报告导出（PDF / Markdown）
- 预算预警（接近设定金额时通知）
- 跨平台版本（Tauri 改造）
- 与其他 AI CLI 工具集成（Cursor、Codex 等）

---

## 9. 关键决策记录

1. **数据采集方式**：直接解析 JSONL 日志，不通过 hook 或代理 —— 零侵入、零配置
2. **MCP/Skill 过滤策略**：仅展示用户自定义安装的，过滤所有内置工具和 Anthropic 自带 MCP —— 聚焦真正的"用户行为"，不被高频内置工具淹没
3. **花费定位**：标注为"估算"，不承诺等同账单 —— 避免与订阅制实际支出混淆
