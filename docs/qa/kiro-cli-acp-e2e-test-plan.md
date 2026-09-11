# Kiro CLI ACP 端到端测试方案

> 状态：测试设计完成，自动化尚待按本方案补齐。确定性发布门禁使用仓库内 fake Kiro；真实 Kiro CLI 冒烟仅在隔离测试账号和用户授权的环境中执行。

## 1. 测试目标

验证一名配置为 `KIRO_CLI` 的成员，从 Agent Runtime 发现、认证、配置、聊天和工作流执行，到 ACP 子进程、MCP、权限、持久化、错误恢复和清理的完整产品链路。测试不能只断言按钮存在、HTTP 200 或进程退出码；每个用例至少同时断言以下三层中的两层，P0 用例必须覆盖三层：

1. 前端可见状态：运行时、成员配置、消息、审批、运行记录、Diff 或工作流卡片。
2. 服务端持久状态：runner、model、run、external session ID、终态、runtime snapshot/revision 或数据库记录。
3. 执行器事实：实际命令、ACP JSON-RPC、cwd、MCP 列表、事件序列、stderr 和进程退出。

## 2. Kiro 适配契约

以下契约是测试预期的真源；Kiro 未声明的能力必须按“不支持”验证，不能套用其他 ACP runner 的预期。

| 能力 | Kiro 预期 |
| --- | --- |
| runner | `KIRO_CLI`，原生命令，不依赖 Node/npm/npx |
| 诊断命令 | `kiro-cli --version`；`kiro-cli whoami --format json` |
| 运行命令 | `kiro-cli acp` |
| 认证 | 非空 `KIRO_API_KEY`，或本地 `kiro-cli login`；不得把未认证误报为安装/模型故障 |
| ACP | 协议 v1；支持 `session/new`、`session/load`、`session/prompt`、`session/cancel`；不使用 `session/resume` |
| 模型 | 来自 ACP probe 的 model config option；无静态 fallback |
| ACP auth method | 不支持；UI 不显示 method ID，后端拒绝伪造配置 |
| additional directories | 不支持；UI 隐藏，后端拒绝非空配置 |
| 权限 | `ask`、`auto_allow`、`auto_reject`；`workspace_only`、`full_access` |
| MCP | 每次运行冻结 member-scoped ACP MCP 快照；不导入 ambient `.kiro` MCP |
| follow-up | 复用 external session ID 并调用 `session/load`；load 被拒绝视为无效 session，而非静默拼接历史 |
| 安全参数 | 拒绝 `--agent`、`--trust-all-tools`、`--mode`、`session/set_mode` |
| 能力展示 | Kiro 不声明 token/context 静态能力时显示“不可用”，不得复用其他 runner 缓存 |

## 3. 测试环境与证据

- 使用唯一 `RUN_ID=KIRO-<UTC timestamp>-<short-sha>`。
- 使用临时 SQLite、临时 HOME、临时 Git 仓库 `REG_KIRO_REPO`、仓库外临时目录 `REG_KIRO_OUTSIDE`，不得读取用户真实 `~/.kiro`。
- 将仓库内 `fake_kiro_acp.mjs` 安装为临时 `bin/kiro-cli`；fixture 必须支持版本、两类认证、能力 probe、权限请求、图片、错误、挂起、异常退出和协议日志。
- 所有等待都设置超时：UI/API 10 秒、单轮 Agent 60 秒、工作流 180 秒；取消测试禁止依靠固定 sleep，应等待 protocol log 中出现 prompt。
- 证据写入 `qa_test/kiro-cli/<RUN_ID>/`，每项保存结构化 JSON/JSONL、必要截图、Git 状态和脱敏日志。
- sentinel secrets 覆盖 1、2、3 字符以及普通长度；API key、MCP env/header secret、账号邮箱均不得出现在 UI、API、raw transcript、stderr、tracing、协议日志和报告。

## 4. 自动化分层

| 层 | 建议入口 | 责任 |
| --- | --- | --- |
| Executor fixture | `crates/executors/tests/kiro_acp_fixture.rs` | 真进程 ACP 协议、命令、事件、权限、取消、错误和脱敏 |
| Server/service E2E | 新增 `crates/services/tests/kiro_cli_e2e.rs`，复用临时 DB + fake executor | Agent Runtime、成员配置、chat delivery、工作流、持久化、重启恢复 |
| Frontend acceptance | 新增 `frontend/src/pages/agent-runtime/kiroCliRuntime.acceptance.test.tsx` | 安装/认证状态、capability gates、成员选择、审批与终态投影 |
| MCP cross-layer | `crates/server/tests/member_scoped_mcp_e2e.rs` | member-scoped MCP 的隔离、冻结、迁移、失败和清理 |
| Real CLI smoke | UI + 真实 `kiro-cli`，独立测试账号 | 验证 fake 与当前生产 CLI 的最小兼容边界 |

