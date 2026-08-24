---
title: "Chat Delivery 端到端验收矩阵"
description: "消息投递、队列、运行态与恢复协议的发布阻断验收。"
---

# Chat Delivery 端到端验收矩阵

本矩阵验证每个 `chat_message × session_agent` 的持久 delivery 生命周期。它是消息投递重构的 P0 发布门禁：只有全部 11 项为 `PASS`，才能报告该功能通过；`FAIL`、`BLOCKED`、`SKIPPED` 或 `NOT_RUN` 均为未通过。

## 执行方式与证据

实现完成后，从仓库根目录执行：

```bash
bash scripts/run-chat-delivery-acceptance.sh
```

脚本会在 `qa_test/chat-delivery/<UTC 时间戳>/` 写入：

- `report.md`：每个用例的步骤、预期、实际结果、命令、日志和证据路径；
- `summary.json`：机器可读结论；
- `logs/<CASE_ID>.log`：完整 stdout/stderr（包括 fixture 的事件、快照和数据库断言）。

脚本依赖两个真正执行行为断言的测试入口，缺任一入口或任一目标用例都会标为 `BLOCKED`，而不是静默跳过：

| 层 | 入口 | 责任 |
| --- | --- | --- |
| 服务端跨层 fixture | `crates/services/tests/chat_delivery_e2e.rs` | 持久化 delivery、调度、runner、恢复和事件 outbox |
| 前端运行态 fixture | `frontend/src/context/workspace/chatDeliveryRuntime.acceptance.test.ts` | snapshot/delta reducer、刷新、切 session、WS 重连与投影 |

前端入口必须接受 `--case <CASE_ID>`，后端测试函数名必须与下面“自动化目标”列完全相同。每个测试在成功时应输出结构化事件/快照或数据库行；这些 stdout 内容是本次执行的原始证据，不能仅输出“passed”。

## 判定规则

1. 测试必须使用临时数据库和可控 fake executor；不得依赖本机已认证 Agent、真实网络或固定时间。
2. 每项都要同时断言持久化 delivery、runtime snapshot/revision 和前端可见投影（适用时）。只断言 HTTP 200 或按钮存在不算通过。
3. 重复事件、过期 snapshot、重试请求及恢复流程必须保留同一 `delivery_id`，且不会产生第二个执行。
4. 失败时保留第一轮日志；可以重跑诊断，但最终报告必须写明首次失败和所有重跑结果。
5. 后端 event/snapshot 所用 revision 必须单调递增；客户端不得应用小于或等于已应用 revision 的状态。

## 用例

