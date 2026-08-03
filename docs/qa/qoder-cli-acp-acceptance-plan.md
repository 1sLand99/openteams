# Qoder CLI 真实 ACP 验收方案

> **状态**：待用户授权安装与执行。任何 PAT 必须仅通过环境变量 `QODER_PERSONAL_ACCESS_TOKEN` 注入并全程脱敏。

## 0. 前置与安全红线

| 约束 | 说明 |
|---|---|
| PAT 注入 | 仅通过环境变量 `QODER_PERSONAL_ACCESS_TOKEN` 注入；禁止写入配置文件、命令行参数或日志。 |
| PAT 脱敏 | 全程脱敏：诊断日志、raw transcript、终端环境、工具终端输出中不得出现明文 token。 |
| 配置隔离 | 使用 `QODER_CONFIG_DIR` 指向隔离测试目录，避免污染用户 `~/.qoder`。 |
| 工作区 | 固定为 `REG_CLI_REPO`（独立测试仓库），不得使用进程 cwd。 |
| 安全标志 | `--yolo`、`--dangerously-skip-permissions` 等冲突标志必须被 `CommandBuildError` 拒绝。 |
| MCP 严格模式 | 命令固定为 `--strict-mcp-config`，环境 MCP 配置不得绕过 allowlist。 |

## 1. 安装与版本 (INSTALL-01)

**前置**：用户已授权安装。

| 步骤 | 操作 | 预期 |
|---|---|---|
| 1.1 | macOS 执行 `curl -fsSL https://qoder.com/install \| bash` | 安装成功，`qodercli` 出现在 PATH |
| 1.2 | `qodercli --version` | 输出版本号，退出码 0 |
| 1.3 | Agent Runtime 页面选择 `QODER_CLI`，执行 Refresh | `installed=true`，`executable` 解析到 `qodercli`，command source 非 cwd |
| 1.4 | 记录 version、executable path、command source | 诊断矩阵存档 |

## 2. 未认证错误 (AUTH-01)

**前置**：Qoder 已安装但未设置任何凭据（隔离 `QODER_CONFIG_DIR` 为空目录）。

| 步骤 | 操作 | 预期 |
|---|---|---|
| 2.1 | 不设置 `QODER_PERSONAL_ACCESS_TOKEN`，不在 `QODER_CONFIG_DIR` 放置 `credentials.json`/`oauth_creds.json`/`auth.json` | `is_authenticated` 返回 false |
| 2.2 | Agent Runtime Refresh | `auth_state=unauthenticated`，显示安装/认证指引（`curl -fsSL https://qoder.com/install \| bash` 和 `authEnvVars: ["QODER_PERSONAL_ACCESS_TOKEN"]`） |
| 2.3 | 尝试发送消息 | 返回明确的未认证类型错误，不静默成功、不 panic |

## 3. initialize 握手 (INIT-01)

**前置**：PAT 通过环境变量注入，`QODER_CONFIG_DIR` 隔离。

| 步骤 | 操作 | 预期 |
|---|---|---|
| 3.1 | ACP probe（`probe_acp`） | `initialize` 请求发送 `ProtocolVersion::V1`，响应协议版本验证为 v1 |
| 3.2 | 记录 agent info | agent name 为 Qoder，记录版本和 agent info |
| 3.3 | 记录 auth methods | probe 返回 `auth_methods` 列表（如有） |
| 3.4 | 记录 capabilities | `session_capabilities`（new/resume/load/list/close/delete）按 Agent 声明捕获 |
| 3.5 | 记录 config options | probe 返回 `configOptions`，其中 category=model 的选项包含五档模型 |
| 3.6 | probe 完成后清理 | 若创建了 probe session，probe 结束后 close/delete 该 session（如能力支持） |

## 4. 新建会话 (NEW-01)

| 步骤 | 操作 | 预期 |
|---|---|---|
| 4.1 | 创建自由聊天会话，runner=QODER_CLI，工作区=`REG_CLI_REPO` | `session/new` 发送 cwd=REG_CLI_REPO |
| 4.2 | 发送提示：`仅回复 RUNTIME=QODER_CLI; RUN_ID=<RUN_ID>; NONCE=QODER-FIRST，不修改文件。` | 流式消息到达，终态为 completed，实际 runner 为 QODER_CLI |
| 4.3 | 记录 external session ID | 运行记录包含 Qoder 返回的 sessionId |
| 4.4 | 核对命令参数 | 进程命令为 `qodercli --acp --permission-mode default --strict-mcp-config --allowed-mcp-server-names <allowlist>`，无冲突标志 |

## 5. 续聊 resume/load (RESUME-01)

