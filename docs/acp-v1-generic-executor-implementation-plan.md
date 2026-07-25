# ACP v1 通用执行器实施计划

状态：Proposed
设计真源：`docs/acp-v1-generic-executor-design.md`
实施范围：`crates/executors`、Gemini/Qwen Runtime、成员执行配置、Free Chat、Workflow
交付原则：先新增通用核心，再逐个迁移 Gemini/Qwen，最后删除旧路径

## 1. 交付目标

本计划把设计方案拆成可独立评审、可逐步验证、可回退的实施单元。最终交付应满足：

1. 隐藏 ACP QA runner 可以用 fake Agent 对通用核心做端到端验证，但不向用户开放
   第三方 ACP Agent 接入。
2. 初始化、能力门控、认证、Session 生命周期、config options 和 MCP 注入全部走稳定
   ACP v1 协议。
3. 审批、文件系统和 Terminal 由 OpenTeams 执行安全策略。
4. ACP update 只转换一次，产品事件使用有界队列，并通过 drain acknowledgement
   确定性结束。
5. Free Chat 和 Workflow 的现有 executor 调用契约、Session ID 持久化、日志、token
   metadata 和 changed-files 行为保持不变。
6. Gemini、Qwen 迁移验证后删除本地历史拼接和厂商化 ACP 核心。

本次不实施公开 Generic ACP runner、专用配置 UI、ACP v2、远程 transport、Registry
自动安装和 Agent 私有扩展。

## 2. 当前代码基线

开始开发前，以以下现状作为基线：

- `crates/executors/Cargo.toml` 使用 `agent-client-protocol = 0.8` 和 `unstable`。
- 当前 ACP 实现在 `acp/{harness,client,session,normalize_logs}.rs`。
- `harness.rs` 同时承担进程、协议、Session、日志桥接和退出处理，并包含
  Gemini/Google token metadata。
- `client.rs` 使用无界 channel，未实现 FS/Terminal；缺少审批服务时会回退到任意第一
  个 permission option。
- `session.rs` 保存本地 JSONL 并生成历史拼接 Prompt。
- Gemini 和 Qwen 共享 `AcpAgentHarness`，Qwen 依赖 Gemini re-export。
- `ExecutorError::FollowUpNotSupported` 已存在，ACP 内部新增的细分错误应映射到现有
  executor 外部错误契约。
- executor schema 的实际生成真源是
  `crates/server/src/bin/generate_types.rs`，生成产物是 `shared/schemas/*.json`；
  `shared/types.ts` 和 schema 都不得手工编辑。

阶段 0 必须把这些行为固化为测试和样例，后续 PR 不依赖人工记忆判断是否回归。

## 3. 里程碑与依赖

```text
M0 行为基线
  └─ M1 SDK 1.0 与通用骨架
       └─ M2 Runtime、Session、配置和错误
            ├─ M3 事件、输出与确定性结束
            └─ M4 MCP effective config 与注入
                 └─ M5 Client 审批、FS、Terminal 与安全
                      └─ M6 隐藏 ACP QA runner
                           └─ M7 Gemini 迁移
                                └─ M8 Qwen 迁移
                                     └─ M9 旧路径删除与全量验收
```

M3 和 M4 可以在 M2 的类型与 Session 接口稳定后并行开发；M6 只能在 M3、M4、M5
全部通过安全和一致性测试后开始。

## 4. 分阶段实施

### M0：建立行为基线

目标：只补测试和样例，不改变生产行为。

任务：

- 为 Gemini/Qwen 建立新 Session、Follow-up、消息、thought、tool、plan、审批、取消、
  Agent 异常退出和 token metadata 回归用例。
- 记录 Free Chat 和 Workflow 当前消费的 Session ID、日志 patch、退出结果和 token
  metadata 形状。
- 增加包含末尾 update、stderr、超大历史 Prompt 和 permission option 组合的测试。
- 保存一组脱敏的输入/输出 fixture，供新旧实现对比；fixture 不包含 token、环境变量或
  用户 Prompt 全文。

主要文件：

- `crates/executors/src/executors/acp/tests.rs`
- `crates/executors/src/executors/gemini.rs`
- `crates/executors/src/executors/qwen.rs`
- Chat Runner/Workflow 现有 executor 集成测试

退出条件：

