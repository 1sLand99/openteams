# ACP v1 通用执行器设计

状态：Proposed  
目标版本：ACP v1 stable / Rust SDK 1.0  
适用范围：`crates/executors`、Chat Runner、Workflow Runtime、Gemini/Qwen Runtime 配置
不在范围：ACP v2、远程 HTTP/WebSocket transport、厂商私有能力扩展

## 1. 摘要

OpenTeams 将新增一个厂商无关的 ACP v1 通用执行器。它的协议行为只由 ACP v1
稳定规范、初始化返回的能力以及 Session 返回的配置选项决定，不以 Gemini CLI、
Qwen Code 的现状反推通用设计。通用核心只作为 Gemini/Qwen 的内部执行基础，并通过
隐藏的 QA runner 验证；本阶段不向用户开放第三方 ACP Agent 接入。

核心决策：

1. 将 `agent-client-protocol` 从 `0.8 + unstable` 升级到稳定的 1.0 SDK。
2. `initialize` 是强制握手，不再忽略响应；后续请求必须受协商结果约束。
3. 模型、推理强度、模式及其他 Session 参数统一使用 `configOptions` 和
   `session/set_config_option`，不再依赖通用层中的厂商 CLI 参数。
4. 审批是 OpenTeams 客户端策略，统一为 `ask | auto_allow | auto_reject`。
5. Follow-up 优先使用 Agent 原生 `session/resume`，其次使用 `session/load`；
   通用层不通过拼接历史 Prompt 伪造恢复。
6. Gemini、Qwen 只能在通用执行器通过协议一致性测试之后接入。厂商兼容代码必须
   隔离，不能进入 ACP 核心。
7. 迁移采用“新核心先落地、逐个迁移、验证后删除旧路径”的方式，避免影响现有 Free
   Chat、Workflow、审批、日志、Session ID 和 token 统计。
8. MCP Server 是首期必做能力；每次创建或恢复 Session 都传递本次运行明确允许的
   MCP Server 完整列表，不能依赖厂商配置文件被 Agent 隐式加载。
9. 不增加生产环境可选的 Generic ACP runner、公开配置 Schema 或配置 UI；协议核心
   通过 test/`qa-mode` runner 接入 Free Chat 和 Workflow 做集成验证。

## 2. 设计真源

按以下优先级解释行为：

1. ACP v1 稳定 Schema 和协议文档。
2. `initialize` 返回的协议版本、Agent 信息和能力。
3. `session/new`、`session/load` 或 `session/resume` 返回的 `configOptions`。
4. OpenTeams 本地安全和执行策略。
5. 厂商兼容适配，仅用于协议能力缺失或实现偏差。

当厂商文档、现有代码或历史行为与 ACP v1 稳定规范冲突时，以协议规范为准。厂商差异
只能作为显式兼容项存在，并应具备删除条件。

规范参考：