| 步骤 | 操作 | 预期 |
|---|---|---|
| 5.1 | 同一成员发送：`不要读取磁盘，复述上一条消息中的 NONCE，并回复 FOLLOWUP-OK。` | 若 Agent 声明 `resume` 能力 → 使用 `session/resume`；否则若支持 `load` → 使用 `session/load`；否则返回 `FollowUpNotSupported` 类型错误 |
| 5.2 | 核对外部 session ID | 续聊使用同一 sessionId 或显式 resume/load 关联 |
| 5.3 | 核对 NONCE | Agent 正确复述 `QODER-FIRST` |
| 5.4 | 禁止历史拼接 | 不得通过拼接历史到 prompt 来伪造续聊 |

## 6. 三种审批策略 (APPROVAL-01/02/03)

**前置**：`workspace_only + ask` 为默认安全基线。

### 6.1 Ask 模式 (APPROVAL-01)

| 步骤 | 操作 | 预期 |
|---|---|---|
| 6.1.1 | 设置 `approval_mode=ask`，发送需要文件写入的提示 | Agent 发出 `request_permission`，OpenTeams 弹出审批 UI |
| 6.1.2 | 选择"允许一次" | 权限请求被允许，工具执行成功 |
| 6.1.3 | 再发送需写入提示 | 再次弹出审批（ask 只作用于当前请求，不持久允许） |
| 6.1.4 | 选择"拒绝一次" | 权限被拒绝，Agent 收到 reject，运行以权限拒绝信号结束 |

### 6.2 AutoAllow 模式 (APPROVAL-02)

| 步骤 | 操作 | 预期 |
|---|---|---|
| 6.2.1 | 设置 `approval_mode=auto_allow` | 所有 `request_permission` 自动选择 `allow_always` 然后 `allow_once`，**永不回退到 reject** |
| 6.2.2 | 发送需写入提示 | 无审批 UI 弹出，工具直接执行 |

### 6.3 AutoReject 模式 (APPROVAL-03)

| 步骤 | 操作 | 预期 |
|---|---|---|
| 6.3.1 | 设置 `approval_mode=auto_reject` | 所有 `request_permission` 自动选择 `reject_always` 然后 `reject_once` |
| 6.3.2 | 发送需写入提示 | 权限被拒绝，运行以权限拒绝信号结束 |
| 6.3.3 | 恢复为 `ask` | 确认只影响目标 runner，其他 runner 不受影响 |

## 7. Workspace / Full Access 权限模式 (ACCESS-01/02)

### 7.1 Workspace Only (ACCESS-01)

| 步骤 | 操作 | 预期 |
|---|---|---|
| 7.1.1 | 设置 `access_mode=workspace_only` | `full_access=false` |
| 7.1.2 | Agent 请求读取工作区内文件 | `read_text_file` 成功 |
| 7.1.3 | Agent 请求写入工作区内文件 | `write_text_file` 成功 |
| 7.1.4 | Agent 请求读取工作区外文件（含 `..` 路径、绝对路径、symlink escape） | 请求被拒绝，返回 `invalid_params` 错误 |
| 7.1.5 | 记录 `executor_acp_full_access_enabled=false` | chat run 记录和 workflow session 记录一致 |

### 7.2 Full Access (ACCESS-02)

| 步骤 | 操作 | 预期 |
|---|---|---|
| 7.2.1 | 设置 `access_mode=full_access` | `full_access=true`，`AcpClientServicePolicy.full_access=true` |
| 7.2.2 | Agent 请求读取/写入工作区外文件（在 additional_directories 内） | 请求成功 |
| 7.2.3 | Agent 请求访问 additional_directories 外的路径 | 仍被拒绝 |
| 7.2.4 | 记录 `executor_acp_full_access_enabled=true` | run 记录一致 |
| 7.2.5 | additional_directories 验证 | 相对路径被拒绝，只有规范化后的绝对路径被接受 |

## 8. MCP 允许与隔离 (MCP-01/02)

### 8.1 MCP 允许 (MCP-01)

| 步骤 | 操作 | 预期 |
|---|---|---|
| 8.1.1 | 在 `~/.qoder/settings.json`（或 `QODER_CONFIG_DIR/settings.json`）配置一个无秘密测试 MCP stdio server | canonical ACP 格式被正确解析 |
| 8.1.2 | ACP policy 允许该 server | `--allowed-mcp-server-names` 包含该 server 名称 |
| 8.1.3 | session/new 携带完整 MCP server 列表 | Agent 收到 mcp_servers |
| 8.1.4 | Agent 调用 MCP 工具返回 `<runner-key>:<RUN_ID>` | 工具结果正确投影到消息流 |

