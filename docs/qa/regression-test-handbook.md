---
title: "OpenTeams 功能回归测试操作手册"
description: "供测试 Agent 执行 OpenTeams 全量功能回归、判定功能衰退并产出逐用例测试报告的标准作业手册。"
---

# OpenTeams 功能回归测试操作手册

## 1. 目的

本手册用于判断 OpenTeams 在指定代码版本上是否发生功能衰退。测试 Agent 必须：

1. 按顺序执行本手册中的自动化门禁和功能用例。
2. 为每个用例记录实际结果、证据、耗时和缺陷编号。
3. 不得把“按钮存在”“接口返回 200”单独视为功能通过；必须验证数据、状态和作用域。
4. 即使某用例无法执行，也必须在报告中保留该用例并标记为 `BLOCKED` 或 `SKIPPED`。
5. 使用 `docs/qa/regression-test-report-template.md` 生成一份覆盖全部用例的详细报告。

本手册覆盖当前仓库的核心产品面：

- 项目、工作区与会话
- AI 成员、团队协议和团队模板
- 全部生产 CLI Agent 的安装发现、认证、模型、首次运行、续聊、文件变更、停止和运行记录兼容性
- 自由聊天、流式运行、队列、附件和运行产物
- 工作流计划、调度、输入、审批、审查、重试、跳过、停止和恢复
- 会话源码管理、隔离 worktree、合并和冲突处理
- Issues、GitHub 可选集成
- 设置、Agent runtime、全局搜索、Inbox、构建统计和持久化

未在当前主导航公开的占位页或迁移页不作为独立功能面；GitHub 的实际操作在 Issues/项目集成面执行。

## 2. 测试判定规则

### 2.1 优先级

| 优先级 | 定义 | 发布要求 |
| --- | --- | --- |
| P0 | 启动、数据安全、核心聊天、工作流、源码作用域等阻断性能力 | 必须全部通过 |
| P1 | 主要业务闭环和常用管理能力 | 不允许存在未接受的失败 |
| P2 | 可选外部集成、辅助功能或低频路径 | 可在前置条件不满足时标记 `SKIPPED`，但必须说明原因 |

### 2.2 用例结果

| 结果 | 使用条件 |
| --- | --- |
| `PASS` | 全部步骤已执行，所有验收标准均满足，且证据完整 |
| `FAIL` | 任一验收标准不满足，或出现新的错误、数据串扰、状态不一致 |
| `BLOCKED` | 由于产品缺陷或测试环境故障无法继续；必须创建缺陷并附阻塞证据 |
| `SKIPPED` | 仅限用例明确允许的可选前置条件缺失；不得用于规避失败 |
| `NOT_RUN` | 尚未执行；最终报告中出现此状态时，整体不得判定为通过 |

### 2.3 缺陷严重度

| 严重度 | 定义 |
| --- | --- |
| S1 | 数据丢失、越权/越作用域操作、无法启动、不可恢复破坏 |
| S2 | P0/P1 核心闭环不可用、稳定复现的崩溃或永久卡死 |
| S3 | 有可接受绕过方式的功能错误、状态或内容明显错误 |
| S4 | 轻微视觉、文案或低影响易用性问题 |

### 2.4 整体结论

- `PASS`：P0、P1 全部 `PASS`，P2 已执行或有合规的 `SKIPPED` 理由，且不存在未解决的 S1/S2。
- `CONDITIONAL PASS`：P0 全部通过，仅存在已接受的 S3/S4 或可选外部集成未执行；必须列出风险和批准人。
- `FAIL`：任一 P0 失败、存在 S1/S2、存在 `NOT_RUN`，或用例/证据覆盖不完整。

### 2.5 功能衰退基线

测试负责人必须在执行前指定一个可比较基线，优先顺序为：最近的已发布版本、目标分支最近一次绿色构建、双方约定的基线 Commit。目标版本与基线版本必须使用相同测试数据、Provider、操作系统和浏览器执行同一用例。

- 目标版本失败、基线通过：判定为功能衰退。
- 两个版本均失败：记录为既有缺陷；除非已有明确风险接受，否则当前用例仍不得判为通过。
- 目标版本和基线表现不同但均满足验收：记录行为差异，由产品/研发确认是否为预期变更。
- 无法获得可比较基线：可以报告当前功能失败，但必须写“衰退尚未确认”，不得臆测为回归。

## 3. Agent 强制执行协议

### 3.1 开始前

1. 读取仓库根目录 `AGENTS.md` 和本手册。
2. 记录当前时间、操作系统、Node、pnpm、Rust、浏览器、应用版本、分支、提交 SHA 和 `git status --short`。
3. 确认测试针对的是本次指定提交，而不是另一个工作目录或已部署版本。
4. 确认已有用户数据、已有 Provider、已有 Git 凭据不得被覆盖或删除。
5. 为本轮生成唯一的 `RUN_ID`，格式为 `REG-YYYYMMDD-HHMM-<short-sha>`。

### 3.2 执行中

1. 严格按用例步骤执行；不得在没有记录的情况下修改步骤。
2. 每个用例至少保留一种证据：截图、终端日志、API 响应、Git 状态或数据库只读查询。
3. 所有页面操作同时观察浏览器控制台和失败网络请求；新出现的未处理异常应判为失败。
4. 任何等待动作都必须设置超时。默认页面响应超时 10 秒，Agent 运行超时 5 分钟，工作流超时 15 分钟。
5. 失败后先保存现场，再允许重试一次；原始失败仍需进入报告。
6. 只操作带本轮 `RUN_ID` 的测试数据。不得清理、归档、提交或丢弃其他项目/会话的内容。
7. 涉及 `Discard`、删除项目、删除会话、删除 worktree 等操作时，再次核对目标名称和路径必须含本轮 `RUN_ID`。

### 3.3 证据命名

默认输出目录：

```text
qa_test/<RUN_ID>/
├── report.md
├── logs/
├── evidence/
└── defects/
```

证据文件使用：

```text
<CASE_ID>_<step-number>_<short-description>.<png|txt|json|log>
```

敏感信息不得进入证据。API key、OAuth token、Authorization header、个人目录和私有仓库地址必须打码。

## 4. 前置条件与测试数据

### 4.1 必需前置条件

- 仓库依赖可安装，Rust、Node.js 和 pnpm 满足项目要求。
- 至少配置一个仅用于测试的可运行 Agent/Provider，用于 `CHAT-*` 和 `WF-*`。
- 执行完整 CLI 兼容认证时，测试实验室必须安装并认证 13 个生产 CLI Agent；缺少任一个时，对应 `CLI-*` 用例标记为 `BLOCKED`，不得标记为 `SKIPPED`。
- 浏览器允许访问本地前后端。
- Git 可用，但不得改写全局 Git 配置。
- GitHub 用例需要测试专用 GitHub 账号和测试仓库；缺失时只有 `INT-001`、`INT-002` 可标记为 `SKIPPED`。

### 4.2 创建隔离测试夹具

从仓库根目录执行以下命令。它只在系统临时目录创建测试仓库，并用命令级 Git 身份创建种子提交。

```bash
RUN_ID="REG-$(date +%Y%m%d-%H%M)-$(git rev-parse --short HEAD)"
REG_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/openteams-regression.XXXXXX")"
REG_REPO_A="${REG_ROOT}/${RUN_ID}-repo-a"
REG_REPO_B="${REG_ROOT}/${RUN_ID}-repo-b"
REG_CLI_REPO="${REG_ROOT}/${RUN_ID}-cli-repo"
REG_EVIDENCE="qa_test/${RUN_ID}"

mkdir -p "${REG_REPO_A}/src"
mkdir -p "${REG_REPO_B}"
mkdir -p "${REG_CLI_REPO}"
mkdir -p "${REG_EVIDENCE}/logs"
mkdir -p "${REG_EVIDENCE}/evidence"
mkdir -p "${REG_EVIDENCE}/defects"

git -C "${REG_REPO_A}" init -b main
printf '# %s\n' "${RUN_ID}" > "${REG_REPO_A}/README.md"
printf 'seed-%s\n' "${RUN_ID}" > "${REG_REPO_A}/src/seed.txt"
printf 'base-%s\n' "${RUN_ID}" > "${REG_REPO_A}/conflict.txt"
git -C "${REG_REPO_A}" add .
git -C "${REG_REPO_A}" -c user.name="OpenTeams Regression" -c user.email="regression@example.invalid" commit -m "test: seed ${RUN_ID}"

printf 'plain-%s\n' "${RUN_ID}" > "${REG_REPO_B}/README.md"
git -C "${REG_CLI_REPO}" init -b main
printf '# CLI compatibility %s\n' "${RUN_ID}" > "${REG_CLI_REPO}/README.md"
git -C "${REG_CLI_REPO}" add README.md
git -C "${REG_CLI_REPO}" -c user.name="OpenTeams Regression" -c user.email="regression@example.invalid" commit -m "test: seed CLI compatibility ${RUN_ID}"

printf 'attachment-%s\n' "${RUN_ID}" > "${REG_ROOT}/attachment.txt"
cp readmes/images/openteams-logo.png "${REG_ROOT}/attachment.png"
```

记录并验证：

```bash
printf 'RUN_ID=%s\nREG_ROOT=%s\nREG_REPO_A=%s\nREG_REPO_B=%s\nREG_CLI_REPO=%s\n' \
  "${RUN_ID}" "${REG_ROOT}" "${REG_REPO_A}" "${REG_REPO_B}" "${REG_CLI_REPO}"
git -C "${REG_REPO_A}" status --short
git -C "${REG_REPO_A}" log -1 --oneline
git -C "${REG_CLI_REPO}" status --short
git -C "${REG_CLI_REPO}" log -1 --oneline
```

