import type { WorkflowCardData } from '@/lib/api';

export type LoopReviewSettingsRow = {
  stepId: string;
  title: string;
  userReview: boolean;
};

type PlanLoopReviewProjection = {
  steps: Array<
    Pick<WorkflowCardData['steps'][number], 'step_key' | 'title'>
  >;
  plan: Pick<WorkflowCardData['plan'], 'nodes' | 'loops'>;
};

export function buildPlanLoopReviewSettingsRows(
  projection: PlanLoopReviewProjection
): LoopReviewSettingsRow[] {
  const stepByKey = new Map(
    projection.steps.map((step) => [step.step_key, step])
  );
  const planNodeById = new Map(
    projection.plan.nodes.map((node) => [node.id, node])
  );
  const rowsByReviewStep = new Map<string, LoopReviewSettingsRow>();

  for (const planLoop of projection.plan.loops ?? []) {
    const reviewStepKey =
      planLoop.reviewStep ?? planLoop.review_step_key ?? null;
    if (!reviewStepKey) continue;
    const reviewStep = stepByKey.get(reviewStepKey);
    const reviewNode = planNodeById.get(reviewStepKey);
    rowsByReviewStep.set(reviewStepKey, {
      stepId: reviewStepKey,
      title:
        reviewNode?.data.title ??
        reviewStep?.title ??
        planLoop.loopKey ??
        planLoop.loop_key ??
        reviewStepKey,
      userReview:
        planLoop.userReviewRequired ??
        planLoop.user_review_required ??
        true,
    });
  }

  // The compiler's loop source of truth is a review node with a non-empty
  // reviewScope. Preview projections do not have runtime loops yet and
  // generated plans normally omit the legacy top-level `loops` array.
  for (const node of projection.plan.nodes) {
    const stepType = node.data.stepType ?? node.data.step_type;
    if (
      stepType !== 'review' ||
      !node.data.reviewScope ||
      node.data.reviewScope.length === 0 ||
      rowsByReviewStep.has(node.id)
    ) {
      continue;
    }
    rowsByReviewStep.set(node.id, {
      stepId: node.id,
      title: node.data.title ?? stepByKey.get(node.id)?.title ?? node.id,
      userReview: true,
    });
  }

  return Array.from(rowsByReviewStep.values());
}