### 8.2 MCP 隔离 (MCP-02)

| 步骤 | 操作 | 预期 |
|---|---|---|
| 8.2.1 | 配置两个 MCP server，policy 只允许其中一个 | `--allowed-mcp-server-names` 只含允许的名称 |
| 8.2.2 | `--strict-mcp-config` 生效 | 环境级/项目级 MCP 配置不被合并 |
| 8.2.3 | MCP env/header secret 脱敏 | 配置中的 token/secret 在日志中脱敏，config_hash 不含明文 |
| 8.2.4 | resume/load 时 MCP 列表重新解析 | 撤销的 server 在续聊中不再出现 |
| 8.2.5 | 空 allowlist | `--allowed-mcp-server-names ""` 为空，无 server 被加载 |

## 9. 五档模型 (MODEL-01)

| 步骤 | 操作 | 预期 |
|---|---|---|
| 9.1 | probe 返回 configOptions（category=model） | 包含 `lite`、`efficient`、`auto`、`performance`、`ultimate` 五档 |
| 9.2 | 逐档通过 `session/set_config_option` 设置模型 | 每次 Agent 响应确认 requested value 为 current |
| 9.3 | 设置 `lite`，发送最小只读消息 | 运行使用 lite 档，usage 记录对应模型 |
| 9.4 | 设置 `performance`，发送最小只读消息 | 运行使用 performance 档 |
| 9.5 | 设置 `ultimate`，发送最小只读消息 | 运行使用 ultimate 档 |
| 9.6 | 设置自定义模型 ID | `list_models` 返回值包含自定义 ID |
| 9.7 | config override 冲突 | 若 `config_overrides` 已包含 model category，则 `QoderCli.model` 不重复设置 |
| 9.8 | 模糊匹配拒绝 | 当多个 config option 匹配同一 model preference 时，返回 ambiguity 错误 |
| 9.9 | Agent 未确认值 | 若 `set_config_option` 响应中 current value ≠ requested value，返回类型错误 |

## 10. Usage 与 Token 追踪 (USAGE-01)

| 步骤 | 操作 | 预期 |
|---|---|---|
| 10.1 | 发送一条正常消息 | `UsageUpdate` 被捕获，`AcpTokenUsageAccumulator` 累积 token |
| 10.2 | 核对运行记录 token 信息 | `token_usage` 包含 input/output tokens，归属当前 run |
| 10.3 | 核对 context usage | 若 Agent 提供 context usage，正确归属当前 run；不提供时 UI 显示"不可用"而非伪造 |
| 10.4 | 续聊后核对 token | 新 run 的 token 独立，不沿用前次 run 缓存 |

## 11. 图片支持 (IMAGE-01)

| 步骤 | 操作 | 预期 |
|---|---|---|
| 11.1 | 发送附带图片的消息（结构化 prompt，content blocks 含 image） | `session/prompt` 请求中 image content block 被保留 |
| 11.2 | Agent 处理图片 | 流式响应正常，不因图片导致协议错误 |
| 11.3 | 核对消息投影 | 图片附件在 UI 消息中正确展示 |

## 12. 取消 (CANCEL-01)

| 步骤 | 操作 | 预期 |
|---|---|---|
| 12.1 | 启动一个预计持续 20 秒以上的只读任务 | 运行中状态 |
| 12.2 | 点击 Stop | 发送 `session/cancel` notification，等待 prompt response |
| 12.3 | 核对终态 | 运行以 cancelled 终态结束，无永久 spinner、无失联进程 |
| 12.4 | drain 完整性 | 取消后尾部事件无丢失（drain acknowledgement 而非固定 sleep） |
| 12.5 | 恢复使用 | 发送一条最小只读消息，验证 runner 仍可继续使用 |

## 13. 进程清理 (CLEANUP-01)

| 步骤 | 操作 | 预期 |
|---|---|---|
| 13.1 | 正常完成一次运行 | `qodercli` 子进程退出，退出码 0，无僵尸进程 |
| 13.2 | 取消运行后 | 子进程被正确终止，无残留进程 |
| 13.3 | Agent 非正常退出 | 报告 `abnormal exit` 错误，不 hang |
| 13.4 | terminal 子进程 | Agent 创建的 terminal 子进程在 session 结束后被 kill/release |
| 13.5 | 有界终端 | terminal 数量有上限，ring buffer 有界，kill/release 幂等 |

## 14. 凭据不泄漏 (CRED-01)