预期 `git status --short` 为空，最近提交信息包含本轮 `RUN_ID`。

### 4.3 标准测试对象

| 对象 | 名称 |
| --- | --- |
| Git 项目 A | `<RUN_ID>-project-a`，工作区为 `REG_REPO_A` |
| 普通目录项目 B | `<RUN_ID>-project-b`，工作区为 `REG_REPO_B` |
| CLI 兼容项目 | `<RUN_ID>-cli-compat`，工作区为 `REG_CLI_REPO` |
| 主工作区会话 | `<RUN_ID>-main-session` |
| 隔离会话 | `<RUN_ID>-isolated-session` |
| Lead 成员 | `<RUN_ID>-Lead` |
| Worker 成员 | `<RUN_ID>-Worker` |
| 团队模板 | `<RUN_ID>-team-template` |
| 本地 Issue | `<RUN_ID>-issue-local` |

测试结束前不得删除项目 A；后续用例依赖它。

## 5. 固定执行顺序

1. `PRE-*`：环境、门禁、构建和启动。
2. `NAV-*`、`PRJ-*`、`SES-*`：建立可用的测试项目和会话。
3. `MEM-*`：建立测试成员、协议和模板。
4. `CLI-*`：逐一认证全部生产 CLI Agent，并验证跨 Agent 作用域和适配器能力。
5. `CHAT-*`：产生消息、运行记录和文件变更。
6. `WF-*`：产生工作流执行、审查和统计数据。
7. `SCM-*`：验证源码操作和 worktree。
8. `ISS-*`、`INT-*`、`STA-*`：验证事项、集成、设置和统计。
9. 清理测试数据、汇总缺陷并生成最终报告。

若前置用例失败，依赖用例标记为 `BLOCKED`，不得伪造为 `FAIL` 或 `PASS`。

## 6. 自动化门禁

### PRE-001 记录基线与依赖安装

- 优先级：P0
- 前置：仓库可读；本轮尚未改动源代码。

步骤：

1. 记录 `git rev-parse HEAD`、`git branch --show-current` 和 `git status --short`。
2. 记录 `node --version`、`pnpm --version`、`rustc --version`、`cargo --version`。
3. 执行 `pnpm install --frozen-lockfile`，将完整输出保存到 `logs/PRE-001-install.log`。
4. 再次记录 `git status --short`。

验收标准：

- 所有版本命令成功，提交 SHA 与待测版本一致。
- 依赖安装退出码为 0。
- 安装前后的源码工作树差异一致；不得出现无法解释的新修改。

### PRE-002 格式、Lint、类型和生成文件门禁

- 优先级：P0
- 前置：PRE-001 通过。

步骤：

1. 依次执行 `pnpm run format:check`、`pnpm run frontend:check`、`pnpm run backend:lint`。
2. 执行 `pnpm run generate-types:check` 和 `pnpm run prepare-db:check`。
3. 分别保存完整命令、退出码和输出。

验收标准：

- 五条命令退出码均为 0。
- 无 Rust warning 被 Clippy 放过，无 TypeScript/ESLint 错误。
- 生成的 TypeScript 声明和 SQLx 离线缓存与源码一致。

### PRE-003 自动化测试

- 优先级：P0
- 前置：PRE-001 通过。

步骤：

1. 执行 `pnpm run frontend:test`。
2. 执行 `cargo test --workspace --features qa-mode`。
3. 保存总用例数、通过数、失败数、忽略数和完整失败栈。
4. 失败时只重跑失败测试一次，并同时保留首次失败日志。

验收标准：

- 两个测试命令退出码均为 0。
- 没有新增失败、panic、线程永久挂起或测试进程异常退出。
- 被忽略测试的数量和原因已记录；不得用重跑结果覆盖首次失败事实。

### PRE-004 构建、启动和健康检查

- 优先级：P0
- 前置：PRE-002、PRE-003 已完成。

步骤：

1. 执行 `pnpm run frontend:build` 并保存构建日志。
2. 执行 `pnpm dev`，等待前后端端口写入 `.dev-ports.json`。
3. 读取该文件中的 `frontend`、`backend` 端口。
4. 打开前端地址，并请求 `http://localhost:<backend>/api/info`。
5. 保存应用首屏、健康响应、浏览器控制台和启动日志。

验收标准：

- 前端构建和开发服务启动成功。
- `/api/info` 返回 2xx 和可解析 JSON。
- 首屏在 10 秒内可交互，无白屏、无限加载或未处理异常。
- 启动日志中没有数据库迁移失败、端口冲突或持续重启。

## 7. 导航与全局能力

### NAV-001 主导航烟测

- 优先级：P0
- 前置：PRE-004 通过。

步骤：

1. 依次打开 Workspace、Issues、Members、Team templates、Settings、Agent runtime、Build Statistics。
2. 每次记录页面标题或唯一内容、URL/当前 tab、控制台错误和失败请求。
3. 返回 Workspace。

验收标准：

- 每个入口都能在 10 秒内显示对应页面，不出现白屏或错误边界。
- 页面之间切换时项目选择保持不变。
- 各入口不得错误展示为其他页面，返回 Workspace 后原会话仍可继续操作。

### NAV-002 工作区 Tab 生命周期

- 优先级：P1
- 前置：至少存在一个会话；若尚未创建，可在 PRJ-001 后回补执行。

步骤：

1. 打开一个会话 Tab，再打开 Issues 和 Settings Tab。
2. 在各 Tab 之间切换，确认内容和标题对应。
3. 关闭中间 Tab；再尝试关闭最后一个 Tab。
4. 刷新页面后观察当前 Tab 和应用可用性。

验收标准：

- 切换不会丢失其他 Tab，Tab 标题与内容一致。
- 关闭中间 Tab 后自动选中合理的相邻 Tab。
- 应用阻止关闭最后一个 Tab，并给出明确提示。
- 刷新后应用仍可用，不出现指向已删除对象的死 Tab。

### NAV-003 全局搜索与定位

- 优先级：P1
- 前置：CHAT-001 和 ISS-001 已产生带 `RUN_ID` 的消息和 Issue；执行到该阶段后回补。

步骤：

1. 使用全局搜索快捷键或搜索入口，搜索完整 `RUN_ID`。
2. 分别打开一条聊天消息结果和 Issue 结果。
3. 使用无匹配关键词 `<RUN_ID>-NO-MATCH` 搜索。

验收标准：

- 搜索结果只包含有权限且匹配的项目、会话、消息或 Issue。
- 打开消息结果后定位到正确项目/会话；打开 Issue 后定位到正确详情。
- 无匹配时显示明确空状态，不保留旧结果。

### NAV-004 Inbox、未读状态与快捷键

- 优先级：P1
- 前置：WF-003、WF-004 或 WF-009 已产生一个待处理 Inbox 项。

步骤：

1. 打开 Inbox，记录未读计数和对应待处理项。
2. 打开该项，验证是否定位到正确项目、会话和工作流动作。
3. 标记单项已读，再产生/选择另一项并执行“全部已读”。
4. 归档一个测试项；验证它从当前列表消失。
5. 验证打开搜索、关闭弹窗、切换主区域等默认快捷键；不得在文本输入时误触发。

验收标准：

- 未读计数与列表一致，定位后目标卡片可见。
- 单项/全部已读和归档状态在刷新后保持。
- 快捷键只在适用上下文触发；输入框内的普通输入不被拦截。

## 8. 项目与工作区

### PRJ-001 从现有 Git 仓库创建项目

- 优先级：P0
- 前置：`REG_REPO_A` 存在且 Git 状态干净。

步骤：

1. 创建项目，名称填 `<RUN_ID>-project-a`。
2. 通过目录选择器选择 `REG_REPO_A`，或填入其规范化绝对路径。
3. 验证界面识别到 Git 仓库并提交创建。
4. 打开项目，核对名称、路径和默认会话。

验收标准：

- 创建成功且没有重复记录。
- 项目工作区精确指向 `REG_REPO_A`，不得回退到进程 cwd。
- Git 仓库识别正确，项目首个会话可打开。

### PRJ-002 普通目录校验与 Git 初始化

- 优先级：P1
- 前置：`REG_REPO_B` 存在且不是 Git 仓库。

步骤：

1. 创建 `<RUN_ID>-project-b` 并选择 `REG_REPO_B`。
2. 验证界面明确提示该目录不是 Git 仓库，隔离 worktree 选项不可误用。
3. 选择 Git 初始化和一个可用 `.gitignore` 模板后完成创建。
4. 终端只读验证 `git -C "${REG_REPO_B}" status` 和 `.gitignore`。

验收标准：

- 初始化前不会把普通目录误判为 Git 仓库。
- 初始化后 `.git` 存在，`git status` 成功，选择的 `.gitignore` 内容已落盘。
- 初始化只作用于 `REG_REPO_B`。

### PRJ-003 编辑项目并验证项目作用域

- 优先级：P0
- 前置：PRJ-001、PRJ-002 通过。

步骤：

1. 在项目 A 创建一个带 `RUN_ID-A` 的会话，在项目 B 创建一个带 `RUN_ID-B` 的会话。
2. 编辑项目 A 的名称/描述，再刷新确认持久化。
3. 在 A、B 间来回切换，观察会话、成员、Issues、源码变更和统计。
4. 记录任何跨项目泄漏。

验收标准：

- 项目编辑保存并在刷新后保持。
- 每个项目只展示自身会话、成员、Issues 和项目级数据。
- 源码操作始终使用所选项目/会话解析出的工作区，绝不使用另一个项目或进程 cwd。

### PRJ-004 删除测试项目

- 优先级：P1
- 前置：项目 B 的依赖用例已完成；项目 A 保留。

步骤：

1. 对项目 B 发起删除，先取消一次。
2. 验证取消后项目和数据仍存在。
3. 再次删除并确认目标名称包含 `RUN_ID`。
4. 刷新并搜索项目 B。