- 测试可以稳定复现旧路径的关键行为。
- 没有生产代码变化。
- `cargo test -p executors acp` 和相关 Chat/Workflow 目标测试通过。

### M1：升级 SDK 并建立通用骨架

目标：完成 ACP 1.0 编译迁移，建立厂商无关模块边界，但不切换 Gemini/Qwen。

任务：

- 将 `agent-client-protocol` 升级到稳定 1.0 并移除 `unstable`。
- 按稳定 API 修复现有编译调用，不用兼容补丁恢复已删除的 `session/set_model`。
- 新增 `config.rs`、`runtime.rs`、`events.rs`、`output.rs`、`mcp.rs`。
- 在 `acp/mod.rs` 定义通用 `Acp` 配置、审批策略、Session 偏好、Client service policy
  和内部错误类型。
- 将 ACP 细分错误统一映射到 `ExecutorError`；明确
  `FollowUpNotSupported` 与 Session 方法调用失败的边界。
- 建立 fake ACP Agent 测试入口，支持配置 initialize 版本、capabilities、Session
  response 和 update 序列。

主要文件：

- `crates/executors/Cargo.toml`
- `Cargo.lock`
- `crates/executors/src/executors/acp/mod.rs`
- `crates/executors/src/executors/acp/config.rs`
- `crates/executors/src/executors/acp/runtime.rs`
- `crates/executors/src/executors/acp/events.rs`
- `crates/executors/src/executors/acp/output.rs`
- `crates/executors/src/executors/acp/mcp.rs`
- `crates/executors/src/executors/acp/tests.rs`

退出条件：

- 依赖树中 ACP SDK 为 stable 1.0，未启用 `unstable`。
- fake Agent 可以完成 initialize 和最小 new/prompt 流程。
- 通用模块中没有 Gemini、Qwen、Google、Alibaba 或厂商 session namespace。
- Gemini/Qwen 旧 runner 仍通过 M0 回归测试。

### M2：实现 Runtime、Session、配置和错误

目标：打通通用执行器的协议生命周期。

任务：

- `runtime.rs` 负责命令解析、进程组、stdio 独占、connection、initialize、认证、取消
  和进程退出。
- initialize 固定请求 v1，验证响应版本，保存 `agentInfo` 和
  `NegotiatedCapabilities`。
- `session.rs` 实现 new、resume、load、list、close、delete、prompt 和 cancel；
  所有可选方法先检查能力快照。
- Follow-up 按 resume、load 的顺序选择；两者都不支持时返回
  `FollowUpNotSupported`，不得创建新 Session 拼接历史。
- 将 Agent Session ID 作为 opaque string 传递和输出。
- 解析 Session 返回的 config options，按 model、thought level、mode、精确 option
  ID 的顺序设置；每次以响应中的完整 options 刷新本地状态。
- 找不到 option、category 不唯一或值无效时记录 warning 并保留 Agent 默认值。
- 初始化、认证、Session 和 Prompt 失败映射到设计中的错误分类，非幂等请求不自动
  重试。

主要文件：

- `crates/executors/src/executors/acp/runtime.rs`
- `crates/executors/src/executors/acp/session.rs`
- `crates/executors/src/executors/acp/config.rs`
- `crates/executors/src/executors/acp/mod.rs`
- `crates/executors/src/executors/acp/tests.rs`

测试重点：

- 版本匹配、不匹配、非法 initialize response。
- 认证成功、需要认证、认证失败。
- 每个 Session capability 的正向调用和未声明能力拒绝。
- model 切换后 options 动态变化。
- Follow-up 的 resume/load/不支持三分支。
- cancel、Agent 提前退出、Prompt error。

退出条件：

- 最小文本 Agent 可通过 new 和 Follow-up 完成两个 turn。
- 未声明的可选方法不会出现在 fake Agent 收到的请求中。
- 通用核心不调用 `session/set_model`，不读取本地历史 JSONL。

### M3：统一事件、输出与确定性结束

目标：替换重复转换、伪 stdout、无界队列和固定 sleep。

任务：

- 在 `events.rs` 定义 `AcpRuntimeEvent`、payload、connection/session/sequence、
  message ID 和 tool call ID。
- `client.rs` 只接收协议回调并调用唯一事件转换器，不在 normalizer 再解析
  `SessionNotification`。