| 步骤 | 操作 | 预期 |
|---|---|---|
| 14.1 | 设置 `QODER_PERSONAL_ACCESS_TOKEN` 为唯一测试 token（可辨识的 sentinel 值） | — |
| 14.2 | 搜索 raw transcript | 不得出现 token 明文 |
| 14.3 | 搜索诊断日志（tracing） | 不得出现 token 明文 |
| 14.4 | 搜索 stderr 输出 | 不得出现 token 明文 |
| 14.5 | terminal 子进程环境检查 | `is_sensitive_env_name("QODER_PERSONAL_ACCESS_TOKEN")=true`，token 被 `is_sensitive_env_name` 过滤（包含 "TOKEN"），不进入 terminal env |
| 14.6 | Agent 通过 `create_terminal` 启动子进程 | 子进程 `env` 不含 `QODER_PERSONAL_ACCESS_TOKEN` |
| 14.7 | Agent 读取 terminal output | 输出中不含 token（即便子进程尝试 `echo $QODER_PERSONAL_ACCESS_TOKEN` 也为空） |
| 14.8 | 命令行参数检查 | `ps`/`pgrep` 输出中 `qodercli` 命令行不含 token，token 仅在环境变量中 |
| 14.9 | 配置文件检查 | `QODER_CONFIG_DIR` 中不因运行产生含 token 的新文件（除非 Agent 自身写入认证文件，此情况需额外审计） |
| 14.10 | `is_authenticated` 逻辑 | env 中有 `QODER_PERSONAL_ACCESS_TOKEN` → `true`；无 → 检查 `credentials.json`/`oauth_creds.json`/`auth.json` 中是否有 `accessToken`/`access_token`/`refreshToken`/`refresh_token`/`token` |

## 15. 文件变更验证 (FILES-01)

| 步骤 | 操作 | 预期 |
|---|---|---|
| 15.1 | 发送：`仅在当前工作区创建 cli-qoder-<RUN_ID>.txt，内容严格为 QODER_CLI:<RUN_ID>，不要修改其他文件。` | 文件被创建 |
| 15.2 | 核对 Diff 和 files/activity | 只有一个文件变更 |
| 15.3 | `git -C "${REG_CLI_REPO}" status --short` | 只有目标文件 |
| 15.4 | 核对内容 | 内容为 `QODER_CLI:<RUN_ID>` |

## 16. 持久化与收尾 (PERSIST-01)

| 步骤 | 操作 | 预期 |
|---|---|---|
| 16.1 | 刷新应用并重新打开会话 | 消息、runner、模型、运行记录、token 信息保持 |
| 16.2 | 核对秘密值 | UI、日志、报告中不出现 token |
| 16.3 | 清理 | stage 本 runner 的文件并提交，确保仓库重新干净 |

## 验收检查清单

- [ ] INSTALL-01：安装成功，version 可读，diagnostics 识别为 installed
- [ ] AUTH-01：未认证时返回明确错误，不静默成功
- [ ] INIT-01：initialize 握手成功，协议版本 v1，能力快照完整
- [ ] NEW-01：新会话创建成功，命令参数安全，sessionId 记录
- [ ] RESUME-01：续聊使用原生 resume/load，不拼接历史
- [ ] APPROVAL-01：ask 模式弹出审批，允许/拒绝按预期
- [ ] APPROVAL-02：auto_allow 永不回退到 reject
- [ ] APPROVAL-03：auto_reject 自动拒绝
- [ ] ACCESS-01：workspace_only 阻止工作区外访问（含 `..`/绝对路径/symlink）
- [ ] ACCESS-02：full_access 按配置允许工作区外访问
- [ ] MCP-01：允许的 MCP server 被加载并可用
- [ ] MCP-02：隔离生效，strict 模式阻止环境配置合并，secret 脱敏
- [ ] MODEL-01：五档模型可设置且 Agent 确认
- [ ] USAGE-01：token 追踪正确，归属当前 run
- [ ] IMAGE-01：图片 content block 被保留
- [ ] CANCEL-01：取消发送 notification，终态正确，无丢失
- [ ] CLEANUP-01：进程清理完整，无僵尸进程
- [ ] CRED-01：PAT 全程不泄漏到日志/transcript/终端环境/命令行
- [ ] FILES-01：文件变更仅限目标文件
- [ ] PERSIST-01：持久化正确，无秘密泄漏

## 执行命令参考

```bash
# 单元测试（不依赖真实 CLI，可在授权前运行）
cargo test -p executors --features qa-mode
cargo test -p executors --features qa-mode --test acp_qa

# 真实 CLI 验收（需用户授权安装 Qoder CLI）
# 通过 OpenTeams UI 操作 Agent Runtime + 自由聊天
```