验收标准：

- 删除前有明确且不可误触的确认。
- 取消不产生任何删除。
- 确认后项目 B 从列表消失且无法重新打开；项目 A 及其文件完全不受影响。

## 9. 会话

### SES-001 创建主工作区会话并验证持久化

- 优先级：P0
- 前置：项目 A 已选择。

步骤：

1. 新建自由聊天会话 `<RUN_ID>-main-session`，选择主工作区模式。
2. 关闭并重新打开该会话。
3. 刷新页面，再重启前后端后打开该会话。

验收标准：

- 会话属于项目 A，模式为主工作区。
- 会话 ID、标题、成员和已有消息在刷新/重启后保持。
- 不会重复创建默认会话或改变其他会话排序。

### SES-002 创建隔离 worktree 会话

- 优先级：P0
- 前置：项目 A 是干净的 Git 仓库。

步骤：

1. 新建 `<RUN_ID>-isolated-session` 并启用“Isolate worktree”。
2. 提交创建但暂不运行 Agent，打开 File Changes。
3. 切换到普通目录项目时再次打开新建会话弹窗，观察隔离选项。

验收标准：

- Git 项目可选择隔离模式，创建后显示“worktree 尚未创建/将在首次运行创建”的状态。
- 未运行 Agent 前不得无理由创建或改动主仓库。
- 非 Git 工作区的隔离选项禁用，并给出明确原因。

### SES-003 重命名、置顶、归档与恢复

- 优先级：P1
- 前置：存在一个本轮测试会话。

步骤：

1. 将测试会话重命名为 `<RUN_ID>-renamed`。
2. 置顶并切换其他会话，验证排序；再取消置顶。
3. 归档该会话，确认它离开活动列表。
4. 在 Settings 的 Archived sessions 中恢复，再刷新。

验收标准：

- 名称和置顶状态即时更新并持久化。
- 归档会话不会出现在活动列表，且归档不会删除消息/成员。
- 恢复后会话重新可用，历史完整。

### SES-004 删除会话和安全保护

- 优先级：P1
- 前置：创建一个仅用于删除的 `<RUN_ID>-delete-session`。

步骤：

1. 发起删除并取消，验证会话仍存在。
2. 再次确认删除，刷新并搜索该会话。
3. 尝试关闭最后一个 Tab。

验收标准：

- 删除确认清楚显示目标会话，取消无副作用。
- 确认后会话不再可访问，其删除不影响同项目其他会话。
- 最后一个 Tab 保护仍然生效。

## 10. 成员、协议和团队模板

### MEM-001 添加自定义项目成员

- 优先级：P0
- 前置：有可用测试 Agent/Provider。

步骤：

1. 在项目 A 的 Members 中新增 `<RUN_ID>-Lead`，角色说明包含“负责计划、分工和验收”。
2. 选择可用 runner/model，工作区选择 `REG_REPO_A`。
3. 新增 `<RUN_ID>-Worker`，角色说明包含“按步骤执行并提交证据”。
4. 刷新成员页和测试会话。

验收标准：

- 两名成员只出现在项目 A，配置与输入一致。
- 运行时可用性状态与 Agent runtime 页面一致。
- 刷新后成员不重复、不丢失。

### MEM-002 编辑和移除成员

- 优先级：P1
- 前置：MEM-001 通过。

步骤：

1. 编辑 Worker 的名称描述或模型并保存。
2. 重新打开验证保存结果。
3. 新建一个仅用于删除的 `<RUN_ID>-temp-member`，取消一次移除，再确认移除。

验收标准：

- 允许编辑的字段保存并持久化；不可编辑字段不得被静默改写。
- 取消移除后成员仍在；确认后仅临时成员被移除。
- Lead、Worker 和其他项目成员不受影响。

### MEM-003 团队协议持久化与注入

- 优先级：P1
- 前置：MEM-001 通过。

步骤：

1. 保存协议：`所有测试回复必须包含 RUN_ID=<RUN_ID>；不得修改未指定文件。`
2. 刷新 Members 页面，确认协议文本保持。
3. 在主会话向 Worker 发送“回复当前 RUN_ID，不修改文件”。
4. 查看运行输入或执行记录中的协议上下文。

验收标准：

- 协议按项目保存，刷新后文本完全一致。
- 运行输入包含当前项目协议，Agent 回复包含正确 `RUN_ID`。
- 切换项目时不得注入项目 A 的协议。

### MEM-004 团队模板创建、编辑和删除

- 优先级：P1
- 前置：Agent 配置可用。

步骤：

1. 创建 `<RUN_ID>-team-template`，添加 Lead、Worker、团队协议和两个工作流步骤。
2. 保存并打开只读详情，逐字段核对。
3. 编辑协议、成员职责和第二个步骤；保存并刷新。
4. 创建一个临时模板，验证删除确认后删除。

验收标准：

- 模板名称、协议、成员、runner/model、技能/MCP 配置和工作流步骤无丢失。
- 空白步骤不会被保存，步骤顺序稳定。
- 编辑后详情与最新值一致；删除只作用于临时模板。

### MEM-005 从模板实例化会话

- 优先级：P1
- 前置：MEM-004 通过。

步骤：

1. 从 `<RUN_ID>-team-template` 创建项目 A 的新会话。
2. 打开会话成员列表和团队协议。
3. 向 Lead、Worker 各发送一次只读问候。

验收标准：

- 会话成员、角色、模型和协议与模板一致。
- 实例化不会反向修改模板，也不会创建重复项目成员。
- 两名成员均可被 `@` 提及并进入运行。

## 11. 全部 CLI Agent 兼容性

### 11.1 生产 CLI Agent 清单

兼容性清单以生产 `BaseCodingAgent` 类型为真源。当前必须覆盖以下 13 个 runner；测试 Agent 不得仅抽测自己已安装的子集。

| 用例 | 产品名称 | runner key | 主要启动方式 | 兼容性重点 |
| --- | --- | --- | --- | --- |
| CLI-101 | Claude Code | `CLAUDE_CODE` | OpenTeams 固定版本的 `@anthropic-ai/claude-code` | 首次运行、续聊、上下文用量、slash commands |
| CLI-102 | Amp | `AMP` | OpenTeams 固定版本的 `@sourcegraph/amp` | mode 映射、流式 JSON、续聊 |
| CLI-103 | Gemini CLI | `GEMINI` | `gemini` / ACP | ACP probe、认证方法、模型选项、权限 |
| CLI-104 | OpenAI Codex | `CODEX` | OpenTeams 固定版本的 `@openai/codex` app-server | thread 续聊、reasoning、上下文用量 |
| CLI-105 | OpenCode | `OPENCODE` | OpenTeams 管理的 OpenCode runtime | provider/model 发现、会话、slash commands |
| CLI-106 | OpenTeams CLI | `OPEN_TEAMS_CLI` | 同目录、开发二进制、用户目录或 PATH 中的 bundled CLI | 二进制优先级、模型、会话 |
| CLI-107 | Cursor Agent CLI | `CURSOR_AGENT` | `cursor-agent` | 安装/认证提示、stream-json、模型 |
| CLI-108 | Qwen Code | `QWEN_CODE` | `qwen` / ACP | ACP probe、模型、权限、续聊 |
| CLI-109 | GitHub Copilot CLI | `COPILOT` | OpenTeams 固定版本的 `@github/copilot` | 登录、流式运行、续聊 |
| CLI-110 | Factory Droid | `DROID` | `droid exec` | autonomy level、权限、stream-json |
| CLI-111 | Kimi Code | `KIMI_CODE` | `kimi acp` | ACP probe、provider/model、认证、续聊 |
| CLI-112 | Pi | `PI` | OpenTeams 固定版本的 `pi-acp` + `@earendil-works/pi-coding-agent` + `pi-mcp-adapter` (NPX) | ACP initialize、模型刷新、Skill/MCP 成员隔离、`--no-skills` 强制、三种审批策略、provider 配置同步、session/load 续聊 |
| CLI-113 | Qoder CLI | `QODER_CLI` | `qodercli --acp` | ACP probe、五档模型、PAT 认证、`--strict-mcp-config`、workspace/full access、session/resume 续聊 |

`QA_MOCK` 和 `ACP_QA` 仅在 `qa-mode` 中存在，不属于生产 CLI 兼容清单，由 PRE-003 自动化测试覆盖。Claude Code Router（CCR）不是独立 `BaseCodingAgent`，但作为 `CLAUDE_CODE` 的受支持适配器变体由 CLI-204 单独覆盖。

### 11.2 标准逐 CLI 执行程序

CLI-101 至 CLI-113 都必须完整执行以下步骤，不得用“其他 Agent 已通过”代替。每个 runner 使用独立成员 `<RUN_ID>-<runner-key>` 和独立会话 `<RUN_ID>-cli-<runner-key>`，工作区固定为 `REG_CLI_REPO`。

