import assert from 'node:assert/strict';
import type { WorkflowCardData } from '@/lib/api';
import { buildPlanLoopReviewSettingsRows } from './workflowReviewSettings';

const previewProjection = {
  steps: [
    {
      id: 'draft',
      step_key: 'draft',
      title: 'Draft',
    },
    {
      id: 'quality-review',
      step_key: 'quality-review',
      title: 'Quality review',
    },
  ],
  plan: {
    nodes: [
      {
        id: 'draft',
        data: {
          stepType: 'task',
          title: 'Draft',
        },
      },
      {
        id: 'quality-review',
        data: {
          stepType: 'review',
          title: 'Quality review',
          reviewScope: ['draft'],
        },
      },
      {
        id: 'plain-review',
        data: {
          stepType: 'review',
          title: 'Plain review',
          reviewScope: [],
        },
      },
    ],
    edges: [],
    loops: null,
  },
} satisfies {
  steps: Array<
    Pick<WorkflowCardData['steps'][number], 'step_key' | 'title' | 'id'>
  >;
  plan: Pick<WorkflowCardData['plan'], 'nodes' | 'edges' | 'loops'>;
};

assert.deepEqual(buildPlanLoopReviewSettingsRows(previewProjection), [
  {
    stepId: 'quality-review',
    title: 'Quality review',
    userReview: true,
  },
]);

const projectionWithLegacyLoop = {
  ...previewProjection,
  plan: {
    ...previewProjection.plan,
    loops: [
      {
        loopKey: 'legacy-loop',
        reviewStep: 'quality-review',
        userReviewRequired: false,
      },
    ],
  },
} satisfies {
  steps: Array<
    Pick<WorkflowCardData['steps'][number], 'step_key' | 'title' | 'id'>
  >;
  plan: Pick<WorkflowCardData['plan'], 'nodes' | 'edges' | 'loops'>;
};

assert.deepEqual(buildPlanLoopReviewSettingsRows(projectionWithLegacyLoop), [
  {
    stepId: 'quality-review',
    title: 'Quality review',
    userReview: false,
  },
]);

console.log('Workflow pre-execution loop review settings: PASS');
