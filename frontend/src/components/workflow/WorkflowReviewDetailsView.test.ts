import assert from 'node:assert/strict';
import { parseWorkflowReviewTranscriptDetails } from './WorkflowReviewDetailsView';

// The parser reads the backend's normalized review projection from the
// transcript meta (`acceptance_results[]` with
// step_key/criterion/level/verdict/evidence), never the agent's raw
// `loop_review_result` protocol payload (`results` map).
//
// Loop review acceptance items carry `step_key`; it must be preserved so the
// UI can show which step each criterion belongs to.
{
  const details = parseWorkflowReviewTranscriptDetails(
    JSON.stringify({
      source: 'workflow_structured_loop_review_result',
      verdict: 'approved',
      acceptance_results: [
        {
          step_key: 'draft',
          criterion: 'cargo test 全部通过',
          level: 'required',
          verdict: 'passed',
          evidence: 'cargo test 输出正常',
        },
        {
          step_key: 'revise',
          criterion: '附带截图',
          level: 'recommended',
          verdict: 'failed',
          evidence: '未附截图',
        },
      ],
      evidence: ['已检查 docs/draft.md'],
    })
  );
  assert.ok(details);
  assert.equal(details.acceptanceResults.length, 2);
  assert.equal(details.acceptanceResults[0].stepKey, 'draft');
  assert.equal(details.acceptanceResults[1].stepKey, 'revise');
  assert.deepEqual(details.evidence, ['已检查 docs/draft.md']);
}

// Items with empty evidence are still shown (without an expandable section)
// instead of being silently dropped.
{
  const details = parseWorkflowReviewTranscriptDetails(
    JSON.stringify({
      acceptance_results: [
        {
          step_key: 'draft',
          criterion: '格式符合规范',
          level: 'required',
          verdict: 'passed',
          evidence: '',
        },
      ],
    })
  );
  assert.ok(details);
  assert.equal(details.acceptanceResults.length, 1);
  assert.equal(details.acceptanceResults[0].evidence, '');
}

// Step review items (no `step_key`) keep their existing behavior, including
// the `not_applicable` verdict that loop review results no longer produce.
{
  const details = parseWorkflowReviewTranscriptDetails(
    JSON.stringify({
      source: 'workflow_step_review',
      acceptance_results: [
        {
          criterion: 'lint 通过',
          level: 'required',
          verdict: 'passed',
          evidence: 'pnpm lint 退出码 0',
        },
        {
          criterion: '性能基准达标',
          level: 'recommended',
          verdict: 'not_applicable',
          evidence: '本步骤不涉及性能改动',
        },
      ],
      risks: ['覆盖率不足'],
      unfinished_items: [],
    })
  );
  assert.ok(details);
  assert.equal(details.acceptanceResults.length, 2);
  assert.equal(details.acceptanceResults[0].stepKey, undefined);
  assert.equal(details.acceptanceResults[1].verdict, 'not_applicable');
  assert.deepEqual(details.risks, ['覆盖率不足']);
}

// Items missing criterion/level/verdict are dropped; when nothing remains the
// parser returns null.
{
  const details = parseWorkflowReviewTranscriptDetails(
    JSON.stringify({
      acceptance_results: [
        { step_key: 'draft', criterion: '', level: 'required', verdict: 'passed', evidence: 'x' },
        { step_key: 'draft', criterion: '有效', level: '', verdict: 'passed', evidence: 'x' },
      ],
    })
  );
  assert.equal(details, null);
  assert.equal(parseWorkflowReviewTranscriptDetails(null), null);
  assert.equal(parseWorkflowReviewTranscriptDetails('not-json'), null);
}

console.log('WorkflowReviewDetailsView tests passed');