1. **发现与诊断**：在 Agent runtime 选择目标 runner，执行 Refresh；记录 `installed`、`executable`、availability、version、resolved command、command source、config path、run mode、discovered models、model source、last error。对已安装且已认证的 CLI，产品必须识别为可运行。
2. **配置持久化**：保持 `run_mode=auto` 或测试批准的 local 模式，选择一个已发现/实验室指定模型；若有 reasoning、mode、autonomy 或 ACP 选项，选择安全测试值。保存、刷新页面并重新打开核对。
3. **首次运行**：创建目标成员和自由聊天会话，发送：`仅回复 RUNTIME=<runner-key>; RUN_ID=<RUN_ID>; NONCE=<runner-key>-FIRST，不修改文件。` 记录实际 runner、命令、模型、流式消息和终态。
4. **续聊**：在同一成员发送：`不要读取磁盘，复述上一条消息中的 NONCE，并回复 FOLLOWUP-OK。` 核对外部会话/thread ID、上下文和新运行记录；若适配器内部重建进程，也必须保持产品层续聊语义。
5. **文件变更**：发送：`仅在当前工作区创建 cli-<runner-key>-<RUN_ID>.txt，内容严格为 <runner-key>:<RUN_ID>，不要修改其他文件。` 核对文件、Diff、运行 files/activity/log 和 `git -C "${REG_CLI_REPO}" status --short`。
6. **停止与恢复使用**：启动一个预计持续 20 秒以上的只读任务，在运行中 Stop；确认中断终态后发送一条最小只读消息，验证该 runner 仍可继续使用。
7. **持久化与收尾**：刷新应用并重新打开会话，核对消息、runner、模型、运行记录、token/context 信息和文件证据。只 stage 本 runner 的文件并提交 `test: <runner-key> compatibility <RUN_ID>`，确保仓库重新干净。

逐 CLI 通用验收标准：

- OpenTeams 使用所选 runner，不得静默回退到默认 Agent、错误 runner 或进程 cwd。
- 首次运行、续聊、文件写入和 Stop 均有明确终态；不得重复消息、重复运行、永久 spinner 或失联进程。
- 续聊返回正确 NONCE；运行记录保留真实 runner、模型和外部 session/thread 标识（若 CLI 提供）。
- 文件只出现在 `REG_CLI_REPO`，内容与 runner key 对应；其他项目和用户目录无变更。
- 模型、运行模式和适配器配置在刷新/重启后保持；秘密值不得出现在 UI、日志或报告。
- CLI 不提供 token/context 或模型枚举时，UI 必须显示明确的“不可用/配置来源”，不得伪造数据或沿用其他 runner 缓存。

### CLI-001 生产 Runner 清单、安装与认证前检

- 优先级：P0
- 前置：PRE-004 通过；测试实验室声明已准备全部生产 CLI。

步骤：

1. 从生成的 `BaseCodingAgent` 类型和 Agent runtime 页面分别收集生产 runner key。
2. 对照本节 13 项清单，记录每项安装方式、认证账号类型、CLI 版本和凭据责任人；报告中只记录账号别名，不记录秘密。
3. 对 13 项逐一执行 Refresh 和 diagnostics，保存状态矩阵。
4. 创建 `<RUN_ID>-cli-compat` 项目，工作区设为 `REG_CLI_REPO`。
5. 对无法安装/认证/执行的项停止对应 runner 用例并标记 `BLOCKED`；判断是实验室缺口还是产品发现错误。

验收标准：

- 类型真源、Agent runtime 和报告矩阵的 13 个生产 runner 一一对应，无缺失、重复或错误 key。
- QA-only runner 不出现在生产 UI。
- 已安装/认证项显示 executable；已知未安装项显示明确 Not found/setup 信息，不导致全页失败。
- CLI 兼容项目精确使用 `REG_CLI_REPO`。

### CLI-101 Claude Code 兼容性

- 优先级：P1
- 前置：Claude Code 已安装/可由固定 npx 命令启动并完成认证；CCR 关闭。

步骤：

1. 以 `CLAUDE_CODE` 完整执行“标准逐 CLI 执行程序”步骤 1–7。
2. 在 diagnostics 记录 Claude Code 版本、实际 command source、模型和 slash command 加载状态。
3. 在首次运行与续聊后核对 context/token usage 和 Claude session 标识。

验收标准：

- 满足全部逐 CLI 通用验收标准。
- 实际 runner 为 `CLAUDE_CODE` 且未误启用 CCR。
- slash commands 加载失败时有局部错误而不是阻断聊天；CLI 提供的上下文用量可正确归属当前运行。

### CLI-102 Amp 兼容性

- 优先级：P1
- 前置：Amp 已安装/可由固定 npx 命令启动并完成认证。

步骤：

1. 以 `AMP` 完整执行“标准逐 CLI 执行程序”步骤 1–7。
2. 选择实验室支持的 `smart`、`deep`、`rush` 或 `free` mode，保存后核对实际 CLI 参数语义。
3. 检查 stream-json 归一化后的增量消息和最终消息。

验收标准：

- 满足全部逐 CLI 通用验收标准。
- Agent runtime 的 model/mode 不被错误当作普通模型 ID，保存和运行一致。
- 流式片段不重复拼接，续聊不会把旧线程全部重复输出到新消息。

### CLI-103 Gemini CLI 兼容性

- 优先级：P1
- 前置：Gemini CLI 已安装、认证，ACP probe 成功。

步骤：

1. 以 `GEMINI` 完整执行“标准逐 CLI 执行程序”步骤 1–7。
2. 记录 ACP agent version、认证方法、模型 config option 和 permission option。
3. 首次运行使用 `workspace_only + ask`，对一个仅写入目标文件的权限请求选择允许。

验收标准：

- 满足全部逐 CLI 通用验收标准。
- ACP probe 和实际运行均为 Gemini，模型/认证选项来自当前 probe，不沿用 Qwen/Kimi。
- workspace-only 阻止仓库外访问；ask 审批只作用于当前请求。

### CLI-104 OpenAI Codex 兼容性

- 优先级：P1
- 前置：Codex 已安装/可由固定 npx 命令启动并完成认证。

步骤：

1. 以 `CODEX` 完整执行“标准逐 CLI 执行程序”步骤 1–7。
2. 记录 app-server 连接、thread ID、模型、reasoning effort 和 command source。
3. 检查首次运行与续聊的 thread 关联以及 token/context usage。

验收标准：

- 满足全部逐 CLI 通用验收标准。
- 实际通过 Codex app-server 执行，续聊使用正确 thread，不创建无关会话或串用其他成员 thread。
- reasoning 配置和用量属于当前 Codex run；认证/配置目录信息不泄漏秘密。

### CLI-105 OpenCode 兼容性

- 优先级：P1
- 前置：OpenCode runtime 可用并已配置测试 provider。

步骤：

1. 以 `OPENCODE` 完整执行“标准逐 CLI 执行程序”步骤 1–7。
2. 记录 OpenTeams 解析出的 runtime/version、provider、model source 和 slash commands。
3. 切换一次已配置模型并执行最小只读消息，再恢复测试模型。

验收标准：

- 满足全部逐 CLI 通用验收标准。
- 使用当前 OpenCode runtime，模型发现和执行 provider 一致。
- runtime 切换/缓存不串到 OpenTeams CLI 或其他 OpenCode 工作区，slash command 错误可诊断。

### CLI-106 OpenTeams CLI 兼容性

- 优先级：P0
- 前置：开发或发布产物包含可执行的 OpenTeams CLI。

步骤：

1. 以 `OPEN_TEAMS_CLI` 完整执行“标准逐 CLI 执行程序”步骤 1–7。
2. 记录二进制解析来源：显式 override、服务端同目录、开发 `binaries`、用户 bundled 目录或 PATH。
3. 验证模型列表、slash commands、首次运行和续聊均来自同一解析出的 CLI。

验收标准：

- 满足全部逐 CLI 通用验收标准。
- bundled CLI 在支持的开发/发布方式中无需用户另装第三方 CLI 即可发现。
- 二进制优先级稳定且日志显示真实来源；runner key 始终为 `OPEN_TEAMS_CLI`。

### CLI-107 Cursor Agent CLI 兼容性

- 优先级：P1
- 前置：`cursor-agent` 已安装并通过登录或测试 API key 认证。

步骤：

1. 以 `CURSOR_AGENT` 完整执行“标准逐 CLI 执行程序”步骤 1–7。
2. 记录 setup helper、resolved command、模型列表以及 `-p --output-format=stream-json` 运行结果。
3. 在认证缺失的受控副本中观察一次 setup/auth 错误展示，随后恢复测试认证。

验收标准：

- 满足全部逐 CLI 通用验收标准。
- 未安装/未认证时提供准确 setup 指引；已认证后 Refresh 可恢复 executable。
- stream-json 正确归一化，认证错误不被误报为模型或工作区错误。

### CLI-108 Qwen Code 兼容性

- 优先级：P1
- 前置：Qwen Code 已安装、认证，ACP probe 成功。

步骤：

1. 以 `QWEN_CODE` 完整执行“标准逐 CLI 执行程序”步骤 1–7。
2. 记录 ACP agent version、认证方法、模型和权限选项。
3. 使用 `workspace_only + ask` 执行目标文件写入并完成单次审批。

验收标准：

- 满足全部逐 CLI 通用验收标准。
- ACP probe、模型选项、权限和 run records 均归属 `QWEN_CODE`。
- Qwen 的配置和 native skill 路径不与 Gemini、Kimi 或 OpenCode 混用。

### CLI-109 GitHub Copilot CLI 兼容性

- 优先级：P1
- 前置：GitHub Copilot CLI 已安装并用测试账号完成认证。

步骤：

1. 以 `COPILOT` 完整执行“标准逐 CLI 执行程序”步骤 1–7。
2. 记录 CLI 版本、认证状态、模型发现结果和 stream 输出。
3. 核对续聊、Stop 后恢复以及 GitHub 账号信息的脱敏展示。

验收标准：

- 满足全部逐 CLI 通用验收标准。
- Copilot 不得借用项目 GitHub integration 的 OAuth 状态冒充 CLI 登录。
- CLI 不提供的模型/context 能力显示为明确不可用，不复用其他 runner 数据。

### CLI-110 Factory Droid 兼容性

- 优先级：P1
- 前置：Droid 已安装并用测试账号或测试 API key 认证。

步骤：

1. 以 `DROID` 完整执行“标准逐 CLI 执行程序”步骤 1–7。
2. 将 autonomy level 设为允许工作区测试文件写入但不允许高风险系统改动的级别。
3. 记录 `droid exec` stream-json、模型、审批和 Stop 行为。