- 覆盖稳定 v1 的 message、thought、tool、plan、commands、config、session info、
  usage 和 stop reason update。
- 不支持的内容块写入受限 transcript，并输出可理解的降级占位。
- `output.rs` 建立协议、产品事件、诊断三通道；产品事件使用有界 channel。
- 实现按 Session 路由、连接内 sequence 保序、message ID 聚合和 tool call upsert。
- 为无 message ID 的 Agent 实现仅限当前 turn 的兼容聚合器。
- 实现单块、累计消息、tool result、stderr 和 transcript 大小限制及 truncation
  metadata。
- normalizer 仅负责 `AcpRuntimeEvent -> ConversationPatch`。
- Run 收尾按 producer close、projector drain、flush、ack、exit result 的顺序执行。

主要文件：

- `crates/executors/src/executors/acp/events.rs`
- `crates/executors/src/executors/acp/output.rs`
- `crates/executors/src/executors/acp/client.rs`
- `crates/executors/src/executors/acp/normalize_logs.rs`
- `crates/executors/src/executors/acp/runtime.rs`
- `crates/executors/src/executors/acp/tests.rs`

退出条件：

- `client.rs` 和 `normalize_logs.rs` 不再存在两套 Session update 映射。
- ACP 产品事件不使用 unbounded channel，不依赖固定 flush sleep。
- prompt response 前后边界处的最后一条 update 不丢失。
- 原始 stdout 不进入用户消息或普通 run log。
- usage 只来自稳定 usage update，厂商 `_meta.quota` 不进入通用核心。

### M4：实现 MCP effective config 与 Session 注入

目标：把 MCP allowlist 固化为 OpenTeams 强制执行的安全边界。

任务：

- 从现有 `McpConfig`/Team MCP 设置中提取 canonical server definitions，不经过
  Gemini/Qwen/Codex 厂商格式转换。
- 定义 `EffectiveAcpMcpConfig`，合并 runtime/team definitions、成员 allowlist、
  profile 禁用项和 Session 安全限制。
- 实现 stdio、HTTP、SSE 到 ACP v1 类型的转换。
- 校验名称唯一性、command、args、env、URL、headers 和允许的 `_meta`。
- 根据 initialize 返回的 MCP capabilities 门控 HTTP/SSE；显式允许但无效或不支持的
  Server 在 Session 请求前失败，不能静默丢弃。
- new、load、resume 每次重新解析，并显式携带完整列表；空列表也必须发送。
- 日志只记录 Server name、transport、配置哈希和结果；不记录 command args、env
  value、headers 或 secret。
- 成员级 MCP allowlist 的合并在 `member_execution.rs` 完成，ACP 核心只接收已经确定
  的运行上下文。

主要文件：

- `crates/executors/src/executors/acp/mcp.rs`
- `crates/executors/src/mcp_config.rs`
- `crates/services/src/services/member_execution.rs`
- `crates/executors/src/executors/acp/session.rs`
- `crates/executors/src/executors/acp/tests.rs`

退出条件：

- stdio/HTTP/SSE、allowlist、禁用、空列表、重复名称和非法配置测试通过。
- MCP 撤销后下一次 resume/load 收到的新列表不再包含被撤销 Server。
- secret 不出现在 tracing、错误文本、transcript 或测试快照中。
- 同一个 Server 不会由 canonical 注入和厂商配置 fallback 重复连接。

### M5：实现 Client 审批、FS、Terminal 与安全

目标：完整实现 OpenTeams 作为 ACP Client 的受控服务。

任务：

#### 审批

- 将策略显式建模为 `ask | auto_allow | auto_reject`。
- `auto_allow` 只选择 `allow_always`，其次 `allow_once`；无允许项时 Cancelled。
- `auto_reject` 只选择 `reject_always`，其次 `reject_once`；无拒绝项时 Cancelled。
- `ask` 调用现有 `ExecutorApprovalService`；拒绝理由不在 client 内隐式创建新 Prompt。
- 记录 Session ID、tool call ID、策略和 option kind，不记录敏感参数。

#### 文件系统

- 创建共享 workspace root guard，只允许主 cwd 和已授权 additional directories。
- 拒绝 `..`、root 外绝对路径、symlink 逃逸和不存在父目录逃逸。
- 设置单次读写和返回内容上限。
- 协议错误不泄露宿主机敏感绝对路径。