## 5. 端到端用例矩阵

### 5.1 发现、认证与配置

| ID | 优先级 | 场景与步骤 | 必须断言 |
| --- | --- | --- | --- |
| KIRO-E2E-001 | P0 | PATH 中无 `kiro-cli`，打开 Agent Runtime 并 Refresh。 | Kiro 卡片仍可渲染；状态为未安装；展示官方 macOS/Linux、Windows 安装命令和文档；不显示 Node 安装步骤；其他 runner 不受影响。 |
| KIRO-E2E-002 | P0 | 注入 fake binary，执行 Refresh 和 diagnostics。 | `installed/executable/version` 正确；版本命令只有 `--version`；runtime command 为 `kiro-cli acp`；resolved cwd 不是服务端 cwd；品牌、logo、schema 和 `Kiro CLI` 标签正确。 |
| KIRO-E2E-003 | P0 | 清空 API key 和本地登录，先 diagnostics，再发消息。 | `whoami --format json` 被调用且不进入 initialize；UI 为 `unauthenticated`，不出现 ACP/model discovery error；发送返回结构化 `AuthRequired`，提示 `kiro-cli login` 或 `KIRO_API_KEY`。 |
| KIRO-E2E-004 | P0 | 分别用非空 `KIRO_API_KEY`、嵌套 `account` JSON、顶层 `accountType+email` JSON 认证。再测试空 key、false 标志、残缺 JSON 和 timeout。 | 三种有效认证均可 probe；无效响应保持 unauthenticated；超时有界且可重试；UI/API/日志不回显 key 或邮箱。 |
| KIRO-E2E-005 | P0 | 认证后 probe ACP，并保存模型与权限配置；分别测试 `model` 字段与 model category config override；刷新页面、重启服务后重新打开。 | 协议 v1、agent name/version、`supports_session_load=true`、`supports_session_resume=false`；模型来自 probe；config override 存在时不会重复发送 model 设置；无 auth methods/additional directories 控件；配置持久化且只属于 Kiro。 |
| KIRO-E2E-006 | P0 | 保存 append prompt、Kiro env 和一个安全 future 参数；再通过 UI/API 注入 method ID、非空 additional directories 及受限运行参数。 | append prompt 恰好组合一次，普通 env 到达 Kiro、秘密 env 仅以环境变量传递且 UI 脱敏；不支持配置返回 Configuration/CommandBuild 错误；受限参数即使嵌入 base override 也被拒绝；诊断命令不继承 ACP additional params；安全参数只进入 `acp` 命令。 |
| KIRO-E2E-007 | P1 | 在 Team 页面用 `KIRO_CLI`、`kiro-cli`、`kiro_cli` 创建/编辑成员，选择 probe 模型和 ACP 权限。 | 三种输入规范化为 `KIRO_CLI`；只有 available runtime 可选；成员 `execution_config` 保存 runner/model/acp；Permissions tab 可见，受 capability gate 限制；刷新后不丢失。 |

### 5.2 聊天、会话与内容投影