验收标准：

- 满足全部逐 CLI 通用验收标准。
- autonomy level 保存并映射到本次 Droid 运行；不得静默升级为 unsafe。
- 安全级别不足时显示权限错误，不伪装为运行成功或修改仓库外文件。

### CLI-111 Kimi Code 兼容性

- 优先级：P1
- 前置：Kimi Code 已安装、认证，ACP probe 和 provider discovery 成功。

步骤：

1. 以 `KIMI_CODE` 完整执行“标准逐 CLI 执行程序”步骤 1–7。
2. 记录 ACP agent version、terminal auth method、provider/model 和权限选项。
3. 使用 `workspace_only + ask` 执行目标文件写入并完成单次审批。

验收标准：

- 满足全部逐 CLI 通用验收标准。
- `kimi acp`、provider list 和实际运行使用一致的 Kimi 配置。
- Kimi 的认证、模型、权限和 session 信息不串到 Gemini/Qwen。

### CLI-112 Pi 兼容性

- 优先级：P1
- 前置：Node.js 与 npx 均可执行；Pi 未全局安装不影响测试；三个 NPX 包（`pi-acp@0.0.33`、`@earendil-works/pi-coding-agent@0.83.0`、`pi-mcp-adapter@2.18.0`）已在 npm 缓存或可网络获取；provider API Key 已配置。

步骤：

1. 以 `PI` 完整执行"标准逐 CLI 执行程序"步骤 1–7。
2. **发现与诊断补充**：确认 availability 只依赖 Node.js + npx（不依赖全局 Pi）；记录 `pi-acp` 版本 0.0.33、Pi coding-agent 版本 0.83.0、MCP adapter 版本 2.18.0；确认 Pi models sync 状态为 `synchronized=true`。
3. **模型刷新**：执行 ACP probe，记录 `initialize` 事件中的 `configOptions`（category=model）；确认模型值来自 ACP 服务端返回，不经前端硬编码或格式转换。
4. **Provider 配置同步**：确认 Pi provider 配置同步将 `openteams-` 命名空间的受管条目原子写入 `~/.pi/agent/providers.json`（隔离 HOME）；验证 0600 权限、无效 JSON 保护、密钥以 Pi 字面量编码且不泄漏到命令行、日志或 API 响应。
5. **Skill/MCP 成员隔离**：为两个差异化成员分别配置不同的 MCP allowlist 和 Skill 路径；验证 `freeze_runtime_snapshot` 按成员策略过滤 `mcpServers`；验证 `--no-skills` 强制追加；验证未授权的 MCP 服务器不出现在启动参数、运行时快照或工具列表中；验证密钥在快照中被遮蔽。
6. **三种审批策略**：对原生工具（bash）和 MCP 工具分别执行 `ask`（弹出审批 UI，允许/拒绝按预期）、`auto_allow`（永不回退到 reject）、`auto_reject`（自动拒绝）；验证 `permission.jsonl` 记录正确的决策。
7. **session/load 续聊**：在同一成员发送续聊消息，验证使用 `session/load`（非历史拼接）；核对外部 session ID 复用。
8. **取消**：启动长任务并 Stop，验证 `session/cancel` notification 发送、protocol.jsonl 记录 cancel 事件、终态正确、进程树清理完整。
9. **离线 fixture 回归**：确认 `cargo test -p executors --features qa-mode --test pi_acp_fixture` 在无 npm 网络、无全局 Pi、无真实 provider 密钥条件下通过；真实 NPX 冒烟为 `#[ignore]` 不进入默认 CI。

验收标准：

- 满足全部逐 CLI 通用验收标准。
- 实际 runner 为 `PI`，命令中包含固定版本 `pi-acp@0.0.33`、`@earendil-works/pi-coding-agent@0.83.0`、`pi-mcp-adapter@2.18.0`，无 `latest` 或未定版本。
- `--no-skills` 强制追加到 launcher 命令；只有成员 Registry 校验通过的 Skill 路径进入 Pi 参数。
- 仅成员 MCP allowlist 中的服务器进入隔离快照和工具面；未授权的全局或项目 MCP 不会启动。
- 原生工具与 MCP 工具均经过三种审批策略，审批门一致（`approval_extension.mjs` 对所有 `tool_call` 使用同一 `ctx.ui.confirm`）。
- provider 配置同步使用原子合并（同目录 create_new 临时文件 + 0600 + rename），无效旧 JSON 不损坏原文件。
- 密钥只以 Pi 原生字面值进入受保护文件，不进入命令行、日志、普通 API 错误或前端响应。
- 取消后 `protocol.jsonl` 包含 `session/cancel` 记录，进程树完整清理。
- 参考文档：`docs/agents/pi.md`（架构与维护）、`docs/pi-agent-e2e-regression-test-plan.md`（E2E 测试用例）。

### CLI-113 Qoder CLI 兼容性

- 优先级：P1
- 前置：用户已授权安装 Qoder CLI；`QODER_PERSONAL_ACCESS_TOKEN` 通过环境变量注入；`QODER_CONFIG_DIR` 指向隔离测试目录。

步骤：

1. 以 `QODER_CLI` 完整执行"标准逐 CLI 执行程序"步骤 1–7。
2. **安装与版本**：执行 `qodercli --version`，确认 `installed=true`、`executable` 解析到 `qodercli`；Agent Runtime Refresh 后 availability 为 `INSTALLATION_FOUND`。
3. **未认证错误**：不设置 PAT 且 `QODER_CONFIG_DIR` 为空目录时，`is_authenticated` 返回 false；尝试发送消息返回明确的未认证类型错误，不静默成功。
4. **initialize 握手**：ACP probe 发送 `ProtocolVersion::V1`，响应协议版本 v1；记录 agent name=Qoder、`session_capabilities`（new/resume/load/close/delete）、`configOptions`（category=model 包含五档模型 lite/efficient/auto/performance/ultimate）。
5. **五档模型**：逐档通过 `session/set_config_option` 设置模型，每次 Agent 响应确认 requested value 为 current；设置 `lite` 和 `performance` 各发送最小只读消息，核对 usage 记录对应模型。
6. **三种审批策略**：对 `workspace_only + ask` 执行文件写入并完成单次审批；切换 `auto_allow` 验证永不回退到 reject；切换 `auto_reject` 验证自动拒绝；确认配置改变只影响 `QODER_CLI`，其他 runner 不受影响。
7. **Workspace/Full Access**：`workspace_only` 模式下 Agent 读取/写入工作区内文件成功，访问工作区外文件（含 `..`/绝对路径/symlink escape）被拒绝；`full_access` 模式下 `additional_directories` 内可访问，外部仍被拒绝。
8. **MCP 允许与隔离**：配置无秘密测试 MCP stdio server，`--allowed-mcp-server-names` 包含允许的名称；配置两个 server 时 policy 只允许一个，`--strict-mcp-config` 阻止环境配置合并；空 allowlist 时不加载任何 server；MCP env/header secret 脱敏。
9. **session/resume 续聊**：在同一成员发送续聊消息，验证使用 `session/resume`（非历史拼接）；核对外部 session ID 复用；Agent 正确复述上一轮 NONCE。
10. **凭据不泄漏**：设置 sentinel PAT 值，搜索 raw transcript、诊断日志、stderr 输出不得出现 token 明文；terminal 子进程环境不含 `QODER_PERSONAL_ACCESS_TOKEN`（被 `is_sensitive_env_name` 过滤）；命令行参数不含 token。
11. **进程清理**：正常完成时 `qodercli` 子进程退出码 0；取消运行后子进程被正确终止；Agent 非正常退出时报告 `abnormal exit` 不 hang。
12. **图片支持**：发送附带图片的消息，`session/prompt` 请求中 image content block 被保留；流式响应正常。

验收标准：

- 满足全部逐 CLI 通用验收标准。
- 实际 runner 为 `QODER_CLI`，命令为 `qodercli --acp --permission-mode default --strict-mcp-config --allowed-mcp-server-names <allowlist>`，无 `--yolo`/`--dangerously-skip-permissions` 等冲突标志。
- 五档模型（lite/efficient/auto/performance/ultimate）可设置且 Agent 确认 current value = requested value。
- `workspace_only` 阻止工作区外访问（含 `..`/绝对路径/symlink）；`full_access` 按配置允许 `additional_directories` 内访问。
- `--strict-mcp-config` 生效，环境级/项目级 MCP 配置不被合并；secret 脱敏。
- PAT 全程不泄漏到日志/transcript/终端环境/命令行/配置文件；`is_sensitive_env_name("QODER_PERSONAL_ACCESS_TOKEN")=true`。
- 续聊使用原生 `session/resume`，不拼接历史；NONCE 正确复述。
- 参考文档：`docs/qa/qoder-cli-acp-acceptance-plan.md`（完整验收方案）。

### CLI-201 跨 Runner 会话、日志与工作区隔离

- 优先级：P0
- 前置：CLI-101 至 CLI-113 均已产生首次运行、续聊和文件提交。

步骤：

1. 汇总 13 个会话的 runner key、成员 ID、run ID、外部 session/thread ID、模型和 resolved workspace。
2. 逐个重新打开会话，要求复述各自 NONCE；不得再次提供 NONCE。
3. 对照 `REG_CLI_REPO` 的 13 个文件和提交，核对作者 runner 与内容。
4. 搜索每个 runner 的日志和错误，检查是否出现其他 runner 的命令、模型、认证信息或 session ID。

验收标准：

- 13 个 runner 的运行归属、会话上下文和日志边界清晰，无静默 fallback 或跨成员 session 复用。
- 每个 Agent 只复述自己的 NONCE；共享公开聊天历史之外的私有 runner 状态不得泄漏。
- 工作区均为 `REG_CLI_REPO`，文件和提交与对应 runner 一一匹配。

