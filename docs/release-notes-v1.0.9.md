# v1.0.9 发布说明

## 新功能
- **Agent 安装引导与 OAuth 状态检测**：为 Claude、Codex、Copilot、Cursor、Droid、Gemini、Kimi、OpenCode、Qwen、Amp 等执行器新增安装状态通知与 OAuth 登录态检查，前端提供 AgentInstallGuide 组件与系统终端打开能力
- **模型发现机制重构**：除 Codex、Claude Code、OpenCode 外的执行器改用原生命令进行模型发现，统一 command 构建逻辑并扩展 agent_runtime 支持
- **聊天活动日志重设计**：重新设计聊天 Agent 活动日志以提升可读性，过滤框架内部噪音行，清理命令详情（剥离 shell 包装与 cd 前缀），用状态图标替代重复文本，折叠连续工具调用为聚合摘要行，显示每行耗时
- **活动日志 Markdown 渲染**：连续散文行合并为块，使用共享 AgentMarkdown 渲染器（支持粗体、斜体、行内代码、列表、链接、GFM）
- **工作流详情与日志 UI 格式化**：格式化工作流详情 UI 与日志 UI，启用 Kimi 与 OpenCode 权限请求命令详情展示

## 改进
- **Kimi 与 OpenCode 权限审批详情**：支持展示 Kimi 与 OpenCode 权限请求的工具描述详情，改进 ACP 客户端与 executor_approvals 审批展示逻辑
- **OpenTeams CLI 永久允许**：openteams-cli SDK 支持 always allow 权限模式

## Bug 修复
- **执行器超时配置**：调整所有执行器的超时时间，修复 OpenCode 与 OpenTeams CLI SDK 超时问题
- **命令提示翻译键**：修复命令提示翻译键错误
- **Gemini 模型替换**：将 gemini-3-pro-preview 替换为 gemini-3-flash-preview

## 包含的提交
- `2ddf261d` Fix: replace gemini-3-pro-preview model to gemni-3-flash-preview
- `8d727bf9` fix: command hint translate key
- `e1e3f895` Fix: change the timeout of all executors
- `68850500` feat: redesign chat agent activity log for readability
- `5efa185f` feat: render activity log prose with shared markdown renderer
- `56e3e830` Fix: support display Kimi and opencode permission request detail tool description
- `7a4a53a8` feat: 1. format workflow detail ui and log ui; 2. enable kimi and opencode permission request command detail showup
- `b92de0d2` fix: support openteams-cli always allow
- `5754b64c` feat: support install notify and oauth status checking
- `7aab7f66` Fix: change model discovery logic and use native command except codex, cc and opencode
