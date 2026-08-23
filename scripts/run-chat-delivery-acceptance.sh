#!/usr/bin/env bash
# Runs the P0 chat-delivery acceptance matrix and writes an auditable report.
# It deliberately treats missing test targets as BLOCKED, never as a pass.

set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
report_dir=""

usage() {
  printf '%s\n' "Usage: bash scripts/run-chat-delivery-acceptance.sh [--report-dir <path>]"
}

while (($# > 0)); do
  case "$1" in
    --report-dir)
      if (($# < 2)); then
        usage >&2
        exit 2
      fi
      report_dir="$2"
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      usage >&2
      exit 2
      ;;
  esac
done

if [[ -z "$report_dir" ]]; then
  report_dir="$repo_root/qa_test/chat-delivery/$(date -u +%Y%m%dT%H%M%SZ)"
elif [[ "$report_dir" != /* ]]; then
  report_dir="$repo_root/$report_dir"
fi

log_dir="$report_dir/logs"
mkdir -p "$log_dir"

backend_test="$repo_root/crates/services/tests/chat_delivery_e2e.rs"
frontend_test="$repo_root/frontend/src/context/workspace/chatDeliveryRuntime.acceptance.test.ts"
report_path="$report_dir/report.md"
summary_path="$report_dir/summary.json"

case_ids=(
  CDD-001 CDD-002 CDD-003 CDD-004 CDD-005 CDD-006 CDD-007 CDD-008 CDD-009 CDD-010 CDD-011
)
case_titles=(
  "空闲发送：starting→running→final"
  "同成员繁忙：排队与 FIFO"
  "多 Agent：delivery 相互独立"
  "刷新与切换 session：运行态可恢复"
  "WS 重连、重复、乱序与缺口收敛"
  "中间 Agent 消息不终结 run"
  "发送超时重试：消息与 delivery 幂等"
  "starting/running 停止"
  "失败阻塞、continue 与下一条启动"
  "删除 queued"
  "claim、run 绑定、finalize 边界恢复"
)
case_targets=(
  delivery_idle_send_transitions_starting_running_final
  delivery_busy_member_queues_fifo
  delivery_multi_agent_targets_are_independent
  frontend:CDD-004
  frontend:CDD-005
  delivery_intermediate_agent_send_does_not_finalize_run
  delivery_send_retry_is_idempotent
  delivery_stop_is_safe_for_starting_and_running
  delivery_failure_blocks_continue_and_starts_next
  delivery_delete_removes_only_queued
  delivery_recovers_claim_bind_and_finalize_boundaries
)

status_for_case=()
actual_for_case=()
command_for_case=()
frontend_check_status="NOT_RUN"
frontend_check_actual="尚未执行"
runner_tail_check_status=()
runner_tail_check_actual=()
runner_tail_check_id=(
  "runner-prebind-failure-reason"
  "runner-no-delivery-stop-recovery"
)
runner_tail_check_target=(
  "services::chat_runner::tests::prebind_configuration_failure_finalizes_claimed_delivery"
  "services::chat_runner::tests::stop_agent_with_control_but_without_delivery_recovers_to_idle"
)

run_case() {
  local index="$1"
  local case_id="${case_ids[$index]}"
  local target="${case_targets[$index]}"
  local log_path="$log_dir/$case_id.log"
  local started_at
  local exit_code

  started_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  if [[ "$target" == frontend:* ]]; then
    command_for_case[$index]="pnpm -C frontend exec tsx src/context/workspace/chatDeliveryRuntime.acceptance.test.ts --case $case_id"
    if [[ ! -f "$frontend_test" ]]; then
      status_for_case[$index]="BLOCKED"
      actual_for_case[$index]="缺少前端运行态验收入口：$frontend_test"
      printf '%s\n' "[$started_at] BLOCKED: ${actual_for_case[$index]}" > "$log_path"
      return
    fi
    if (
      cd "$repo_root"
      pnpm -C frontend exec tsx src/context/workspace/chatDeliveryRuntime.acceptance.test.ts --case "$case_id"
    ) > "$log_path" 2>&1; then
      exit_code=0
    else
      exit_code=$?
    fi
  else
    command_for_case[$index]="cargo test -p services --features qa-mode --test chat_delivery_e2e $target -- --exact --nocapture"
    if [[ ! -f "$backend_test" ]]; then
      status_for_case[$index]="BLOCKED"
      actual_for_case[$index]="缺少服务端跨层验收入口：$backend_test"
      printf '%s\n' "[$started_at] BLOCKED: ${actual_for_case[$index]}" > "$log_path"
      return
    fi
    if (
      cd "$repo_root"
      cargo test -p services --features qa-mode --test chat_delivery_e2e "$target" -- --exact --nocapture
    ) > "$log_path" 2>&1; then
      exit_code=0
    else
      exit_code=$?
    fi
  fi

  if ((exit_code == 0)); then
    status_for_case[$index]="PASS"
    actual_for_case[$index]="自动化目标退出码为 0；原始事件、快照和断言见日志。"
  else
    status_for_case[$index]="FAIL"
    actual_for_case[$index]="自动化目标退出码为 ${exit_code}；请根据日志中的首个断言或运行错误创建 blocker。"
  fi
}

for index in "${!case_ids[@]}"; do
  run_case "$index"
done

# A passing standalone TypeScript fixture cannot validate a production module
# that fails to type-check. Keep this as a separate hard gate so the report
# never advertises a shippable 11/11 result while the frontend is broken.
if (
  cd "$repo_root"
  pnpm run frontend:check
) > "$log_dir/frontend-check.log" 2>&1; then
  frontend_check_status="PASS"
  frontend_check_actual="pnpm run frontend:check 退出码为 0。"
else
  frontend_check_status="FAIL"
  frontend_check_actual="pnpm run frontend:check 非 0；完整诊断见 frontend-check.log。"
fi

# These runner-entry regressions exercise paths that are not an extra user
# scenario, but are mandatory safety properties of CDD-008/009. Keep their
# own logs and make either failure release-blocking.
for index in "${!runner_tail_check_id[@]}"; do
  tail_id="${runner_tail_check_id[$index]}"
  tail_target="${runner_tail_check_target[$index]}"
  if (
    cd "$repo_root"
    cargo test -p services --features qa-mode --lib "$tail_target" -- --exact
  ) > "$log_dir/$tail_id.log" 2>&1; then
    runner_tail_check_status[$index]="PASS"
    runner_tail_check_actual[$index]="runner 回归目标退出码为 0。"
  else
    runner_tail_check_status[$index]="FAIL"
    runner_tail_check_actual[$index]="runner 回归目标非 0；完整诊断见 $tail_id.log。"
  fi
done

overall="PASS"
for index in "${!case_ids[@]}"; do
  if [[ "${status_for_case[$index]}" != "PASS" ]]; then
    overall="NOT_PASSED"
    break
  fi
done
if [[ "$frontend_check_status" != "PASS" ]]; then
  overall="NOT_PASSED"
fi
for index in "${!runner_tail_check_status[@]}"; do
  if [[ "${runner_tail_check_status[$index]}" != "PASS" ]]; then
    overall="NOT_PASSED"
  fi
done

{
  printf '%s\n' "# Chat Delivery 验收报告"
  printf '\n'
  printf '%s\n' "- 执行时间（UTC）：$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf '%s\n' "- 仓库：$repo_root"
  printf '%s\n' "- 结论：$overall"
  printf '%s\n' "- 规则：仅 11 个独立用例全部 PASS 时允许报告通过。"
  printf '\n'
  printf '%s\n' "| ID | 用例 | 步骤与预期 | 实际结果 | 命令 | 日志/证据 | 状态 |"
  printf '%s\n' "| --- | --- | --- | --- | --- | --- | --- |"
  for index in "${!case_ids[@]}"; do
    printf '| %s | %s | %s | %s | `%s` | [%s](logs/%s.log) | %s |\n' \
      "${case_ids[$index]}" \
      "${case_titles[$index]}" \
      "见 docs/qa/chat-delivery-e2e-acceptance.md" \
      "${actual_for_case[$index]}" \
      "${command_for_case[$index]}" \
      "${case_ids[$index]}" \
      "${case_ids[$index]}" \
      "${status_for_case[$index]}"
  done
  printf '\n'
  printf '%s\n' "## 构建门禁"
  printf '\n'
  printf '%s\n' "- 前端 TypeScript：$frontend_check_status — $frontend_check_actual（[日志](logs/frontend-check.log)）"
  printf '\n'
  printf '%s\n' "## Runner 尾项门禁"
  printf '\n'
  for index in "${!runner_tail_check_id[@]}"; do
    printf '%s\n' "- ${runner_tail_check_id[$index]}：${runner_tail_check_status[$index]} — ${runner_tail_check_actual[$index]}（[日志](logs/${runner_tail_check_id[$index]}.log)）"
  done
  printf '\n'
  printf '%s\n' "## Blocker 规则"
  printf '\n'
  printf '%s\n' "任何非 PASS 行都是 blocker。首次失败和所有重跑输出必须保留在本报告目录；不得基于单独重跑成功抹去首次日志。"
} > "$report_path"

{
  printf '%s' '{"overall":"'
  printf '%s' "$overall"
  printf '%s' '","report":"report.md","cases":['
  for index in "${!case_ids[@]}"; do
    if ((index > 0)); then
      printf '%s' ','
    fi
    printf '{"id":"%s","status":"%s","log":"logs/%s.log"}' \
      "${case_ids[$index]}" "${status_for_case[$index]}" "${case_ids[$index]}"
  done
  printf '%s' '],"checks":['
  printf '{"id":"frontend:check","status":"%s","log":"logs/frontend-check.log"}' \
    "$frontend_check_status"
  for index in "${!runner_tail_check_id[@]}"; do
    printf ',{"id":"%s","status":"%s","log":"logs/%s.log"}' \
      "${runner_tail_check_id[$index]}" \
      "${runner_tail_check_status[$index]}" \
      "${runner_tail_check_id[$index]}"
  done
  printf '%s\n' ']}'
} > "$summary_path"

printf 'Chat delivery acceptance: %s\nReport: %s\n' "$overall" "$report_path"
if [[ "$overall" != "PASS" ]]; then
  exit 1
fi