| ID | 优先级 | 场景与步骤 | 必须断言 |
| --- | --- | --- | --- |
| KIRO-E2E-008 | P0 | 给空闲 Kiro 成员发送首次文本消息，fake 依次产生 user、thought、message、tool call、tool update、done，并夹带 `_kiro.dev/session/update`。 | delivery 从 starting→running→completed；只启动一次 `kiro-cli acp`；`session/new` cwd 为成员工作区；标准事件按序投影；私有通知不成为 `Other`/用户消息；run 记录为 `KIRO_CLI`、正确 model 和 external session ID。 |
| KIRO-E2E-009 | P0 | 同一成员发送包含 NONCE 的 follow-up。 | 使用同一 external session ID 和 `session/load`；从未发送 `session/resume`；第二次 MCP 快照随新运行重新装载；NONCE 可恢复；不把完整历史拼进 prompt。 |
| KIRO-E2E-010 | P0 | 令 `session/load` 返回拒绝/unknown session，再发送后续消息。 | 当前 follow-up 以明确无效 session/不支持错误收敛，不伪装成功、不永久 spinner；旧 run/transcript 保留；用户重新发起新会话可成功，不串用旧 session ID。 |
| KIRO-E2E-011 | P1 | 发送包含文本和图片的结构化 prompt。 | `session/prompt` 使用 `prompt` 字段且保留 text/image content block、MIME 和数据；附件 UI、消息持久化和响应正常；不退化为 `[image]` 字符串。 |
| KIRO-E2E-012 | P0 | 请求只创建 `kiro-<RUN_ID>.txt`，完成后检查 activity、Diff、Git；刷新会话。 | 文件只在解析后的成员工作区，内容正确；`.openteams/` 不出现在源码变更；run、消息、Diff、runner/model/session ID 刷新后保持。 |
| KIRO-E2E-013 | P0 | 运行 A 时连续排入 B/C，删除 B，完成 A/C；再模拟页面刷新和 WS 重连。 | 每个 delivery 唯一且 FIFO；删除只作用于 B；Kiro 每次最多一个 in-flight；snapshot revision 单调，旧 delta 不回滚状态；不得重复启动 ACP 进程或最终消息。 |
| KIRO-E2E-014 | P0 | 让 prompt 挂起，待协议日志确认后点击 Stop；随后发送最小消息。 | 发出无 request id 的 `session/cancel`；运行进入 cancelled/合法停止终态；尾部事件完成 drain；队列可继续；无失联或僵尸进程。 |

### 5.3 权限与文件系统边界

| ID | 优先级 | 场景与步骤 | 必须断言 |
| --- | --- | --- | --- |
| KIRO-E2E-015 | P0 | `workspace_only + ask` 下分别请求工作区内读写、`..`、绝对路径和 symlink escape；对合法请求先拒绝再允许。 | 待审批前无副作用；拒绝/允许只绑定当前 request；工作区内允许后成功；三种越界路径始终拒绝；审计记录不含 secret。 |
| KIRO-E2E-016 | P0 | 切换 `auto_allow` 和 `auto_reject`，分别触发 native terminal/file 与 MCP tool 权限。 | auto_allow 不弹 UI且绝不回退 reject；auto_reject 无副作用；native/MCP 共用相同审批语义；模式改变仅影响目标成员/runner，刷新后保持。 |
| KIRO-E2E-017 | P1 | 经危险操作确认切换 `full_access`，只访问 `REG_KIRO_OUTSIDE` 中的 sentinel；再恢复 workspace_only。 | run 记录 `full_access=true`；仅显式测试目标成功；恢复后相同访问被拒绝；Kiro UI 仍不提供 additional directories，API 注入该字段仍失败。 |

### 5.4 MCP 隔离与安全

| ID | 优先级 | 场景与步骤 | 必须断言 |
| --- | --- | --- | --- |
| KIRO-E2E-018 | P0 | 配置两个 member MCP（stdio + HTTP/header），分别测试默认全选、显式 allowlist、显式空 allowlist和无配置。 | `session/new/load` 只携带该成员当次冻结列表；空 allowlist 明确传空列表；ambient `.kiro/settings/mcp.json` 不被导入或改写；server 名称、transport 和工具结果正确。 |
| KIRO-E2E-019 | P0 | A/B 两名 Kiro 成员并发运行不同 MCP 配置；运行 A 后修改 registry，再执行 follow-up。 | A/B snapshot/hash、工具和秘密互不串扰；在途 A 使用冻结旧快照，下一次运行才见新配置；失败或取消后各自 cleanup；vendor 配置字节稳定。 |
| KIRO-E2E-020 | P0 | 在 API key、MCP env/header 中注入 1/2/3 字符和普通长度 sentinel，覆盖成功、prompt error、session error、probe error、取消。 | 标准事件、stderr（含跨 chunk）、tracing、raw transcript、协议日志、HTTP 错误、DB 和报告均无明文；需要说明时只出现 `[redacted]`；清理后的 `.openteams/tmp` 无快照残留。 |

### 5.5 错误、恢复与产品集成