#### Terminal

- Terminal 绑定 Session ID 和 connection ID，并复用同一 root guard。
- 使用进程组；create、output、wait、kill、release 和 connection drop 均可安全清理。
- 设置并发数、ring buffer 字节数、空闲和结束超时。
- wait、kill、release 幂等。
- 环境以 `ExecutionEnv` 为基础并过滤 OpenTeams 内部 secret。

主要文件：

- `crates/executors/src/executors/acp/client.rs`
- `crates/executors/src/executors/acp/config.rs`
- `crates/executors/src/executors/acp/runtime.rs`
- `crates/executors/src/executors/acp/tests.rs`

退出条件：

- 所有 permission option 组合均有确定结果，不存在任意第一项回退。
- FS 的相对路径、绝对路径、`..` 和 symlink 测试通过。
- Terminal 并发、缓冲、取消、连接断开和幂等测试通过。
- cancel 后 ACP Agent 和所有 Terminal 后代进程均退出。

### M6：接入隐藏 ACP QA runner

目标：在不迁移 Gemini/Qwen、也不暴露第三方入口的情况下，验证通用核心与 OpenTeams
后端契约可以端到端工作。

任务：

- 不在生产 `CodingAgent`/`BaseCodingAgent` 中增加用户可见的 `ACP` wire value。
- 增加 `test`/`qa-mode` 专用 `AcpQaExecutor`；如需经过真实后端路由，只增加
  `qa-mode` 下可见的 `ACP_QA` variant。
- 为 QA runner 实现 `StandardCodingAgentExecutor`，保持 `spawn`、
  `spawn_follow_up`、`SpawnedChild` 和 `ExecutorExitResult` 外部契约。
- command、Session 偏好、审批、FS/Terminal 和 MCP policy 由 fake Agent fixture 或
  QA 配置注入，不进入生产配置存储。
- 使用 QA runner 验证 Free Chat 的 new/follow-up/审批/MCP/取消，以及 Workflow 的
  worker/reviewer/retry/resume/interrupt。
- 验证 Session ID、事件、token metadata、changed files 和退出结果仍进入现有持久化
  与投影路径。
- 不增加 ACP 默认 profile、Agent Runtime 条目、`acp.json` schema、共享公共类型、
  前端 runner label、配置表单或 i18n 文案。

主要文件：

- `crates/executors/src/executors/acp/qa.rs`
- `crates/executors/src/executors/acp/mod.rs`
- `crates/executors/src/executors/acp/tests.rs`
- `crates/executors/src/executors/mod.rs`（仅 `qa-mode` variant）
- Free Chat/Workflow 的 QA 集成测试

退出条件：

- QA runner 在 `test`/`qa-mode` 下完成 Free Chat 和 Workflow 核心场景。
- 普通生产 build、Agent Runtime API、默认 profiles、公共 wire types 和前端 UI 中
  均不存在 Generic ACP 入口。
- QA runner 不调用旧 harness 或 `SessionManager`。
- `cargo test -p executors acp`、后端检查和目标 QA 集成测试通过。

### M7：迁移 Gemini

目标：让 Gemini 只保留启动描述和具名兼容项。

任务：

- 将 Gemini 的启动命令、环境和 availability 保留在 adapter，协议行为交给通用核心。
- `yolo: true/false` 反序列化兼容映射为 `auto_allow/ask`；新 schema/UI 不再新增
  `yolo` 配置。
- model、thought level、mode 优先使用 Session config options。
- token 使用稳定 usage update；不得在通用核心写死 Gemini/Google metadata。
- Follow-up 优先原生 resume/load；若受支持版本仍有明确协议偏差，只能在 Gemini
  adapter 中加入具名、可测试、有删除条件的兼容项。
- 验证 Gemini 不会同时从全局配置和 Session 参数重复加载 MCP Server。

退出条件：

- Gemini 的 Free Chat、Workflow、审批、自动允许、取消、MCP 和 Follow-up 回归通过。
- 新建 Gemini Session 不再写入本地历史 JSONL。
- Gemini adapter 不复制通用 runtime、事件或 Session 实现。
- 未满足删除条件的兼容项已记录具体 Agent 版本、行为和后续删除门槛。

