---
title: "OpenTeams 功能回归测试报告模板"
description: "与 OpenTeams 功能回归测试操作手册逐用例对应的详细报告模板。"
---

# OpenTeams 功能回归测试报告

> 使用前复制本文件到 `qa_test/<RUN_ID>/report.md`。不得删除用例行；未执行项必须保留并填写原因。

## 1. 执行摘要

| 字段 | 值 |
| --- | --- |
| RUN_ID | `<RUN_ID>` |
| 待测版本/应用版本 |  |
| 比较基线版本/Commit |  |
| 分支 / Commit SHA |  |
| 测试开始时间 |  |
| 测试结束时间 |  |
| 执行 Agent |  |
| 测试环境 |  |
| 报告结论 | `PASS / CONDITIONAL PASS / FAIL` |
| 结论理由 |  |

### 核心结论

- 完成情况：
- 阻断问题：
- 功能衰退判断：
- 发布建议：

## 2. 环境与基线

| 项目 | 实际值 |
| --- | --- |
| 操作系统/架构 |  |
| 浏览器及版本 |  |
| Node.js |  |
| pnpm |  |
| rustc/cargo |  |
| 前端地址 |  |
| 后端地址 |  |
| 测试 Provider/Agent |  |
| REG_ROOT |  |
| REG_REPO_A |  |
| REG_REPO_B |  |
| 测试前 `git status --short` |  |
| 测试后 `git status --short` |  |

### 基线差异

说明测试开始前已有的修改、已知故障、环境限制和批准的例外：

## 3. 汇总统计

### 3.1 按结果

| 结果 | 数量 |
| --- | ---: |
| PASS |  |
| FAIL |  |
| BLOCKED |  |
| SKIPPED |  |
| NOT_RUN |  |
| 合计 | 56 |

### 3.2 按优先级

| 优先级 | 总数 | PASS | FAIL | BLOCKED | SKIPPED | NOT_RUN |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| P0 | 29 |  |  |  |  |  |
| P1 | 25 |  |  |  |  |  |
| P2 | 2 |  |  |  |  |  |

### 3.3 缺陷

| 严重度 | 新增 | 已知复现 | 已解决并验证 | 未解决 |
| --- | ---: | ---: | ---: | ---: |
| S1 |  |  |  |  |
| S2 |  |  |  |  |
| S3 |  |  |  |  |
| S4 |  |  |  |  |

## 4. 自动化门禁

| 用例 | 命令 | 退出码 | 通过/总数 | 耗时 | 日志 | 结果 |
| --- | --- | ---: | --- | --- | --- | --- |
| PRE-001 | `pnpm install --frozen-lockfile` |  |  |  |  |  |
| PRE-002 | `pnpm run format:check` |  |  |  |  |  |
| PRE-002 | `pnpm run frontend:check` |  |  |  |  |  |
| PRE-002 | `pnpm run backend:lint` |  |  |  |  |  |
| PRE-002 | `pnpm run generate-types:check` |  |  |  |  |  |
| PRE-002 | `pnpm run prepare-db:check` |  |  |  |  |  |
| PRE-003 | `pnpm run frontend:test` |  |  |  |  |  |
| PRE-003 | `cargo test --workspace --features qa-mode` |  |  |  |  |  |
| PRE-004 | `pnpm run frontend:build` |  |  |  |  |  |
| PRE-004 | `pnpm dev` + `/api/info` |  |  |  |  |  |

### 自动化异常与重跑

记录首次失败、重跑原因、重跑结果和 flaky 判断。不得只保留最终成功：

## 5. 全用例结果

`实际结果`必须写观察到的事实，不能只写“符合预期”。`证据`使用相对报告目录的可点击路径。