| ID | 场景与步骤 | 预期结果 | 自动化目标 |
| --- | --- | --- | --- |
| CDD-001 | 空闲成员发送一条定向消息；fake executor 在创建 run 后暂停；读取 delivery/runtime；释放 executor 并读取最终消息。 | 同一 `delivery_id` 依次为 `starting`、`running(run_id 已绑定)`、`completed`；最终消息存在，成员回到 idle，前端卡片不再显示为 active。 | `delivery_idle_send_transitions_starting_running_final` |
| CDD-002 | 向同一忙碌成员连续发送 A、B、C；完成 A 后依次释放 B、C。 | A 为 in-flight，B/C 为 `queued`；claim 顺序严格为 A→B→C，任一时刻最多一个 in-flight，三个 delivery 均唯一。 | `delivery_busy_member_queues_fifo` |
| CDD-003 | 一条消息同时定向 Alpha、Beta；分别完成 Alpha，再让 Beta 失败或继续运行。 | 为每个目标创建独立 delivery/run；Alpha 的终态不会更改 Beta 的队列、run、卡片或最终输出。 | `delivery_multi_agent_targets_are_independent` |
| CDD-004 | 在 `starting` 与 `running` 各执行一次页面刷新和 session A→B→A 切换；以新旧 snapshot 交错返回。 | 同一持久 delivery 卡片在回到 A 后可见且保持正确状态/run；较旧 snapshot 不得降级或清空较新 revision，过程活动仍关联正确 delivery。 | 前端入口 `--case CDD-004` |
| CDD-005 | WS 断连；期间产生 delta；重连后交付重复 delta、乱序 delta 和一个有 revision 缺口的 delta。通过实际生产 resync scheduler/mock `chatRuntimeApi.getSnapshot` 观察 hydration；令首次请求失败并推进可控定时器，再返回完整 snapshot。 | 客户端恰好收敛到最新 revision；重复/旧事件无副作用；同一 session 的重复缺口在首次请求 in-flight 时只能发起一次 snapshot 请求；首次失败必须保持 `needsResync` 并在 backoff 后自动重试，成功回包才清除标记和收敛。只手动 dispatch snapshot 不构成通过。 | 前端入口 `--case CDD-005` |
| CDD-006 | run 未终结时 fake executor 发送一条或多条 agent 过程/协作消息，再检查 delivery 和活动卡片，最后终结 run。 | 每条中间 `message_new` 可显示为普通消息，但不会完成/删除仍为 `running` 的 delivery；只有终态 transition 才移除 active 卡片。 | `delivery_intermediate_agent_send_does_not_finalize_run` |
| CDD-007 | 同一 `client_message_id` 的发送请求在服务端提交后故意令首个响应超时；客户端原样重试。 | 返回原 `chat_message_id` 和全部原 delivery；消息、目标、队列行、run 与 outbox 均没有重复。 | `delivery_send_retry_is_idempotent` |
| CDD-008 | 分别在 `starting`（run 未绑定）和 `running`（run 已绑定）通过 `ChatRunner::stop_agent` 请求停止；在运行态以可控 fake executor 栅栏并发触发 stop 与 finalize；另构造有 run control 但无 durable delivery 的异常 member。 | 停止只终结对应 delivery/run，状态变为合法停止终态，不遗留 processing/running lease；竞态双方只能有一个有效终态提交，不能重复 final message、run、outbox revision 或启动被取消工作，成员最终可调度。无 delivery 的异常 member 必须收敛/恢复，不能走兼容分支直接写 `Stopping`。直接调用 delivery service CAS 不构成通过。 | `delivery_stop_is_safe_for_starting_and_running` + runner 尾项门禁 |
| CDD-009 | A 运行失败且 B 已排队；观察 blocked；执行 continue；释放 B；另模拟 pre-bind/dispatch 失败。 | A 记录失败原因并阻塞 B；continue 只设置 `failure_resolved_at`，不得把终态 `failed` 重写为 `skipped`，随后启动 B；B 完成后队列恢复可用，不能重复 claim。pre-bind 或 dispatch CAS 后变为 `Failed`/`Skipped` 的 delivery 必须持久保存非空 `failure_reason`。 | `delivery_failure_blocks_continue_and_starts_next` + runner 尾项门禁 |
| CDD-010 | 成员忙碌时排入 B、C；删除 B；让当前项完成。再尝试删除当前 in-flight 项。 | 仅 B 被删除，C 仍按序启动；删除 in-flight 返回冲突且不改 delivery/message；共享源消息有其他 target 时不得被误删。 | `delivery_delete_removes_only_queued` |
| CDD-011 | 分别在 claim 后、run 绑定后、finalize 提交边界模拟进程终止；重建 `ChatRunner` 并实际调用 `recover_orphaned_session_agents`。 | 每个 delivery 恢复为一次且仅一次可执行/已终结状态；无孤儿 in-flight、无重复 delivery/run/最终消息，revision/outbox 可重新水合。仅重建或查询 service 而未进入 runner recovery 不构成通过。 | `delivery_recovers_claim_bind_and_finalize_boundaries` |

## 实现约束

后端 fixture 应复用 ACP QA 的临时 SQLite + fake executor 模式，而不是 mock 掉 delivery service 或 runner。前端 fixture 应直接喂入 versioned snapshot/delta 与重连时序，不允许以源码字符串匹配代替状态断言。

运行报告初始状态必须是 `NOT_RUN`。本矩阵本身不宣告通过；只接受报告目录中 11 个独立命令退出码均为 0 的 `PASS` 结论。