- [ACP 官方文档](https://agentclientprotocol.com/)
- [ACP 架构与 MCP](https://agentclientprotocol.com/get-started/architecture)
- [ACP Session Config Options](https://agentclientprotocol.com/protocol/v1/session-config-options)
- [ACP Tool Calls and Permissions](https://agentclientprotocol.com/protocol/v1/tool-calls)
- [ACP 更新记录](https://agentclientprotocol.com/updates)

## 3. 背景与现状问题

当前 `crates/executors/src/executors/acp/` 已经为 Gemini、Qwen 提供了部分 ACP
能力，但它不是一个完整、厂商无关的 ACP v1 执行器。

### 3.1 协议层问题

- 依赖仍是 `agent-client-protocol = 0.8`，并开启 `unstable`。
- `initialize` 结果被忽略，没有校验版本，也没有保存 Agent capabilities。
- 使用已经移出稳定协议的 `session/set_model` 路径。
- Session mode、model、reasoning 没有统一通过 `configOptions` 管理。
- 文件系统和 Terminal 客户端方法全部返回 `method_not_found`。
- `session/list`、`session/resume`、`session/close`、`session/delete` 等稳定生命周期
  没有完整接入。
- 多种稳定 Session update 被归入 `Other` 后忽略。
- MCP servers、additional workspace roots、message ID、usage update 和
  session info 没有形成完整通用路径。

### 3.2 厂商耦合

- `AcpAgentHarness::new()` 默认使用 `gemini_sessions`。
- 通用 Harness 中硬编码 `runtime_agent = gemini` 和 `provider_id = google`。
- Gemini/Qwen 的 model、thinking effort、yolo 由 CLI 参数或临时环境配置实现。
- Qwen 通过 Gemini 模块 re-export Harness，形成不必要的反向依赖。

### 3.3 Session 与事件管线问题

- Follow-up 创建新 ACP Session，再把本地 JSONL 历史拼进 Prompt；这不是原生
  ACP Session 恢复。
- 本地 `SessionManager` 同时承担事件存储、历史压缩、Session fork 和 Resume Prompt
  生成，职责混杂。
- ACP update 先转换成 `AcpEvent`，序列化到伪 stdout，再由 normalizer 解析；同时存在
  重复的 `SessionNotification -> AcpEvent` 转换。
- 依靠固定 500ms sleep 等待日志消费，完成条件不是确定性的。

### 3.4 审批问题

- 是否注入 `ExecutorApprovalService` 被同时当作“是否需要审批”和“自动允许”开关。
- 自动允许在找不到 allow 选项时会退到第一个任意选项，可能意外选择拒绝。
- `yolo` 是 Gemini/Qwen 的厂商配置名，不适合作为 ACP 通用语义。

## 4. 目标与非目标

### 4.1 目标

- 为 Gemini、Qwen 提供同一套符合 ACP v1 稳定协议的内部执行路径。
- 完整执行初始化与能力协商，并只调用 Agent 声明支持的可选方法。
- 为模型、推理、模式和 Agent 自定义配置提供统一内部配置机制。
- 支持明确、可测试的审批策略。
- 支持 ACP v1 客户端文件系统和 Terminal 服务，并执行工作区安全约束。
- 支持 ACP v1 MCP Server 配置解析、transport 能力门控，并在 new/load/resume 时
  注入本次允许的完整列表。
- 使用 Agent 原生 Session ID 和生命周期能力完成连续对话。
- 保持现有 `StandardCodingAgentExecutor`、`SpawnedChild`、`MsgStore`、Chat Runner
  和 Workflow Runtime 的外部行为不变。
- 删除被通用实现替代的重复、厂商化和历史补丁代码。

### 4.2 非目标

- 不实现 ACP v2 或双协议协商。
- 不实现远程 HTTP/WebSocket transport；第一阶段只支持本地 stdio。
- 不实现仍处于草案阶段的 MCP-over-ACP transport；首期仅使用 ACP v1 稳定的
  stdio、HTTP 和 SSE MCP Server 描述。
- 不要求所有 Agent 实现所有可选能力。
- 不在第一阶段实现 ACP Registry 自动安装和升级。
- 不向用户开放第三方 Generic ACP runner、启动命令配置或专用配置 UI。
- 不重构所有执行器公共接口，也不改变非 ACP Agent 的运行逻辑。
- 不把 Agent 私有扩展提升为 OpenTeams 通用能力。

## 5. 设计原则与不变量

1. **协议优先**：核心模块中不得出现 `gemini`、`qwen`、Google、Alibaba 等厂商分支。
2. **能力门控**：未协商成功或未声明的可选能力不得调用。
3. **Agent 默认可运行**：缺少用户偏好或配置不匹配时保留 Agent 默认值。
4. **配置 ID 不可推断**：配置选项 ID 由 Agent 定义；category 只用于语义定位。
5. **Session ID 透明传递**：Agent Session ID 视为 opaque string，不解析、不改写。
6. **安全边界在客户端**：文件、Terminal、审批不能依赖 Agent 自觉限制。
7. **单一事件映射**：每个 ACP update 只转换一次，不能在 client 和 normalizer 中复制
   业务映射。
8. **确定性结束**：依靠 channel close、任务 join 和 drain acknowledgement 完成收尾，
   不依赖固定 sleep。
9. **渐进迁移**：删除旧代码之前必须有通用实现、测试和对应 Agent 的行为等价验证。
10. **失败可诊断**：协议版本、Agent 信息、能力、Session ID、方法和错误码必须进入
    结构化日志，但不能记录密钥。

## 6. 总体架构

```text
Gemini/Qwen/ACP QA fixture 启动描述
                 │
                 ▼
        AcpRuntime（厂商无关）
     ┌───────────┼────────────┐
     ▼           ▼            ▼
  Process     Connection    Session
  + stdio     + initialize  + config
              + auth        + prompt
              + caps        + lifecycle
     └───────────┼────────────┘
                 ▼
           AcpClientServices
       permission / fs / terminal
                 │
                 ▼
          AcpEventProjector
                 │
                 ▼
 SpawnedChild / MsgStore / Chat Runner / Workflow
```

### 6.1 模块职责

建议将现有 `acp/` 调整为：

```text
crates/executors/src/executors/acp/
├── mod.rs              # Acp executor、公共类型和导出
├── config.rs           # 启动配置、Session 偏好、审批和客户端服务策略
├── runtime.rs          # 子进程、stdio、initialize、auth、取消和退出
├── session.rs          # new/load/resume/list/close/delete、prompt、configOptions
├── mcp.rs              # 允许列表解析、canonical config 转换和 transport 门控
├── client.rs           # Agent 调用 Client 时的 permission/fs/terminal 实现
├── events.rs           # Session update 到唯一内部事件模型的转换
├── output.rs           # 有界事件队列、诊断日志、transcript 和确定性 drain
├── normalize_logs.rs   # 内部事件到现有 MsgStore patch 的兼容投影
└── tests.rs            # fake Agent 和协议一致性测试
```

文件数量保持有限：不为每个 JSON-RPC 方法单独建模块，也不预留 ACP v2 抽象。

### 6.2 主要类型

```rust
pub struct Acp {
    pub agent_id: String,
    pub base_command: String,
    pub append_prompt: AppendPrompt,
    pub approval_policy: AcpApprovalPolicy,
    pub session_preferences: AcpSessionPreferences,
    pub client_services: AcpClientServicePolicy,
    pub mcp: AcpMcpPolicy,
    pub cmd: CmdOverrides,
    pub approvals: Option<Arc<dyn ExecutorApprovalService>>,
}

pub enum AcpApprovalPolicy {
    Ask,
    AutoAllow,
    AutoReject,
}

pub struct AcpSessionPreferences {
    pub model: Option<String>,
    pub thought_level: Option<String>,
    pub mode: Option<String>,
    pub option_values: BTreeMap<String, AcpConfigValue>,
}

pub struct AcpClientServicePolicy {
    pub filesystem: bool,
    pub terminal: bool,
}

pub struct AcpMcpPolicy {
    pub enabled: bool,
    pub allowed_server_names: Option<BTreeSet<String>>,
}
```

`base_command` 继续交给现有 `CommandBuilder` 和 `CmdOverrides` 解析，环境变量继续由
`AgentRuntimeConfig.env_json` 与 `ExecutionEnv` 管理，避免创建第二套 secret store。

## 7. 配置模型

配置分为四层，职责不能混用。

### 7.1 启动配置

负责找到并启动 Agent：

```json
{
  "agent_id": "example-agent",
  "base_command": "example-agent --acp",
  "additional_params": [],
  "env": {}
}
```

- `agent_id` 用于展示、日志和未来 Registry 对接，不参与协议判断。
- 命令、参数、cwd 和环境变量在启动前固定。
- model、reasoning 和 approval 不属于启动协议配置。

### 7.2 协议初始化配置

OpenTeams 固定请求 `ProtocolVersion::V1`，同时发送：

- OpenTeams 的 `clientInfo`。
- 实际实现的 Client capabilities。
- Boolean config option capability。
- 文件系统、Terminal、认证等按本地 policy 启用的能力。

收到响应后必须：

1. 验证返回版本为 v1。
2. 保存 `agentInfo` 和 `agentCapabilities`。
3. 生成连接级 `NegotiatedCapabilities` 快照。
4. 若 Agent 需要认证，进入认证流程。
5. 后续所有 capability-gated 方法查询该快照。

初始化失败、版本错误或响应不可解析均为启动失败，不能继续创建 Session。

### 7.3 Session 配置

模型、推理强度、模式等设置来自 Session 返回的 `configOptions`：

| OpenTeams 配置 | ACP category | 设置方式 |
| --- | --- | --- |
| `model_name` | `model` | `session/set_config_option` |
| `thinking_effort` | `thought_level` | `session/set_config_option` |
| `mode` | `mode` | `session/set_config_option` |
| 其他模型参数 | `model_config` 或精确 option ID | `session/set_config_option` |

应用算法：

1. 创建或恢复 Session，读取最新 `configOptions`。
2. 先定位唯一的 `category=model` 选项并应用模型偏好。
3. 使用该响应返回的最新选项重新解析后续配置。
4. 依次应用 `thought_level`、`mode`。
5. 最后应用用户按精确 option ID 保存的 `option_values`。
6. 每次设置后用响应中的完整 `configOptions` 更新本地状态。
7. 找不到选项、category 不唯一或值无效时记录 warning，保留 Agent 默认值。

禁止行为：

- 不能把 option ID 写死为 `model`、`reasoning` 或 `mode`。
- 不能仅根据显示名称匹配。
- 不能在 Session 返回 `configOptions` 之前发送设置。
- 不能继续调用已移除的 `session/set_model`。
- 通用核心不能通过 CLI 参数或临时设置文件注入模型和推理强度。

### 7.4 OpenTeams 执行策略

以下配置是客户端策略，不由 ACP Agent 定义：

- 审批策略。
- 工作区根目录和 additional directories。
- 是否暴露文件系统和 Terminal Client 服务。
- Terminal 数量、输出大小、进程结束超时。
- 日志保留和敏感字段脱敏。

## 8. 审批设计

### 8.1 策略语义

| 策略 | 行为 |
| --- | --- |
| `ask` | 调用现有 `ExecutorApprovalService`，等待用户选择 |
| `auto_allow` | 选择 `allow_always`，否则 `allow_once`，否则 Cancelled |
| `auto_reject` | 选择 `reject_always`，否则 `reject_once`，否则 Cancelled |

不得退回“选择 options 中第一个值”。

### 8.2 与现有配置的迁移

- Gemini/Qwen 的 `yolo: true` 迁移为 `auto_allow`。
- `yolo: false` 迁移为 `ask`。
- 通用 ACP 核心默认使用 `ask`。
- 迁移期间保留旧 profile 的反序列化兼容，但新配置和 UI 不再暴露 `yolo`。

### 8.3 用户拒绝

- 有拒绝理由时，先返回合适的 reject option。
- 是否将拒绝理由作为后续用户消息发送，必须是显式 OpenTeams 行为，不能由
  permission client 隐式创建 Prompt 循环。
- 取消或超时返回 `Cancelled`，并记录 tool call ID、Session ID 和原因。

## 9. 能力协商与降级

ACP v1 中许多能力是可选的。“通用”表示能够正确协商和降级，不表示假定所有 Agent
都支持全部方法。

| 能力 | OpenTeams 行为 |
| --- | --- |
| `session/new` | 必须；缺失则不能执行 |
| `session/load` | 声明支持时可用于带历史恢复 |
| `session/resume` | Follow-up 首选，不要求 Agent 重放历史 |
| `session/list` | 声明支持时用于发现 Agent Session |
| `session/close` | 声明支持时用于释放活动 Session 资源 |
| `session/delete` | 仅在用户明确删除且 Agent 声明支持时调用 |
| config options | Agent 提供时应用；缺失时使用默认配置 |
| permission request | 根据 OpenTeams approval policy 回应 |
| filesystem | 仅 OpenTeams policy 开启并在 initialize 中声明后提供 |
| terminal | 仅 OpenTeams policy 开启并在 initialize 中声明后提供 |
| MCP servers | 首期必做；new/load/resume 均传递当前允许的完整配置 |
| additional directories | 只传递已解析、已授权的绝对目录 |
| message ID | 存在时持久化；v1 缺失时允许继续 |
| usage update | 转换成现有 token/context/cost 元数据 |
| `_meta` | 按规范传播，不依赖其中的厂商字段完成核心逻辑 |

### 9.1 MCP Server 支持（首期必做）

#### 配置真源

ACP 核心不能让 Agent 自己从 Gemini、Qwen 或其他厂商配置文件中隐式寻找 MCP
Server。OpenTeams 在每次运行前生成 `EffectiveAcpMcpConfig`，其输入按优先级为：

1. 当前 Agent Runtime/Team MCP 设置中的 canonical server definitions。
2. 当前成员或 Agent 的 MCP server allowlist。
3. Executor profile 中的禁用项或本次 Session 的安全限制。

`allowed_server_names` 为显式 allowlist；存在时只传其中的 Server。没有显式 allowlist
时，沿用当前已配置并启用的 Server，以保持现有行为。内置 Server 模板只是候选定义，
没有被用户配置或启用时不得自动注入。

现有 `McpConfig` 和 Team MCP 配置界面继续作为配置入口，但 ACP 路径读取 canonical
配置后直接转换为协议类型，不再先转换成 Gemini/Qwen/Codex 等厂商配置格式。

#### Canonical 类型转换

```text
OpenTeams canonical stdio -> AcpMcpServer::Stdio
OpenTeams canonical http  -> AcpMcpServer::Http
OpenTeams canonical sse   -> AcpMcpServer::Sse
```

转换规则：

- stdio 是 ACP v1 Agent 必须支持的 MCP transport。
- HTTP 只有在 initialize 返回的 MCP capabilities 声明支持时才能传递。
- SSE 只有在 initialize 返回的 MCP capabilities 声明支持时才能传递。
- Server name 必须非空且在本次列表中唯一。
- stdio command 必须非空；args 必须为字符串数组；env key 必须合法。
- HTTP/SSE URL 必须可解析；headers 只接受字符串键值。
- `_meta` 仅复制 OpenTeams 明确允许的键，不能整包透传厂商 metadata。
- 不执行 shell 展开，也不把 command 与 args 拼成 shell 字符串。

一个被明确允许的 Server 如果配置无效或 transport 不受 Agent 支持，必须在
`session/new/load/resume` 前返回带 Server name 的配置错误，不能静默丢弃后继续运行。

#### Session 注入

`session/new`、`session/load` 和 `session/resume` 均必须显式携带本次解析得到的完整
`mcpServers` 列表：

```json
{
  "cwd": "/workspace/project",
  "mcpServers": [
    {
      "name": "project-tools",
      "command": "project-mcp",
      "args": ["stdio"],
      "env": []
    }
  ]
}
```

规则：

- 每次 new/load/resume 前重新解析配置，不能复用历史 Session 中的隐式授权。
- Server 被撤销后，下一次恢复必须发送不含该 Server 的完整列表。
- 空列表也要显式传递，不能依赖 Agent 恢复旧 MCP 配置。
- ACP v1 没有首期采用的通用 mid-session MCP 重配置方法；运行中的配置变化从下一次
  resume/load 生效。
- Session ID 与 MCP 配置无绑定推断；同一 Session 在不同恢复请求中可以获得不同的
  允许列表。

#### Secret 与安全边界

- stdio env、HTTP/SSE headers 允许包含密钥，但只能从现有安全配置源解析。
- env/header value 不进入 tracing、run log、协议摘要、错误文本或 UI。
- 诊断信息只记录 Server name、transport、配置哈希和成功/失败状态。
- 不记录完整 command args，因为参数本身也可能包含 token。
- 未经允许的 Server 不能通过 `_meta`、厂商配置 fallback 或历史 Session 恢复重新
  出现。
- 注入 Server 不等于预先批准它的全部工具调用；Agent 发出 permission request 时仍按
  OpenTeams approval policy 处理。
- Agent 直接连接 MCP Server，OpenTeams 不能假定 Agent 会为每次 MCP tool call 请求
  permission，因此 Server allowlist 是必须由 OpenTeams 强制执行的硬边界。

#### 与 Gemini/Qwen 的迁移

Gemini/Qwen 迁移到通用执行器后，也必须使用 Session `mcpServers` 注入。适配阶段需要
验证 Agent 是否还会同时加载自己的全局 MCP 配置；若会，兼容适配器必须避免同名
Server 被重复连接。厂商配置读取只能作为 canonical definition 来源，不能绕过本次
allowlist。

Follow-up 选择：

```text
有 session/resume  -> resume
否则有 session/load -> load
否则                -> FollowUpNotSupported
```

通用层不降级为“新建 Session + 历史拼接 Prompt”。Gemini/Qwen 如果暂时依赖该行为，
只能在迁移适配阶段保留局部兼容，验证原生恢复后删除。

## 10. 执行生命周期

### 10.1 新 Session

1. 解析 command、cwd 和 `ExecutionEnv`。
2. 启动进程组并接管 stdio。
3. 建立 ACP Client connection。
4. 发送 v1 `initialize`，验证响应并保存能力。
5. 完成必要认证。
6. 解析、校验并按 Agent capabilities 转换当前允许的 MCP servers。
7. 调用 `session/new`，传入 cwd、additional directories 和完整 MCP server 列表。
8. 持久化 Agent 返回的 Session ID。
9. 根据 Session `configOptions` 应用偏好。
10. 发送 `session/prompt`。
11. 持续消费 Session updates、permission requests 和 usage updates。
12. 收到 Prompt response 后记录 stop reason。
13. 等待事件 projector 完成 drain，再发送 `ExecutorExitResult`。

### 10.2 Follow-up

1. 使用 DB 中的 opaque Agent Session ID。
2. 初始化新连接并重新协商能力。
3. 重新解析当前允许的 MCP servers，并按最新 Agent capabilities 转换。
4. 携带完整 MCP server 列表调用 `session/resume` 或 `session/load`。
5. 使用恢复响应中的最新 `configOptions` 重新校验配置。
6. 发送当前 Prompt；不得重复发送历史消息。

### 10.3 取消

OpenTeams `CancellationToken` 触发后：

1. 请求取消正在等待的 JSON-RPC request。
2. 发送 Session cancel。
3. 取消仍未结束时等待一个短的可配置 grace period。
4. 超时后终止整个进程组。
5. Terminal Client 创建的所有子进程也必须终止。
6. 最终状态为 cancelled，不误报 success。

### 10.4 关闭与删除

- 进程退出不等于删除 Agent Session。
- `session/close` 用于释放活动资源，不清除 Session 历史。
- `session/delete` 只响应明确用户操作，不在正常 run 结束时自动调用。
- 未声明 close/delete 能力时只更新 OpenTeams 本地状态，不发送非法请求。

## 11. Client 服务与安全

### 11.1 文件系统

实现 v1 text file read/write 时必须：

- 只允许有效 Session 的主 cwd 和已授权 additional directories。
- 将相对路径解析到所属 root。
- 拒绝 `..`、root 之外的绝对路径和经 symlink 逃逸的路径。
- 写入前校验最近存在的父目录 canonical path。
- 设置单次读取、写入和返回内容大小上限。
- 不允许访问 OpenTeams secret store、其他 Session worktree 或任意 home 目录。
- 错误返回协议错误，不把宿主机绝对敏感路径写入用户消息。

### 11.2 Terminal

实现 v1 Terminal Client 服务时必须：

- cwd 使用与文件系统相同的 root guard。
- 每个 Terminal 绑定 Session ID 和连接 ID。
- 使用进程组，确保 cancel、release 和 connection drop 能清理后代进程。
- 对并发 Terminal 数、缓存输出字节数和空闲时间设置上限。
- 输出采用有界 ring buffer，避免 Agent 不读取导致内存无限增长。
- `wait_for_exit`、`kill`、`release` 必须幂等。
- 不把 Agent 提供的字符串拼接进额外 shell 命令。
- 环境变量以当前 `ExecutionEnv` 为基础，并过滤 OpenTeams 内部 secret。

### 11.3 认证

- 只使用 v1 SDK 中的稳定 authentication types。
- Agent 声明的认证方法用于生成 UI/操作，不猜测登录命令。
- Terminal authentication 只有在 Client 显式声明对应能力时才能使用。
- API key、token、OAuth code 不进入 tracing、ACP transcript 或 run output。

## 12. 事件模型与投影

`client.rs` 只接收协议回调，`events.rs` 是唯一的协议到内部事件转换器，
`normalize_logs.rs` 只负责内部事件到现有 OpenTeams patch 的投影。

### 12.1 ACP 输出类型与 stdout 契约

本地 stdio 模式下，Agent stdout 是 ACP JSON-RPC 消息流，不是面向用户的普通文本日志。
原始 stdout 必须由 ACP SDK 独占解析，不能直接写入 Chat 消息或 run log。输出分为：

1. Client 请求的响应，例如 initialize、new/resume 和 prompt response。
2. `session/update` 通知，例如 message、thought、tool、plan、config 和 usage。
3. Agent 发向 Client 的请求，例如 permission、filesystem 和 terminal。
4. stderr 中的进程诊断信息。

Agent 写入 stdout 的非 ACP 内容视为协议错误。错误中只保留有大小上限的脱敏片段，
完整内容不能进入用户对话。stderr 独立处理，不能被误识别成 assistant message。

### 12.2 三通道模型

日志管线必须明确拆成三个互不混用的通道：

| 通道 | 内容 | 消费方 | 用户可见性 |
| --- | --- | --- | --- |
| 协议通道 | 原始 JSON-RPC request/response/notification | ACP SDK | 不直接可见 |
| 产品事件通道 | 经过一次转换的 typed runtime event | MsgStore projector | 按事件类型展示 |
| 诊断通道 | stderr、阶段耗时、错误码、能力摘要 | tracing/run diagnostics | 仅错误或诊断 UI |

权限、filesystem 和 terminal 是控制请求，直接由 `AcpClient` 响应；它们可以产生产品
事件和诊断记录，但不能排队等待产品事件消费者之后才执行。

### 12.3 内部事件信封

所有 Session update 统一转换为一个带路由和顺序信息的事件信封：

```rust
pub struct AcpRuntimeEvent {
    pub connection_id: Uuid,
    pub session_id: String,
    pub sequence: u64,
    pub received_at: DateTime<Utc>,
    pub message_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub payload: AcpRuntimeEventPayload,
}
```

- `session_id`、`message_id` 和 `tool_call_id` 都按 opaque string 处理。
- `sequence` 是 OpenTeams 在协议解码后按连接单调递增生成的本地序号，用于保持接收
  顺序和诊断丢失事件，不替代协议 ID。
- 多 Session 共用一个连接时必须按 `session_id` 路由，不能继续使用单一全局
  `stored_session_id`。
- message chunk 有 `messageId` 时按 ID 聚合；不同 ID 必须生成不同消息。
- v1 Agent 未提供可选 message ID 时，使用当前 Prompt/turn 内的兼容聚合器，并在
  thought、tool、plan、stop reason 或显式边界处结束当前消息，不能无限合并同类文本。
- tool call/update 按 `toolCallId` upsert；update 先于 create 到达时创建占位状态，后续
  补全。

| ACP 输入 | OpenTeams 输出 |
| --- | --- |
| Agent message chunk | assistant message patch |
| Agent thought chunk | thinking patch |
| Tool call / update | tool call 创建、状态、内容和结果 patch |
| Plan update | plan patch |
| Available commands | slash command patch |
| Config option update | 当前模型、推理或模式状态 |
| Session info update | Session title、时间等元数据 |
| Usage update | context、token 和 cost 元数据 |
| Message ID | `agent_message_id` |
| Permission request/result | approval activity 和 tool 状态 |
| Stop reason | run completion metadata |
| 未识别稳定 update | warning + 可诊断 raw type，不导致连接崩溃 |

内容块不能只保留 text。当前 UI 不支持的图片、resource 或其他稳定内容块至少应完整
保存在 run transcript，并产生可理解的降级占位，避免静默丢失。

### 12.4 背压、容量与事件保序

- 产品事件使用有界队列，禁止 `mpsc::unbounded_channel`。
- 队列满时对 Session update 施加背压，不丢弃 message、tool、permission result、
  usage 或 completion 等产品事件。
- tracing debug 可以采样或限速，但 error、协议错误和 run completion 不得丢弃。
- Terminal 输出使用独立有界 ring buffer，不能占满产品事件队列。
- 单个内容块、累计消息、tool result、stderr 行和 raw transcript 都设置字节上限。
- 超限必须产生结构化 truncation metadata，不能静默截断。
- 同一连接事件按 `sequence` 投影；不同 Session 之间不承诺额外排序。

### 12.5 确定性完成

固定 sleep 不能作为日志完整性的保证。Run 收尾顺序必须是：

1. 得到 prompt response、cancel 或 fatal error。
2. 停止或关闭本次连接的协议输入，并等待已进入的 Client callbacks 完成。
3. 关闭产品事件 producer。
4. Event projector 消费到 drain barrier，flush 最后一个 message/tool patch。
5. Projector 返回 acknowledgement。
6. 最后发送 `ExecutorExitResult` 和 Finished。

`ExecutorExitResult` 由协议结果、取消状态和子进程状态决定，不能通过是否出现某条
`Done` 日志推断。

### 12.6 Transcript 与脱敏

协议 transcript 默认只记录：

- 方向、时间、request ID、method、Session ID。
- payload 类型和字节数。
- error code、stop reason 和 capability 摘要。

默认不记录完整 Prompt、assistant 内容、环境变量、认证参数或 permission secret。
只有显式诊断模式才能保存脱敏后的 payload，并且必须限制单条大小、总大小和保留时间。
`_meta` 按 allowlist 提取；未知字段不能整包写入 tracing。

### 12.7 现有输出桥接的迁移

第一阶段继续保持 `SpawnedChild`/stdout/`MsgStore` 的外部契约，避免修改全部执行器，
但 ACP 内部必须使用单独的 `AcpOutputBridge`：

- 删除 normalizer 中重复的 `TryFrom<SessionNotification>`。
- 只有 `events.rs` 了解 ACP update variants。
- 原始 ACP stdout 不进入 `MsgStore`；兼容桥只传输 `AcpRuntimeEvent`。
- 事件最多序列化一次，normalizer 不再尝试解析原始 ACP JSON-RPC。
- 使用有界 channel 和 drain acknowledgement 代替无界 channel 与固定 500ms sleep。
- token usage 只读取稳定 `usage_update`；厂商 `_meta.quota` 解析移出通用核心。
- stderr 继续复用现有 `normalize_stderr_logs`，但必须限速、脱敏并与产品事件分开。

## 13. Session 与数据持久化

继续复用现有字段：

- `chat_session_agents.agent_session_id`
- `workflow_agent_sessions.agent_session_id`
- `agent_message_id`
- Chat run 日志、token metadata 和 changed-files capture

规则：

- 持久化 Agent 返回的真实 Session ID，不生成显示用替代 ID。
- 同一个 Workflow Agent Session 优先使用自己的 Agent Session ID，再使用
  Chat Session Agent 的 ID，保持现有恢复优先级。
- OpenTeams DB 和运行记录保存引用与投影；Agent 自己负责其 Session 历史。
- 通用路径不再维护 `~/.openteams/gemini_sessions` 或
  `~/.openteams/qwen_sessions` 历史副本。
- `_meta` 仅按传播规则保存必要部分，不作为恢复真源。

## 14. 错误处理与可观测性

### 14.1 错误分类

```text
LaunchError
TransportError
InitializeError
ProtocolVersionMismatch
AuthenticationRequired / AuthenticationFailed
CapabilityNotSupported
FollowUpNotSupported
SessionError
ConfigOptionRejected
PermissionError
FilesystemPolicyViolation
TerminalPolicyViolation
PromptError
AgentExited
Cancelled
```

错误策略：

- initialize、auth、new/resume/load、prompt 失败是 run 失败。
- Follow-up 时 Agent 同时缺少 `session/resume` 与 `session/load` 能力，返回
  `FollowUpNotSupported`；已选择的 session 方法调用失败仍归为 `SessionError`。
- 可选配置无法应用时 warning 并使用 Agent 默认值。
- 可选 update 无法显示时保留 transcript 并降级，不结束 run。
- JSON-RPC method not found 仅在能力声明与实际行为矛盾时记录兼容性错误。
- 不对非幂等请求做隐式重试。

### 14.2 结构化日志字段

- OpenTeams run/session/session-agent/workflow-session ID。
- Agent Session ID。
- Agent ID、`agentInfo` name/version。
- 协议版本和协商能力摘要。
- JSON-RPC method、error code、stop reason。
- permission policy 和所选 option kind。
- output event sequence、payload 类型、队列容量和 truncation 状态。
- 各阶段耗时：launch、initialize、auth、session、first update、prompt completion。

禁止记录完整环境变量、认证信息和未经处理的 secret。

## 15. 与现有系统的接入

### 15.1 保持不变

- `StandardCodingAgentExecutor::spawn/spawn_follow_up`。
- `SpawnedChild` 和进程组管理的外部契约。
- `ExecutorApprovalService`。
- `CmdOverrides`、`ExecutionEnv` 和 runtime env 合并优先级。
- Chat Runner 与 Workflow Runtime 对 executor 的调用方式。
- Session ID 从日志流进入 DB 的现有持久化路径。
- changed-files、run record、activity、token metadata 等外围能力。

### 15.2 新增隐藏 ACP QA runner

不在生产 `CodingAgent`/`BaseCodingAgent` 中增加对用户可见的 `ACP` runner。通用核心
通过 `test` 或 `qa-mode` 下的 `AcpQaExecutor` 接入，必要时使用仅 QA build 可见的
`ACP_QA` wire value，以验证：

- `StandardCodingAgentExecutor` 的 spawn/follow-up 契约。
- Free Chat 与 Workflow 的 Session ID、事件、审批、取消和退出投影。
- MCP、config options、FS/Terminal 和错误路径。
- fake Agent 的能力组合、背压与确定性 drain。

QA runner 的 command、Session 偏好和安全策略来自测试 fixture 或 QA 配置，不写入生产
默认 profile，不进入 Agent Runtime 列表，也不生成 `acp.json` 或前端配置 UI。

### 15.3 现有成员配置映射

继续接受当前 `MemberExecutionConfig`：

- `model_name` 映射到 ACP `category=model`。
- `thinking_effort` 映射到 ACP `category=thought_level`。
- `model_variant` 不自动解释为 ACP 值；只有显式配置映射时使用。

成员级配置仍覆盖 executor profile 和 Agent Runtime 默认值，但只有在 Agent 返回相应
config option 时才实际应用。

## 16. 代码修改地图

### 16.1 新增

| 文件 | 修改 |
| --- | --- |
| `crates/executors/src/executors/acp/config.rs` | 通用配置、策略和 Session 偏好 |
| `crates/executors/src/executors/acp/runtime.rs` | 启动、stdio、initialize、auth、取消、退出 |
| `crates/executors/src/executors/acp/mcp.rs` | canonical MCP 解析、allowlist、协议转换和能力门控 |
| `crates/executors/src/executors/acp/events.rs` | 唯一 ACP update 转换 |
| `crates/executors/src/executors/acp/output.rs` | 三通道输出、有界队列、transcript、drain barrier |
| `crates/executors/src/executors/acp/qa.rs` | `test`/`qa-mode` 隐藏 runner |
| `crates/executors/src/executors/acp/tests.rs` | fake Agent 与协议一致性测试 |

### 16.2 重写或修改

| 文件 | 修改 |
| --- | --- |
| `crates/executors/Cargo.toml` | 升级到稳定 ACP SDK 1.0，移除 `unstable` |
| `crates/executors/src/executors/acp/mod.rs` | 定义 `Acp` executor 和版本无关内部事件 |
| `crates/executors/src/executors/acp/client.rs` | 审批策略、FS、Terminal、认证回调 |
| `crates/executors/src/executors/acp/session.rs` | 改为原生 Session 生命周期和 config options |
| `crates/executors/src/executors/acp/normalize_logs.rs` | 只保留 OpenTeams 投影 |
| `crates/executors/src/executors/mod.rs` | 仅在 `qa-mode` 增加隐藏 `CodingAgent::AcpQa` |
| `crates/executors/src/model_sync.rs` | Gemini/Qwen member model/thought 到 ACP 偏好的映射 |
| `crates/executors/src/mcp_config.rs` | 提供不经过厂商 adapter 的 canonical MCP definitions |
| `crates/executors/default_profiles.json` | 迁移 Gemini/Qwen approval 默认值 |
| `crates/services/src/services/agent_runtime.rs` | Gemini/Qwen ACP 配置与 reasoning capability 映射 |
| `crates/services/src/services/member_execution.rs` | 将成员 MCP allowlist 合并到 effective ACP config |
| `crates/executors/src/executors/gemini.rs` | 通用核心稳定后改为启动描述/兼容适配 |
| `crates/executors/src/executors/qwen.rs` | 通用核心稳定后改为启动描述/兼容适配 |
| `shared/types.ts` | Gemini/Qwen 公共配置变化时通过类型生成更新，禁止手工编辑 |

### 16.3 删除

通用核心落地时立即删除：

- 忽略 initialize 结果的逻辑。
- `with_model`、`with_mode` 和 `session/set_model` 通用路径。
- common Harness 中硬编码的 Gemini/Google token metadata。
- permission option 的“任意第一个值”回退。
- client/normalizer 两套重复 Session update 转换。
- 无界 ACP event channel 和固定日志 flush sleep。

Gemini/Qwen 迁移并通过回归后删除：

- `AcpAgentHarness` 的厂商 namespace。
- 现有本地 `SessionManager`、历史 JSONL fork 和 Resume Prompt 拼接。
- `max_resume_prompt_bytes` 历史注入限制。
- Qwen 对 `gemini::AcpAgentHarness` 的 re-export 依赖。
- Gemini/Qwen 通过临时系统设置注入 thinking effort 的代码。
- Gemini/Qwen 用于 model/reasoning/approval 的 CLI 参数；仅当对应 Agent 已正确暴露
  config options 和 permission 行为后删除。

不得提前删除：

- 现有 Gemini/Qwen 默认 profile 和序列化兼容。
- 原生恢复尚未验证前维持现有 follow-up 所需的兼容路径。
- Chat/Workflow 的 Session ID 持久化和日志投影。

## 17. 实施顺序

### 阶段 0：行为基线

- 为当前 Gemini/Qwen 的启动、Session ID、消息、工具、审批、取消、错误和 token
  metadata 补充回归测试。
- 记录 Free Chat 与 Workflow 的关键输出形状。
- 不修改生产路径。

### 阶段 1：ACP v1 通用核心

- 升级 SDK。
- 实现 initialize、capability snapshot、auth。
- 实现 Session 生命周期、config options、prompt/cancel。
- 实现 MCP canonical config、allowlist、transport gating 和 new/load/resume 注入。
- 实现 permission、FS、Terminal。
- 实现统一事件映射。
- 使用 fake ACP Agent 完成一致性测试。
- 新增仅供 test/`qa-mode` 使用的隐藏 ACP QA runner；不进入生产配置或 UI。

### 阶段 2：Gemini 适配

- Gemini 仅提供启动命令。
- 优先使用 ACP config options、usage update 和原生 Session。
- 对 Gemini 未实现的能力建立最小、具名兼容项。
- 完成 Free Chat、Workflow、审批和 Follow-up 回归。

### 阶段 3：Qwen 适配

- 按与 Gemini 相同的通用接口接入。
- 验证 Qwen 自身的 capabilities，不复制 Gemini 条件。
- 完成 oversized prompt、Follow-up 和 tool update 回归。

### 阶段 4：删除旧路径

- 根据 16.3 的删除清单逐项删除。
- 禁止保留“新旧两套永久并行”。
- 运行全量类型生成、backend check、lint 和目标 E2E。

## 18. 测试方案

### 18.1 单元测试

- capability gating。
- config category/ID/value 解析与连续更新。
- `ask/auto_allow/auto_reject` 的所有 option 组合。
- workspace root、`..`、absolute path、symlink escape。
- MCP stdio/HTTP/SSE canonical 转换和 capability gating。
- MCP allowlist、禁用项、空列表、重复名称和非法配置。
- MCP env/header secret 脱敏与配置哈希。
- new/load/resume 每次重新解析并携带完整列表。
- Terminal create/output/wait/kill/release 和有界缓冲。
- 每种稳定 Session update 的事件映射。
- message ID、usage update、`_meta` 传播。
- 连续同类型但不同 message ID 的消息不能合并。
- 缺少 message ID 时的 turn 内兼容聚合。
- 多 Session 事件路由和单连接 sequence 保序。
- 有界队列饱和、内容截断和 Terminal ring buffer。
- stderr 与产品事件隔离、raw transcript 脱敏。
- cancellation、drain barrier 和尾部事件完整性。

### 18.2 Fake Agent 集成测试

测试 Agent 应能按用例返回不同能力：

- initialize 成功、版本不匹配、非法响应。
- 需要认证和认证失败。
- new/load/resume/list/close/delete。
- config options 依赖变化，例如切换 model 后 thought levels 改变。
- MCP stdio 成功、HTTP/SSE 支持与不支持、撤销后 resume、Server 启动失败。
- 文本、thought、tool、plan、command、session info、usage update。
- permission allow/reject/cancel。
- Agent 非正常退出、malformed JSON-RPC、prompt error。
- stdout 混入非协议文本和 stderr 高频输出。
- cancellation、慢速输出、队列饱和及 prompt response 前的最后一条 update。

### 18.3 回归测试

- Gemini：新 Session、Follow-up、模型、推理、工具、审批和自动允许。
- Qwen：同上，并覆盖历史大 Prompt 场景的迁移结果。
- Gemini/Qwen：MCP Server 不重复连接，allowlist 和撤销在 Follow-up 生效。
- Free Chat：stream、run status、Session ID、token 和 changed files。
- Workflow：worker/reviewer、重试、resume、interrupt、approval 和 transcript。

建议验证命令：

```bash
cargo test -p executors acp
pnpm run backend:check
pnpm run backend:lint
pnpm run generate-types:check
pnpm run frontend:check
```

外部真实 Agent conformance 测试不进入默认 CI，避免网络、登录和版本波动造成不稳定。

## 19. 验收标准

满足以下条件才视为 ACP v1 通用执行器完成：

1. 使用稳定 ACP 1.0 SDK，不启用 `unstable`。
2. initialize 响应被验证并形成能力快照。
3. 不调用 Agent 未声明的可选方法。
4. model、thought level、mode 通过 Session config options 设置。
5. 审批三种策略行为确定，不存在任意 option 回退。
6. FS/Terminal 通过安全测试，不能逃逸 Session workspace。
7. Follow-up 使用原生 resume/load；不在通用核心注入历史 Prompt。
8. new/load/resume 显式携带本次允许的完整 MCP server 列表，stdio、HTTP、SSE 按
   capabilities 正确门控，撤销和 secret 脱敏通过测试。
9. 稳定 v1 updates 均被投影或明确降级，不静默丢弃。
10. 原始 ACP stdout、产品事件和诊断日志三通道隔离。
11. 产品事件使用有界队列，并按 Session ID、message ID/tool call ID 正确路由和聚合。
12. Run 完成使用 drain acknowledgement，不依赖固定 sleep，尾部事件无丢失。
13. transcript 和 stderr 通过脱敏、限速、单条与总量限制测试。
14. ACP 核心不存在 Gemini/Qwen/provider 硬编码。
15. 隐藏 ACP QA runner 可覆盖 Free Chat 与 Workflow 集成，但在生产 Agent Runtime、
    默认 profile、公共 wire types 和前端 UI 中均不可见。
16. Gemini/Qwen 迁移后 Free Chat 与 Workflow 现有关键功能无回归。
17. 删除清单中的旧代码已按前置条件清理，没有永久双路径。

## 20. 推迟事项

以下内容在 v1 核心稳定后另行设计：

- ACP Registry 自动安装、升级和分发。
- 远程 HTTP/WebSocket transport。
- ACP v2 与双栈协商。
- 面向用户的第三方 Generic ACP runner、启动命令配置和专用 UI。
- Agent 私有 extension 的产品化展示。
- 跨 Client 的 Session 浏览与高级管理 UI。

这些事项不得阻塞 v1 通用执行器，也不得提前引入占位抽象增加首期复杂度。