| 用例 ID | P | 结果 | 开始/结束 | 实际结果与验收判断 | 证据 | 缺陷 |
| --- | --- | --- | --- | --- | --- | --- |
| PRE-001 | P0 | `NOT_RUN` |  |  |  |  |
| PRE-002 | P0 | `NOT_RUN` |  |  |  |  |
| PRE-003 | P0 | `NOT_RUN` |  |  |  |  |
| PRE-004 | P0 | `NOT_RUN` |  |  |  |  |
| NAV-001 | P0 | `NOT_RUN` |  |  |  |  |
| NAV-002 | P1 | `NOT_RUN` |  |  |  |  |
| NAV-003 | P1 | `NOT_RUN` |  |  |  |  |
| NAV-004 | P1 | `NOT_RUN` |  |  |  |  |
| PRJ-001 | P0 | `NOT_RUN` |  |  |  |  |
| PRJ-002 | P1 | `NOT_RUN` |  |  |  |  |
| PRJ-003 | P0 | `NOT_RUN` |  |  |  |  |
| PRJ-004 | P1 | `NOT_RUN` |  |  |  |  |
| SES-001 | P0 | `NOT_RUN` |  |  |  |  |
| SES-002 | P0 | `NOT_RUN` |  |  |  |  |
| SES-003 | P1 | `NOT_RUN` |  |  |  |  |
| SES-004 | P1 | `NOT_RUN` |  |  |  |  |
| MEM-001 | P0 | `NOT_RUN` |  |  |  |  |
| MEM-002 | P1 | `NOT_RUN` |  |  |  |  |
| MEM-003 | P1 | `NOT_RUN` |  |  |  |  |
| MEM-004 | P1 | `NOT_RUN` |  |  |  |  |
| MEM-005 | P1 | `NOT_RUN` |  |  |  |  |
| CHAT-001 | P0 | `NOT_RUN` |  |  |  |  |
| CHAT-002 | P1 | `NOT_RUN` |  |  |  |  |
| CHAT-003 | P1 | `NOT_RUN` |  |  |  |  |
| CHAT-004 | P1 | `NOT_RUN` |  |  |  |  |
| CHAT-005 | P0 | `NOT_RUN` |  |  |  |  |
| CHAT-006 | P0 | `NOT_RUN` |  |  |  |  |
| CHAT-007 | P0 | `NOT_RUN` |  |  |  |  |
| WF-001 | P0 | `NOT_RUN` |  |  |  |  |
| WF-002 | P0 | `NOT_RUN` |  |  |  |  |
| WF-003 | P1 | `NOT_RUN` |  |  |  |  |
| WF-004 | P0 | `NOT_RUN` |  |  |  |  |
| WF-005 | P0 | `NOT_RUN` |  |  |  |  |
| WF-006 | P0 | `NOT_RUN` |  |  |  |  |
| WF-007 | P0 | `NOT_RUN` |  |  |  |  |
| WF-008 | P0 | `NOT_RUN` |  |  |  |  |
| WF-009 | P1 | `NOT_RUN` |  |  |  |  |
| SCM-001 | P0 | `NOT_RUN` |  |  |  |  |
| SCM-002 | P1 | `NOT_RUN` |  |  |  |  |
| SCM-003 | P0 | `NOT_RUN` |  |  |  |  |
| SCM-004 | P0 | `NOT_RUN` |  |  |  |  |
| SCM-005 | P0 | `NOT_RUN` |  |  |  |  |
| SCM-006 | P0 | `NOT_RUN` |  |  |  |  |
| SCM-007 | P0 | `NOT_RUN` |  |  |  |  |
| SCM-008 | P0 | `NOT_RUN` |  |  |  |  |
| ISS-001 | P0 | `NOT_RUN` |  |  |  |  |
| ISS-002 | P1 | `NOT_RUN` |  |  |  |  |
| ISS-003 | P1 | `NOT_RUN` |  |  |  |  |
| ISS-004 | P1 | `NOT_RUN` |  |  |  |  |
| ISS-005 | P1 | `NOT_RUN` |  |  |  |  |
| INT-001 | P2 | `NOT_RUN` |  |  |  |  |
| INT-002 | P2 | `NOT_RUN` |  |  |  |  |
| INT-003 | P1 | `NOT_RUN` |  |  |  |  |
| STA-001 | P1 | `NOT_RUN` |  |  |  |  |
| STA-002 | P1 | `NOT_RUN` |  |  |  |  |
| STA-003 | P1 | `NOT_RUN` |  |  |  |  |

## 6. 非通过用例和重试明细

为每个 `FAIL`、`BLOCKED`、`SKIPPED`、首次失败后重试通过或疑似 flaky 的用例复制以下段落。若没有，写“无”。

### `<CASE_ID>` — `<标题>`

- 最终结果：
- 首次结果：
- 开始/结束时间：
- 执行到的步骤：
- 未执行步骤：
- 前置条件：
- 实际结果：
- 未满足的验收标准：
- 重试次数及结果：
- 根因或阻塞原因：
- 缺陷编号：
- 证据：
- 对后续用例的影响：

## 7. 缺陷清单

| 缺陷 ID | 严重度 | 标题 | 关联用例 | 状态 | 是否阻断发布 |
| --- | --- | --- | --- | --- | --- |
|  |  |  |  |  |  |

### 缺陷详情模板

#### `<DEFECT_ID>` — `<标题>`

- 严重度：
- 首次发现版本：
- 关联用例：
- 前置条件：
- 最小复现步骤：
  1.
  2.
  3.
- 预期结果：
- 实际结果：
- 复现率：
- 影响范围：
- 临时绕过：
- 控制台/网络/服务端错误：
- 证据：

## 8. 功能衰退分析

### 与基线相比的新增问题

| 功能面 | 基线行为 | 当前行为 | 是否衰退 | 证据/缺陷 |
| --- | --- | --- | --- | --- |
|  |  |  |  |  |

### 通过但存在风险

列出偶现失败、性能明显变慢、只有单一平台验证、依赖真实模型不确定性等风险：

### 未覆盖或合规跳过

| 用例 | 原因 | 前置条件缺口 | 补测计划 | 风险接受人 |
| --- | --- | --- | --- | --- |
|  |  |  |  |  |

## 9. 证据索引

| 证据路径 | 类型 | 关联用例 | 内容说明 | 是否脱敏 |
| --- | --- | --- | --- | --- |
|  |  |  |  |  |

## 10. 清理记录

| 测试对象 | 预期清理动作 | 实际状态 | 证据/备注 |
| --- | --- | --- | --- |
| 测试项目/会话 | 仅清理名称含 RUN_ID 的对象 |  |  |
| 测试成员/模板 | 仅清理名称含 RUN_ID 的对象 |  |  |
| 测试 Provider | 删除本轮创建项 |  |  |
| GitHub 测试分支/PR | 按测试账号策略处理 |  |  |
| worktree | 无 running/active/conflicted 遗留 |  |  |
| REG_ROOT | 获得批准后再删除 |  |  |
| 用户既有数据 | 未修改/未删除 |  |  |

## 11. 最终结论

### 判定

`PASS / CONDITIONAL PASS / FAIL`

### 判定依据

1.
2.
3.

### 发布建议

- 建议：
- 必须修复：
- 建议补测：
- 残余风险：
- 负责人/批准人：