### CLI-202 ACP Runner 配置与审批兼容性

- 优先级：P1
- 前置：CLI-103、CLI-108、CLI-111、CLI-112、CLI-113 通过。

步骤：

1. 分别打开 Gemini、Qwen、Kimi、Pi、Qoder diagnostics，保存各自 ACP probe、config options 和 auth methods。
2. 对五者逐一测试 `workspace_only + ask`、一次拒绝、一次允许和一个额外工作目录配置。
3. 将其中一个 runner 改为 `auto_reject`，确认只影响该 runner 后恢复。
4. 刷新和重启，核对五者配置持久化与隔离。

验收标准：

- 五个 ACP runner 的 probe、模型、认证和权限选项各自独立。
- ask/allow/reject 语义一致，配置改变只影响目标 runner。
- 额外目录必须规范化并受访问边界约束；重启后设置不串位。
- Pi 的 `--no-skills` 和 Qoder 的 `--strict-mcp-config` 在各自命令中独立生效。

### CLI-203 MCP 与 Native Skill 适配兼容性

- 优先级：P1
- 前置：实验室为每个 runner 准备一个无秘密、只返回 `<runner-key>:<RUN_ID>` 的测试 MCP 工具，以及一个同名只读测试 Skill。

步骤：

1. 对 13 个 runner 记录 diagnostics 中的 MCP/Skill 配置路径、native discovery roots 和 toggle 能力。
2. 在测试成员 Skills 配置中选择对应 runner 的测试 Skill，刷新并核对选择。
3. 在单独消息中要求 Agent 调用测试 MCP 工具并遵循测试 Skill 输出格式。
4. 禁用可 toggle 的测试 Skill 后重试；对不可 toggle 的 runner 只验证明确的只读状态。
5. 恢复测试配置并确认其他 runner 的 MCP/Skill 文件和选择未变化。

验收标准：

- 13 个 runner 均读取自己的配置 schema/path，不把 `mcp` 与 `mcpServers` 等不同结构互相覆盖。
- MCP 返回值、Skill 指令和运行记录的 runner key 一致；无工具重复调用。
- toggle 能力与 UI/后端一致，不支持 toggle 时给出明确状态而非假成功。
- 测试配置不得覆盖用户已有 MCP、Skill 或秘密。

### CLI-204 Claude Code Router 兼容性

- 优先级：P2
- 前置：测试专用 CCR provider/model 已配置；缺失时允许 `SKIPPED`，但 Claude Code CLI-101 仍必须执行。

步骤：

1. 为 `CLAUDE_CODE` 创建仅本轮使用的 CCR 变体，启用 `claude_code_router` 并选择测试 model mapping。
2. 执行标准程序中的首次运行、续聊、文件写入和 Stop。
3. 记录实际命令、CCR provider/model、Claude session 和文件证据。
4. 切回非 CCR Claude 配置，再运行最小消息确认没有配置泄漏。

验收标准：

- CCR 以 `CLAUDE_CODE` 变体运行，不作为未知 runner 或独立生产 key 出现。
- 启用时实际使用 CCR 命令/provider/model；关闭后恢复标准 Claude Code。
- 两种变体的会话、模型、日志和认证错误可区分，秘密不进入报告。

## 12. 自由聊天

### CHAT-001 单 Agent 提及、流式输出和最终态

- 优先级：P0
- 前置：主会话含 Worker，测试 Provider 可用。

步骤：

1. 发送：`@<Worker> 仅回复 RUN_ID=<RUN_ID>，不要修改文件。`
2. 观察用户消息、运行中状态、流式增量和最终 Agent 消息。
3. 刷新页面并重新打开会话。

验收标准：

- 只触发被提及的 Worker，一次用户消息对应一次运行。
- 运行状态从排队/运行中进入明确终态，没有永久 spinner。
- 最终内容包含正确 `RUN_ID`，刷新后消息顺序和内容保持。

### CHAT-002 多 Agent 共享上下文

- 优先级：P1
- 前置：主会话含 Lead 和 Worker。

步骤：

1. 向 Lead 发送：`记住校验词 <RUN_ID>-CONTEXT，只回复已记录。`
2. 再同时提及 Lead 和 Worker，要求 Lead 说明校验词、Worker 复述 Lead 的上一条公开消息。
3. 检查运行输入、消息发送者和顺序。

验收标准：

- 两个成员各自产生独立且可识别的运行。
- Lead 能读取本会话上下文；Worker 只基于共享可见上下文回答。
- 不混入其他项目/会话的消息，消息归属和时间顺序正确。

### CHAT-003 文本、图片附件和下载

- 优先级：P1
- 前置：`attachment.txt`、`attachment.png` 已创建。

步骤：

1. 在一条消息中附加文本和图片，发送给 Worker 并要求列出文件名。
2. 打开图片预览，关闭后重新打开。
3. 打开或下载文本附件，核对内容中的 `RUN_ID`。
4. 刷新会话后再次访问附件。

验收标准：

- 上传进度完成，消息只创建一次，两个附件名称和类型正确。
- Agent 能收到附件元数据/内容并给出正确文件名。
- 图片可预览，文本内容未损坏，刷新后附件仍可访问。
- 不得暴露服务器绝对路径或其他会话附件。

### CHAT-004 引用、复制与消息操作

- 优先级：P1
- 前置：存在一条 Agent 消息。

步骤：

1. 引用该 Agent 消息并发送跟进问题。
2. 验证新消息显示正确引用摘要，点击引用定位原消息。
3. 复制原消息并与原文比较。
4. 刷新会话，再次检查引用摘要、定位关系和复制结果。

验收标准：

- 引用关系指向正确消息，刷新后仍存在。
- 复制内容与可见正文一致，不包含隐藏元数据。
- 引用和复制能力在刷新后仍对应原消息，不会指向相邻或其他会话消息。

### CHAT-005 运行中消息队列

- 优先级：P0
- 前置：Provider 支持持续数十秒的测试任务。

步骤：

1. 向 Worker 发送一个预计运行 20 秒以上的只读任务。
2. Worker 仍在运行时，再发送两条分别含 `<RUN_ID>-Q1`、`<RUN_ID>-Q2` 的任务。
3. 打开队列，记录顺序；删除 Q2。
4. 等待首任务完成，必要时使用 Continue 继续队列。

验收标准：

- 新任务进入同一成员队列，不并发覆盖当前运行。
- 队列顺序为 Q1 后 Q2；删除只移除 Q2。
- Q1 在前序完成后执行且只执行一次，Q2 不执行。
- 队列状态和消息终态一致，刷新后无幽灵队列项。

### CHAT-006 停止、失败展示与重发

- 优先级：P0
- 前置：可启动一个持续任务。

步骤：

1. 启动持续任务并在运行中点击 Stop。
2. 等待终态，记录 Agent 状态、消息状态和运行日志。
3. 对失败/中断消息执行 Resend，或用相同内容重新发起。
4. 等待重发任务完成。

验收标准：

- Stop 在超时内终止对应运行，不影响其他成员。
- 中断不会伪装成成功，UI 提供清晰终态和可追踪日志。
- 重发只创建一次新运行，原运行记录保留，新运行可独立完成。

### CHAT-007 运行日志、活动、Diff 和未跟踪文件

- 优先级：P0
- 前置：主仓库 Git 状态干净。

步骤：

1. 向 Worker 发送：`创建 reg-agent-<RUN_ID>.txt，内容严格为 <RUN_ID>，不要修改其他文件。`
2. 等待完成，打开该次运行的 output、raw log、activity、file changes。
3. 打开文件 Diff/未跟踪文件内容。
4. 终端执行 `git -C "${REG_REPO_A}" status --short` 并核对。

验收标准：

- Agent 只创建指定文件，文件内容正确。
- 运行记录属于正确会话/成员，日志和 activity 可访问。
- UI 文件列表、状态、Diff/内容与 Git 结果一致。
- `.openteams/` 运行数据不得被当作用户源码变更展示。

## 13. 工作流

工作流基准提示词：

```text
请生成并执行一份工作流：
1. 步骤 A 由 <RUN_ID>-Worker 创建 wf-a-<RUN_ID>.txt，内容为 A:<RUN_ID>。
2. 步骤 B 必须依赖步骤 A，读取 A 的文件后创建 wf-b-<RUN_ID>.txt，内容为 B:<RUN_ID>。
3. 每个任务步骤完成后等待用户审查。
不得修改其他文件。
```

### WF-001 生成计划和预览

- 优先级：P0
- 前置：Lead、Worker 可运行。

步骤：

1. 新建 Workflow 模式会话，选择测试成员并输入基准提示词。
2. 等待计划生成，记录计划卡、节点、负责人、依赖和校验信息。
3. 暂不执行，刷新页面并重新打开预览。

验收标准：

- 计划卡在超时内进入 `preview_ready` 或显示可解释的校验错误。
- 有且仅有 A、B 两个任务，B 依赖 A，负责人和目标文件正确。
- 刷新后计划内容、顺序和可执行状态保持。

### WF-002 依赖调度与成功闭环

- 优先级：P0
- 前置：WF-001 产生有效计划；仓库中不存在两个目标文件。

步骤：

1. 执行计划并持续记录各节点状态和开始/结束时间。
2. 确认 A 完成前 B 不进入运行。
3. 完成需要的用户审查，等待执行终态。
4. 核对两个文件内容、工作流卡和执行 transcript。

验收标准：

- 调度顺序遵守 B 依赖 A；不得提前或重复执行。
- 节点状态通过 reducer 合法流转，无倒退、跳态或永久运行。
- 文件内容分别为 `A:<RUN_ID>`、`B:<RUN_ID>`。
- 执行在用户最终验收后才进入 completed。