### M8：迁移 Qwen

目标：Qwen 独立使用通用核心，不再依赖 Gemini。

任务：

- Qwen 只保留启动命令、环境、availability 和自身具名兼容项。
- 直接读取 Qwen initialize capabilities，不能复用 Gemini 判断。
- 迁移 model、thought level、approval、MCP 和原生 Follow-up。
- 验证旧路径中的 oversized resume Prompt 不再由通用核心生成。
- 删除 Qwen 对 `gemini::AcpAgentHarness` 的 re-export 依赖。

退出条件：

- Qwen 的 Free Chat、Workflow、审批、取消、MCP、Follow-up 和 oversized prompt 回归
  通过。
- Qwen 不再读取 `qwen_sessions` 本地历史作为恢复真源。
- Qwen 和 Gemini adapter 之间没有实现依赖。

### M9：删除旧路径并完成全量验收

目标：消除临时双路径和历史补丁。

删除项：

- `acp/harness.rs` 中被 `runtime.rs` 替代的实现。
- 旧 `SessionManager`、本地 JSONL fork 和 Resume Prompt 拼接。
- `max_resume_prompt_bytes`。
- `with_model`、`with_mode` 和 `session/set_model` ACP 通用路径。
- Gemini/Qwen 通过 CLI 参数或临时设置文件注入协议配置的代码。
- Gemini/Google token metadata 硬编码。
- permission 任意 option 回退。
- 重复 Session update 转换、无界 ACP event channel和固定 flush sleep。

保留项：

- 旧 profile 的反序列化兼容。
- `StandardCodingAgentExecutor`、`SpawnedChild`、审批服务和外围 run 记录契约。
- Chat/Workflow 的 Agent Session ID 持久化。

退出条件：

- 设计文档第 19 节全部验收项有自动化测试或明确的真实 Agent 验证记录。
- 不存在新旧 ACP 核心永久并行。
- 全量检查通过，且代码搜索确认旧符号和厂商硬编码已清理。

## 5. 建议 PR 切分

| PR | 内容 | 合并前门禁 |
| --- | --- | --- |
| 01 | M0 基线测试与 fixture | 现有行为测试稳定 |
| 02 | SDK 1.0、模块骨架、fake Agent | executors 测试、backend check |
| 03 | initialize、auth、Session、config options、错误 | 协议生命周期测试 |
| 04 | typed events、有界输出、drain barrier | 尾部事件/背压/脱敏测试 |
| 05 | canonical MCP、allowlist、transport gating | MCP 与 secret 测试 |
| 06 | approval policy、FS root guard | permission 与路径安全测试 |
| 07 | Terminal 生命周期和取消清理 | 进程组、幂等、缓冲测试 |
| 08 | 隐藏 `AcpQaExecutor` 与 Free Chat/Workflow QA 接入 | 后端检查、QA 集成测试 |
| 09 | Gemini adapter 迁移 | Gemini 回归和真实 Agent conformance |
| 10 | Qwen adapter 迁移 | Qwen 回归和真实 Agent conformance |
| 11 | 删除旧路径、文档与全量验收 | check、lint、类型、E2E 全部通过 |

每个 PR 只合并一个可验证边界。PR 02 至 PR 08 期间允许旧 Gemini/Qwen runner 暂时保留，
但通用 ACP 核心不能调用旧 harness 或 SessionManager；PR 11 必须结束双路径。

## 6. 测试与质量门禁

### 每个后端 PR

```bash
cargo fmt --all -- --check
cargo test -p executors --features qa-mode
cargo test -p executors --features qa-mode --test acp_qa
cargo test -p services --features qa-mode --test acp_backend_qa
pnpm run backend:check
```

### 迁移 Gemini/Qwen 并修改共享类型或 Agent Runtime 时

```bash
pnpm run generate-types
pnpm run generate-types:check
```

### 迁移 Gemini/Qwen 和最终清理时

```bash
pnpm run check
pnpm run lint
pnpm run format:check
```

另需运行目标 E2E：

- ACP QA runner：在 `qa-mode` 下覆盖 Free Chat new/follow-up、Workflow
  worker/reviewer、审批、MCP、取消。
- Gemini：上述场景及原生 Session 恢复。
- Qwen：上述场景及 oversized prompt 迁移。
- MCP：allowlist、撤销、空列表、HTTP/SSE 能力门控、不重复连接。