| ID | 优先级 | 场景与步骤 | 必须断言 |
| --- | --- | --- | --- |
| KIRO-E2E-021 | P0 | 依次模拟 executable missing、probe protocol error、session/new error、prompt error、空 end turn、异常退出和卡死。 | 错误分别归类为 not installed/auth/configuration/run failure/abnormal exit/timeout；每次都有明确终态和 failure reason；不写伪成功消息，不遗留 lease、delivery 或 spinner；修复 fixture 状态后可再次运行。 |
| KIRO-E2E-022 | P0 | 在 delivery claim、run bind 和 finalize 边界终止服务，再重启并执行恢复。 | 每个 delivery 恰好恢复或终结一次；无重复 run/final message/ACP 进程；external session ID 和 revision 一致；成员最终可再次调度。 |
| KIRO-E2E-023 | P0 | 将 Kiro 成员放入两步 workflow：A 写文件，B 依赖 A 并读取；包含用户审批、用户 review、Stop/Retry。 | compiler 接受 `KIRO_CLI`；A 完成前 B 不运行；审批/review/Retry 状态合法；两步 run 都记录 Kiro runner/model/workspace/session；最终完成只由用户验收触发。 |
| KIRO-E2E-024 | P1 | Kiro 与另一 ACP runner 各创建成员、会话和工作流，使用相同模型显示名、不同 NONCE/MCP。 | runtime probe cache、配置、external session、日志、MCP、队列和错误完全隔离；Kiro 未提供的 token/context 显示不可用，不沿用另一 runner 数据。 |
| KIRO-E2E-025 | P1 | 正常完成、取消、错误和服务重启四条路径后检查进程与临时目录。 | `kiro-cli` 及其 terminal 子进程无残留；kill/release 幂等；MCP snapshot cleanup 执行一次且最终为空；用户工作区文件不被 cleanup 删除。 |

### 5.6 真实 Kiro CLI 兼容冒烟

真实测试不得使用个人账号或个人工作区；缺少测试账号/网络/安装授权时标记 `BLOCKED`，不能把 fake 通过等同于真实兼容通过。

| ID | 优先级 | 场景与步骤 | 必须断言 |
| --- | --- | --- | --- |
| KIRO-REAL-001 | P1 | 按官方脚本安装，记录 `kiro-cli --version`；先登出验证 unauthenticated，再用测试账号 `kiro-cli login` 或测试 API key 认证并 Refresh。 | 安装/认证状态和 diagnostics 与真实 CLI 一致；未认证不是红色 probe 故障；凭据与账号标识脱敏。 |
| KIRO-REAL-002 | P1 | 在隔离 Git 仓库执行首次只读消息、NONCE follow-up、单文件写入和 Stop。 | 首次运行、`session/load` 续聊、Diff、取消和恢复使用均通过；命令固定为 `kiro-cli acp`；无静默 fallback。 |
| KIRO-REAL-003 | P1 | 配置一个无秘密本地 MCP echo server，执行一次工具调用，再测试 ask 拒绝/允许。 | Kiro 收到当次 member MCP；工具结果正确；审批有真实副作用边界；关闭配置后下一轮不可见。 |

## 6. 覆盖与缺口判定

现有自动化已经覆盖 KIRO-E2E-004 的主要认证分支、008/009/014 的 executor 生命周期、018–020 的大部分 MCP/脱敏，以及 003/005/007 的部分前端静态行为。以下能力仍不能由现有测试证明，必须新增跨层用例后才能宣称“完整覆盖”：

- Agent Runtime API 到 UI 的未认证状态、刷新与持久化闭环；
- Team 选择 Kiro 后真实创建 chat delivery/workflow run 的闭环；
- 三种审批策略、workspace/full access 和路径逃逸；
- 图片 prompt、queue/WS 重连、服务重启恢复；
- stale `session/load`、异常退出和进程树清理；
- 真实 Kiro CLI 的最小兼容冒烟。

## 7. 发布门禁与执行顺序

建议统一脚本 `scripts/run-kiro-cli-acceptance.sh` 按以下顺序执行，并为每个 case 独立保存日志：

```bash
cargo test -p executors --features qa-mode --test kiro_acp_fixture -- --nocapture
cargo test -p services --features qa-mode --test kiro_cli_e2e -- --nocapture
cargo test -p server --features qa-mode --test member_scoped_mcp_e2e -- --nocapture
pnpm -C frontend exec tsx src/pages/agent-runtime/kiroCliRuntime.acceptance.test.tsx
pnpm run frontend:check
```

判定规则：

- KIRO-E2E-001 至 025 全部 `PASS` 才能通过确定性 Kiro 发布门禁；P0 不允许 `SKIPPED`。
- KIRO-REAL-001 至 003 是生产兼容验收；正式宣称支持当前 Kiro 版本前必须全部 `PASS`。
- 任一 secret 泄漏、工作区越界、跨成员/跨 runner 串扰、重复执行或无法停止均为 S1/S2，整体直接 `FAIL`。
- 每项报告必须记录预期、实际、退出码/终态、证据路径和首次失败；重跑通过不能覆盖首次失败事实。