### WF-003 待用户输入

- 优先级：P1
- 前置：新建单独工作流。

步骤：

1. 提示 Lead 创建一个“必须先向用户询问验证码，再把验证码写入 `wf-input-<RUN_ID>.txt`”的步骤。
2. 执行并等待 pending input 卡和 Inbox 项。
3. 输入 `<RUN_ID>-INPUT` 并提交。
4. 等待步骤完成并核对文件。

验收标准：

- 未提交输入前步骤保持 waiting input，后续依赖不运行。
- Inbox 指向正确会话/步骤，输入只可提交一次。
- 提交后同一步恢复，文件内容包含正确验证码，pending 状态消失。

### WF-004 执行器权限审批

- 优先级：P0
- 前置：测试 Agent 的执行器支持审批请求。

步骤：

1. 创建一个需要执行安全、只作用于 `REG_REPO_A` 的文件读取/写入动作的步骤。
2. 第一次审批选择拒绝，观察步骤和日志。
3. 重试该步骤，第二次对同一范围选择允许。
4. 检查审批请求、Inbox、运行结果和审计信息。

验收标准：

- 待审批时执行暂停，未批准动作不得提前发生。
- 拒绝和允许只作用于对应 request/step，不串到其他运行。
- 拒绝结果可见且不可伪装成功；允许后可继续。
- 审批内容清晰显示动作和目标，敏感值不泄漏。

### WF-005 用户审查接受

- 优先级：P0
- 前置：存在 waiting review 步骤。

步骤：

1. 打开待审查卡，阅读结果、Diff 和 transcript。
2. 输入审查意见 `Accepted <RUN_ID>` 并选择接受。
3. 观察当前步骤、下游步骤和 Inbox。

验收标准：

- 接受前下游受审查门控约束；接受后只推进合法下游。
- 审查人、round、verdict 和反馈被持久化。
- 重复点击不会产生两条审查或重复调度。

### WF-006 审查拒绝、反馈与重试

- 优先级：P0
- 前置：创建一个新的 waiting review 步骤。

步骤：

1. 选择拒绝，填写 `what_wrong=<RUN_ID>-WRONG` 和 `expected=<RUN_ID>-EXPECTED`。
2. 验证空字段时提交被阻止；补全后提交。
3. 选择 retry/revise，等待新一轮运行和审查。
4. 接受修正结果。

验收标准：

- 拒绝必须包含明确反馈，空字段不提交。
- 拒绝记录属于正确 round，旧结果和反馈可追溯。
- 重试创建新 attempt/round，不覆盖旧 transcript。
- 只有接受修正结果后流程继续。

### WF-007 步骤 Stop、Retry 和 Skip

- 优先级：P0
- 前置：准备三个独立的可控步骤或三次独立执行。

步骤：

1. 在 running 步骤执行 Stop，确认对话框后等待终态。
2. 对可重试的失败/停止步骤执行 Retry。
3. 对 ready/failed/paused 步骤执行 Skip，记录下游行为。
4. 对不可跳过状态观察 Skip 按钮是否隐藏/禁用。

验收标准：

- Stop 只影响目标步骤；Retry 生成新 attempt 并保留旧记录。
- Skip 只在后端接受的状态可用，结果明确为 skipped。
- 下游依赖按照编译图和 skip 语义处理，无非法运行。
- 前端控件与后端接受状态一致，不出现 4xx 后仍显示成功。

### WF-008 执行 Stop、Resume 与手动 Complete

- 优先级：P0
- 前置：存在含多个步骤的 running 执行。

步骤：

1. Stop 整个执行并确认，记录运行中、待审查和 ready 步骤状态。
2. 对 paused/允许恢复的执行点击 Resume。
3. 构造一个 failed 或 paused 且无运行中步骤的执行，点击 Complete 并确认。
4. 刷新后读取执行卡、事件和 transcript。

验收标准：

- Stop 后不得继续自动调度；各步骤状态与服务端投影一致。
- Resume 只在可恢复状态出现且不会重复执行已完成步骤。
- Complete 只在合法状态且无运行中步骤时可用，并要求确认。
- 刷新后执行状态不回滚；完整事件/记录可追溯。

### WF-009 审查设置、持久化与 Inbox 聚焦

- 优先级：P1
- 前置：有有效计划或执行。

步骤：

1. 打开 Review settings，分别调整 lead review、user review 和 loop review。
2. 保存并刷新，重新打开核对。
3. 运行受影响步骤并验证实际审查门控。
4. 从对应 Inbox 项打开工作流。

验收标准：

- 设置按 step/loop 保存且刷新后保持。
- 实际审查行为与设置一致，不影响其他步骤。
- Inbox 打开后聚焦到正确的 input/review/approval，而不是只打开会话顶部。

## 14. 源码管理与隔离 worktree

### SCM-001 变更列表和作用域

- 优先级：P0
- 前置：CHAT-007 已创建未提交文件；项目 A、其他项目均可切换。

步骤：

1. 打开主会话 File Changes，刷新并记录所有文件。
2. 切换其他会话和项目，观察变更列表。
3. 返回主会话并与 `git -C "${REG_REPO_A}" status --short` 比较。

验收标准：

- UI 文件、状态和 Git 一致。
- 其他项目/不相关 worktree 的变更不得泄漏。
- `.openteams/` 和仓库外路径不得进入用户变更。

### SCM-002 Diff 查看和复用

- 优先级：P1
- 前置：有一个已跟踪修改和一个未跟踪文本文件。

步骤：

1. 打开已跟踪文件 Diff，核对增删行。
2. 打开未跟踪文件内容。
3. 再打开另一个 Diff，并切换回原 Diff。

验收标准：

- 路径、状态、行内容与工作区一致。
- Diff Tab 复用规则不会展示上一文件的旧内容。
- 二进制/不可预览内容显示明确提示而不是乱码或崩溃。

### SCM-003 Stage、Unstage、Stage All

- 优先级：P0
- 前置：至少两个测试变更，且无用户已有 staged 文件。

步骤：

1. Stage 单个文件并核对 Staged Changes。
2. Unstage 该文件并核对 Changes。
3. Stage All，再 Unstage All。
4. 每步执行 `git -C "${REG_REPO_A}" status --short` 对照。

验收标准：

- 单文件和批量操作只作用于展示的测试文件。
- UI 区域、计数和 Git index 实时一致。
- 不得触碰仓库外路径或其他会话 worktree。

### SCM-004 Commit

- 优先级：P0
- 前置：只 stage 本轮目标文件。

步骤：

1. 留空 commit message，确认 Commit 不可执行。
2. 输入 `test: <RUN_ID> source control` 并提交。
3. 检查提交列表和 `git log -1 --format=%s`。
4. 刷新 File Changes。

验收标准：

- 空消息无法提交。
- 只提交 staged 测试文件，提交信息一致。
- 成功后 staged 区清空、提交列表出现新提交，未 staged 变更保留。

### SCM-005 Discard 安全确认

- 优先级：P0
- 前置：创建 `discard-<RUN_ID>.txt` 和一个已跟踪测试修改。

步骤：

1. 对单文件 Discard，先取消并核对文件仍存在。
2. 再确认丢弃该测试文件。
3. 对 Discard All 打开确认但只在当前列表全部属于本轮时执行。
4. 对照磁盘和 Git 状态。

验收标准：

- 明确显示目标路径和不可恢复提示。
- 取消无副作用；确认只丢弃所选本轮文件。
- 任何路径超出解析后的 workspace 时操作必须被拒绝。

### SCM-006 隔离 worktree 懒创建与无冲突合并

- 优先级：P0
- 前置：SES-002 已创建隔离会话；主仓库干净。

步骤：

1. 在隔离会话首次运行 Worker，创建 `isolated-<RUN_ID>.txt`。
2. 验证 worktree 从 pending/creating 进入 active，记录 branch/path/base commit。
3. 在隔离会话 File Changes 中 stage 并 commit。
4. 点击 Merge 并确认，等待合并和清理。
5. 在主仓库验证文件和提交历史。

验收标准：

- worktree 只在首次运行需要时创建，路径与主工作区不同。
- 隔离修改在合并前不出现在主工作区。
- 合并后主工作区包含正确文件/提交，worktree 状态合法流转为 merged/cleaning/cleaned。
- 其他活跃或未合并 worktree 不被自动清理。

### SCM-007 合并冲突解决与中止

- 优先级：P0
- 前置：新建一个隔离会话；`conflict.txt` 初始一致。

步骤：

1. 在隔离 worktree 将 `conflict.txt` 改为 `worktree-<RUN_ID>` 并提交。
2. 在主仓库将同一行改为 `main-<RUN_ID>` 并提交。
3. 从隔离会话 Merge，打开冲突解决页并记录冲突路径。
4. 第一次选择 Abort，验证主仓库和 worktree 均可继续使用。
5. 再次 Merge，编辑为 `resolved-<RUN_ID>`、标记已解决并 Continue。

验收标准：

- 冲突状态、Inbox 和文件列表只包含仓库内相对路径。
- Abort 后主分支不保留半完成 merge，隔离提交仍可恢复。
- Continue 在全部冲突解决前不可执行。
- 解决后主仓库内容为 `resolved-<RUN_ID>`，合并记录和状态完整。

### SCM-008 丢弃隔离 worktree 与清理保护

- 优先级：P0
- 前置：新建一个带未合并测试提交的隔离会话。

步骤：

1. 点击 Discard/Delete worktree，先取消并验证数据仍在。
2. 再次确认前核对目标会话、分支和路径均包含本轮测试标识。
3. 确认丢弃，等待清理；检查主仓库。
4. 同时确认其他活动 worktree 未被清理。

验收标准：