真实 Agent conformance 不进入默认 CI，但 Gemini/Qwen adapter PR 合并前必须记录所测
CLI 版本、capabilities 摘要和结果；不得记录账号、token、Prompt 或 MCP secret。

## 7. 验收追踪

| 设计验收域 | 落地阶段 | 主要证据 |
| --- | --- | --- |
| stable 1.0、initialize、能力快照 | M1-M2 | dependency/handshake tests |
| config options | M2 | dynamic option tests |
| 原生 Follow-up | M2、M7、M8 | resume/load branch tests |
| 唯一事件映射、稳定 updates | M3 | event matrix tests |
| 三通道、有界队列、drain | M3 | saturation/tail update tests |
| MCP 完整列表、撤销、脱敏 | M4 | new/load/resume MCP tests |
| 审批确定性 | M5 | option combination tests |
| FS/Terminal 访问策略 | M5 | workspace/full-access path/process lifecycle tests |
| 隐藏 ACP QA 集成 | M6 | `qa-mode` Free Chat/Workflow tests |
| 无厂商核心硬编码 | M7-M9 | code search + review |
| Gemini/Qwen 无回归 | M7-M8 | regression + conformance |
| 旧路径删除 | M9 | deletion checklist |

## 8. 发布与回退策略

- M1-M5 不改变默认 runner 路由；出现问题时回退对应 PR，不影响 Gemini/Qwen 生产路径。
- M6 只在 test/`qa-mode` 增加隐藏 runner，不改变生产 runner 列表、API 或 UI。
- M7、M8 分别切换 Gemini、Qwen，每次只迁移一个 Agent。adapter PR 出现回归时回退该
  PR，不回退已经验证的通用核心。
- 不增加永久的“legacy/new”用户开关。迁移期兼容只能存在于具名 adapter，并在 M9
  按删除门槛清理。
- M9 删除前必须保存 M0 新旧对比结果、fake Agent 全套结果和真实 Agent conformance
  记录。

## 9. 主要风险与控制

| 风险 | 控制措施 |
| --- | --- |
| SDK 1.0 API 变化同时破坏旧 runner | M0 先固化行为；M1 只做协议/编译迁移 |
| Agent 能力声明与实际实现不一致 | 记录方法和错误码；兼容项仅放厂商 adapter |
| Session ID 或历史丢失 | opaque 传递；迁移前验证现有 ID 能否原生 resume/load |
| 背压造成协议回调死锁 | 控制请求与产品事件分通道；队列饱和集成测试 |
| 最后一条 update 丢失 | drain barrier 和 prompt 边界测试 |
| FS symlink/父目录边界不确定 | workspace 模式使用共享 root guard、canonical path 测试和写前复验；full-access 模式显式放行 |
| Terminal 后代进程残留 | 进程组、connection drop 清理、超时强杀测试 |
| MCP secret 泄漏 | allowlist 脱敏、日志快照扫描、错误文本测试 |
| QA runner 意外出现在生产环境 | 使用编译条件隔离，并增加生产 runner 列表断言 |
| Gemini/Qwen 公共类型漂移 | 公共类型只由生成器产生，CI 使用 check 命令 |

## 10. 完成定义

只有同时满足以下条件，实施任务才可关闭：

1. 隐藏 ACP QA runner 在 test/`qa-mode` 下完成 Free Chat 和 Workflow 核心场景，且
   生产环境不存在 Generic ACP 入口。
2. fake Agent 覆盖协议版本、能力、Session、配置、MCP、审批、FS、Terminal、事件、
   错误、取消、背压和 drain。
3. Gemini/Qwen 真实 Agent conformance 和回归通过。
4. Gemini/Qwen 公共类型如有变化，`shared/types.ts` 和对应 schema 由生成器更新并
   通过检查；不生成 `shared/schemas/acp.json`。
5. 旧 SessionManager、历史 Prompt 注入、重复事件转换、无界队列、固定 sleep 和厂商
   核心硬编码已删除。
6. `check`、`lint`、`format:check`、类型生成检查和目标 E2E 全部通过。
7. 结构化日志与 transcript 的脱敏审计通过。
8. 设计文档、实现计划和最终代码行为一致。