- 取消无副作用，确认前有“未合并内容会丢失”的明确警告。
- 确认后仅目标 worktree 被删除，主仓库未获得其未合并文件。
- 自动清理绝不删除未合并、active 或 conflicted 的其他 worktree。

## 15. Issues

### ISS-001 创建和编辑本地 Issue

- 优先级：P0
- 前置：项目 A 已选择。

步骤：

1. 创建 `<RUN_ID>-issue-local`，填写描述、优先级和初始状态。
2. 打开详情，修改标题、描述和优先级。
3. 刷新并重新打开。

验收标准：

- Issue 只属于项目 A，创建一次。
- 编辑字段保存并持久化，列表卡和详情一致。
- Issue ID 稳定，不因编辑重复创建。

### ISS-002 状态流转、视图和过滤

- 优先级：P1
- 前置：至少创建三个不同状态的本轮 Issue。

步骤：

1. 将测试 Issue 依次流转到 In Progress、Ready to Merge、Done。
2. 验证列表/看板位置和状态菜单。
3. 使用 All、Active、Backlog 等过滤入口。
4. 清除过滤并刷新。

验收标准：

- 状态变更立即反映在详情和列表，刷新后保持。
- 每个过滤结果只包含符合定义的 Issue，无重复或漏项。
- 清除过滤恢复全部本项目 Issue。

### ISS-003 从 Issue 创建自由聊天会话

- 优先级：P1
- 前置：ISS-001 通过。

步骤：

1. 从 Issue 详情创建/关联 Free chat 会话。
2. 打开新会话并检查预填消息、项目、工作区和关联信息。
3. 从会话返回 Issue，再从 Issue 打开关联会话。

验收标准：

- 会话创建在项目 A，工作区为项目 A 的解析路径。
- Issue 与 session/run 链接双向可见且指向稳定 ID。
- 预填内容包含 Issue 上下文但不泄漏其他 Issue。

### ISS-004 从 Issue 创建工作流会话

- 优先级：P1
- 前置：工作流成员可用。

步骤：

1. 从 Issue 详情选择 Workflow 模式创建会话。
2. 核对预填工作流提示、成员和工作区。
3. 生成计划并至少执行到 preview 或第一步。
4. 回到 Issue 查看 workflow/session/run/step 链接。

验收标准：

- 创建模式确为 workflow，Issue 上下文进入计划。
- 执行实体与 Issue 链接完整，刷新后仍可导航。
- 用户仍保有最终验收控制；Agent 不得自行删除或改写 Issue 路线图。

### ISS-005 删除本地 Issue

- 优先级：P1
- 前置：创建 `<RUN_ID>-issue-delete` 且无后续依赖。

步骤：

1. 发起删除并取消，确认 Issue 保留。
2. 再次确认删除。
3. 搜索/过滤并刷新验证。

验收标准：

- 删除确认显示正确标题，取消无副作用。
- 确认后 Issue 不再出现；其他 Issue 和关联会话不被误删。

## 16. 可选集成与设置

### INT-001 GitHub 授权、仓库连接和 Issue 导入

- 优先级：P2
- 前置：测试专用 GitHub 账号和仓库；缺失时允许 `SKIPPED`。

步骤：

1. 从 Issues 打开外部仓库连接，完成 OAuth；弹窗不可用时验证 device flow。
2. 选择测试仓库并连接。
3. 导入一个标题含 `RUN_ID` 的 GitHub Issue。
4. 刷新并核对来源、标题、状态、标签和远端链接。
5. 断开测试仓库连接但不删除远端数据。

验收标准：

- 授权账号和目标仓库清晰可见，token 不出现在 UI/日志。
- 连接只作用于项目 A，导入不会重复创建同一 GitHub Issue。
- 本地缓存字段与远端一致，断开后本地 Issue 仍安全可见或给出明确契约行为。

### INT-002 GitHub PR 预览、Push 和创建

- 优先级：P2
- 前置：INT-001 通过；测试分支有本轮提交；缺失时允许 `SKIPPED`。

步骤：

1. 选择 repo integration、源分支和目标分支，生成 PR preview。
2. 核对将要 push 的提交、标题和描述。
3. Push 测试分支并创建 PR。
4. 记录 PR URL 和审计记录；若创建失败，使用 Retry 一次。

验收标准：

- Preview 不产生远端写入，且目标仓库/分支正确。
- Push/Create 只作用于测试仓库和测试分支。
- 成功后返回可访问 PR 和审计记录；失败显示真实错误，Retry 不重复创建 PR。

### INT-003 Settings、Provider、Agent runtime 和技能目录

- 优先级：P1
- 前置：有测试 Provider；不得修改用户已有 Provider。

步骤：

1. 在 Settings 查看/新增带 `RUN_ID` 的测试 Provider，保存后重新打开。
2. 使用该 Provider 完成一次最小聊天运行，再编辑一个非敏感字段。
3. 打开 Agent runtime，核对对应 runner 的可用性、版本、命令来源和配置详情，执行刷新。
4. 在 Members 中编辑 Worker，打开 Skills 配置，浏览该 runtime 的已安装技能并只读打开一个详情。
5. 删除本轮测试 Provider；不得删除已有 Provider 或全局技能。

验收标准：

- Provider 配置保存并可被真实运行使用；秘密字段不以明文回显。
- Agent runtime 状态与实际可运行性一致，刷新不产生重复条目。
- 技能列表和详情可加载，作用域/安装状态一致。
- 清理只删除本轮 Provider，其他配置不变。

## 17. 统计、外观与持久化

### STA-001 Build Statistics 汇总

- 优先级：P1
- 前置：CHAT-001、CHAT-007、WF-002 已产生运行和 token/成本数据。

步骤：

1. 打开 Build Statistics，选择包含本轮运行的时间范围。
2. 核对 Total tokens、Model cost、会话用量、模型用量和交付统计。
3. 与运行详情/API 返回进行抽样比对。
4. 切换项目再返回。

验收标准：

- 统计只包含所选项目和时间范围，不出现跨项目泄漏。
- 总计等于明细可解释汇总；估算值和真实值的标识符合契约。
- 切换项目后数据刷新，不沿用上一项目缓存。

### STA-002 工作流统计下钻

- 优先级：P1
- 前置：WF-002 完成且有 step token usage。

步骤：

1. 从会话用量进入工作流步骤下钻。
2. 核对 step title/key、Agent、input/output/total token、cost 和 run 数。
3. 返回会话统计，再次进入同一工作流。

验收标准：

- 下钻只展示目标 workflow/session 的步骤。
- 步骤合计与会话工作流合计一致，返回导航不丢失筛选。
- 无用量时显示明确空状态，不伪造 0 成本为真实数据。

### STA-003 外观、语言、快捷键设置与重启持久化

- 优先级：P1
- 前置：Settings 可用。

步骤：

1. 切换深色、浅色、跟随系统主题并观察可读性。
2. 修改消息字号和页面语言，检查 Workspace、Issues、Settings 三个页面。
3. 修改一个不冲突的快捷键并验证生效，再恢复默认。
4. 刷新并重启应用，核对最终设置。

验收标准：

- 主题切换无不可读文字、透明遮挡或布局崩坏。
- 语言和字号作用于预期范围，缺失翻译有合理 fallback 而不是 key。
- 快捷键冲突被阻止或明确提示，恢复默认有效。
- 最终设置在刷新和重启后保持，且不影响运行数据。

## 18. 清理

1. 停止所有本轮运行和工作流，确保没有 queued/running/merging 状态。
2. 通过 UI 删除或归档名称含 `RUN_ID` 的测试 Issue、会话、成员、团队模板、Provider、CLI 变体和项目。
3. 不得删除项目 A，直到所有 Git、worktree 和统计证据采集完成。
4. 对 `REG_REPO_A`、`REG_REPO_B`、`REG_CLI_REPO` 和 worktree 做最后只读检查，记录未清理项。
5. 恢复 CLI-203 使用的测试 MCP/Skill 配置；只删除本轮新增项，不覆盖各 CLI 原有配置文件。
6. 只有在路径已打印、路径非空、路径位于系统临时目录、且用户明确批准后，才可删除 `REG_ROOT`。
7. 不得通过覆盖 `dev_assets` 或删除数据库的方式清理测试数据。
8. 清理行为和遗留物必须写入报告。

## 19. 报告要求

最终报告必须基于 `docs/qa/regression-test-report-template.md`，并满足：

1. 包含本手册全部 72 个用例，且每个用例恰好一条最终结果。
2. 每条记录包含优先级、开始/结束时间、实际结果、验收判断、证据路径和缺陷编号。
3. 自动化门禁记录完整命令、退出码、总数和失败栈。
4. 所有 `FAIL`、`BLOCKED`、`SKIPPED`、重试通过和疑似 flaky 用例都有详细解释。
5. 缺陷包含复现步骤、预期、实际、严重度、影响范围和证据。
6. 汇总 P0/P1/P2 与 PASS/FAIL/BLOCKED/SKIPPED/NOT_RUN 数量，数字与全用例表一致。
7. 明确给出 `PASS`、`CONDITIONAL PASS` 或 `FAIL`，不得只写“整体正常”。
8. 报告结论必须列出残余风险、未覆盖项和清理状态。

## 20. 手册维护触发条件

出现以下任一变更时，提交代码的人必须同步评估并更新本手册与报告模板：

- 新增或删除主导航、会话模式、工作流状态或用户控制。
- 修改 workflow reducer、worktree 状态机、源码工作区解析或安全边界。
- 修改项目/Issue/GitHub 的关联模型。
- 修改 Agent 执行、队列、审批、附件、运行记录或统计口径。
- 新增、删除或重命名生产 `BaseCodingAgent`，或修改任一 CLI 的启动命令、会话、ACP、MCP、Skill、模型和认证适配。
- 新增测试命令、测试夹具或发布门禁。
