import type { ChatMessage } from 'shared/types';
import { CheckCircleIcon, ClockIcon, PlayIcon, WarningCircleIcon, PauseIcon } from '@phosphor-icons/react';
import { cn } from '@/lib/utils';

type WorkflowCardNode = {
  id: string;
  position: { x: number; y: number };
  data: {
    stepType: string;
    title: string;
    instructions: string;
    agentId?: string | null;
    status?: string | null;
  };
};

type WorkflowCardEdge = {
  id: string;
  source: string;
  target: string;
};

type WorkflowCardProjection = {
  execution_id?: string | null;
  plan_id?: string;
  revision_id?: string;
  title: string;
  goal: string;
  state: 'preview_ready' | 'preview_invalid' | 'running' | 'completed' | 'failed' | 'paused';
  execution_status: string;
  error_message?: string | null;
  completed_step_count: number;
  total_step_count: number;
  result_summary?: string | null;
  outputs: string[];
  steps: Array<{
    id: string;
    title: string;
    step_type: string;
    status: string;
    agent_name?: string | null;
    summary_text?: string | null;
  }>;
  agents?: Array<{
    session_agent_id: string;
    agent_id: string;
    name: string;
  }>;
  plan: {
    nodes: WorkflowCardNode[];
    edges: WorkflowCardEdge[];
    viewport?: { x?: number; y?: number; zoom?: number };
  };
  validation_errors?: string | null;
};

const isRecord = (value: unknown): value is Record<string, unknown> =>
  !!value && typeof value === 'object' && !Array.isArray(value);

export function extractWorkflowCardProjection(
  meta: unknown
): WorkflowCardProjection | null {
  if (!isRecord(meta)) return null;

  // Support both workflow_execution (legacy) and workflow_plan (new preview) card types
  if (meta.card_type !== 'workflow_execution' && meta.card_type !== 'workflow_plan') {
    return null;
  }

  const workflowCard = meta.workflow_card;
  if (!isRecord(workflowCard)) {
    return null;
  }

  return workflowCard as unknown as WorkflowCardProjection;
}

function WorkflowGraph({ nodes, edges }: { nodes: WorkflowCardNode[]; edges: WorkflowCardEdge[] }) {
  if (nodes.length === 0) {
    return null;
  }

  const width = 170;
  const height = 64;
  const padding = 32;
  const minX = Math.min(...nodes.map((node) => node.position.x));
  const minY = Math.min(...nodes.map((node) => node.position.y));
  const maxX = Math.max(...nodes.map((node) => node.position.x + width));
  const maxY = Math.max(...nodes.map((node) => node.position.y + height));
  const viewBox = `${minX - padding} ${minY - padding} ${maxX - minX + padding * 2} ${
    maxY - minY + padding * 2
  }`;

  const nodeById = new Map(nodes.map((node) => [node.id, node]));
  const statusColor = (status?: string | null) => {
    switch (status) {
      case 'completed':
        return { fill: '#DCFCE7', stroke: '#16A34A', text: '#166534' };
      case 'running':
        return { fill: '#DBEAFE', stroke: '#2563EB', text: '#1D4ED8' };
      case 'failed':
        return { fill: '#FEE2E2', stroke: '#DC2626', text: '#991B1B' };
      case 'ready':
        return { fill: '#FEF3C7', stroke: '#D97706', text: '#92400E' };
      default:
        return { fill: '#F8FAFC', stroke: '#CBD5E1', text: '#334155' };
    }
  };

  return (
    <div className="overflow-hidden rounded-2xl border border-[#DCE4F0] bg-[#F8FAFC] px-3 py-3">
      <svg viewBox={viewBox} className="h-[240px] w-full">
        {edges.map((edge) => {
          const source = nodeById.get(edge.source);
          const target = nodeById.get(edge.target);
          if (!source || !target) return null;
          const x1 = source.position.x + width / 2;
          const y1 = source.position.y + height;
          const x2 = target.position.x + width / 2;
          const y2 = target.position.y;
          return (
            <path
              key={edge.id}
              d={`M ${x1} ${y1} C ${x1} ${(y1 + y2) / 2}, ${x2} ${(y1 + y2) / 2}, ${x2} ${y2}`}
              fill="none"
              stroke="#94A3B8"
              strokeWidth="2"
              strokeDasharray="4 4"
            />
          );
        })}
        {nodes.map((node) => {
          const colors = statusColor(node.data.status);
          return (
            <g key={node.id} transform={`translate(${node.position.x}, ${node.position.y})`}>
              <rect
                width={width}
                height={height}
                rx="18"
                fill={colors.fill}
                stroke={colors.stroke}
                strokeWidth="2"
              />
              <text x="14" y="24" fontSize="12" fontWeight="700" fill={colors.text}>
                {node.data.stepType.toUpperCase()}
              </text>
              <text x="14" y="44" fontSize="14" fontWeight="600" fill="#0F172A">
                {node.data.title.length > 18
                  ? `${node.data.title.slice(0, 18)}...`
                  : node.data.title}
              </text>
            </g>
          );
        })}
      </svg>
    </div>
  );
}

type ChatWorkflowCardProps = {
  message: ChatMessage;
  onExecute?: (planId: string) => void;
  onPauseAll?: (executionId: string) => void;
};

export function ChatWorkflowCard({ message, onExecute, onPauseAll }: ChatWorkflowCardProps) {
  const projection = extractWorkflowCardProjection(message.meta);
  if (!projection) {
    return null;
  }

  const isPreview = projection.state === 'preview_ready' || projection.state === 'preview_invalid';
  const isInvalid = projection.state === 'preview_invalid';

  const stateIcon =
    projection.state === 'completed' ? (
      <CheckCircleIcon className="size-icon-sm text-[#15803D]" weight="fill" />
    ) : projection.state === 'failed' || isInvalid ? (
      <WarningCircleIcon className="size-icon-sm text-[#DC2626]" weight="fill" />
    ) : projection.state === 'preview_ready' ? (
      <PlayIcon className="size-icon-sm text-[#D97706]" weight="fill" />
    ) : projection.state === 'paused' ? (
      <PauseIcon className="size-icon-sm text-[#D97706]" weight="fill" />
    ) : (
      <ClockIcon className="size-icon-sm text-[#2563EB]" weight="fill" />
    );

  const stateLabel =
    projection.state === 'completed'
      ? 'Work Item'
      : projection.state === 'failed'
        ? 'Execution Failed'
        : projection.state === 'preview_ready'
          ? 'Plan Ready'
          : projection.state === 'preview_invalid'
            ? 'Plan Invalid'
            : projection.state === 'paused'
              ? 'Paused'
              : 'Workflow Running';

  return (
    <div className="w-full max-w-[760px] rounded-[28px] border border-[#D8E2F0] bg-white p-4 shadow-sm">
      <div className="flex items-start justify-between gap-4">
        <div className="min-w-0">
          <div className="flex items-center gap-2 text-[11px] font-bold uppercase tracking-[0.16em] text-[#64748B]">
            {stateIcon}
            <span>{stateLabel}</span>
          </div>
          <div className="mt-2 text-[20px] font-semibold leading-tight text-[#0F172A]">
            {projection.title}
          </div>
          <div className="mt-2 text-sm leading-6 text-[#475569]">
            {projection.goal}
          </div>
        </div>
        <div className="rounded-full bg-[#EEF4FF] px-3 py-1 text-xs font-semibold text-[#1D4ED8]">
          {projection.completed_step_count}/{projection.total_step_count}
        </div>
      </div>

      {/* Agent list (preview mode) */}
      {isPreview && projection.agents && projection.agents.length > 0 && (
        <div className="mt-3 flex flex-wrap gap-2">
          {projection.agents.map((agent) => (
            <span
              key={agent.session_agent_id}
              className="rounded-full bg-[#F1F5F9] px-3 py-1 text-xs font-medium text-[#475569]"
            >
              {agent.name}
            </span>
          ))}
        </div>
      )}

      <div className="mt-4">
        <WorkflowGraph
          nodes={projection.plan.nodes}
          edges={projection.plan.edges}
        />
      </div>

      <div className="mt-4 grid gap-2">
        {projection.steps.map((step) => (
          <div
            key={step.id}
            className={cn(
              'rounded-2xl border px-3 py-3',
              step.status === 'completed' && 'border-[#BBF7D0] bg-[#F0FDF4]',
              step.status === 'running' && 'border-[#BFDBFE] bg-[#EFF6FF]',
              step.status === 'failed' && 'border-[#FECACA] bg-[#FEF2F2]',
              !['completed', 'running', 'failed'].includes(step.status) &&
                'border-[#E2E8F0] bg-[#F8FAFC]'
            )}
          >
            <div className="flex items-center justify-between gap-3">
              <div className="min-w-0">
                <div className="text-sm font-semibold text-[#0F172A]">
                  {step.title}
                </div>
                <div className="text-xs uppercase tracking-[0.14em] text-[#64748B]">
                  {step.step_type}
                  {step.agent_name ? ` • ${step.agent_name}` : ''}
                </div>
              </div>
              <div className="text-xs font-semibold text-[#475569]">
                {step.status}
              </div>
            </div>
            {step.summary_text && (
              <div className="mt-2 text-sm leading-6 text-[#475569]">
                {step.summary_text}
              </div>
            )}
          </div>
        ))}
      </div>

      {/* Validation errors (preview_invalid) */}
      {isInvalid && projection.validation_errors && (
        <div className="mt-4 rounded-[24px] border border-[#FECACA] bg-[#FEF2F2] p-4 text-sm leading-6 text-[#991B1B]">
          <div className="text-xs font-bold uppercase tracking-[0.16em]">Validation Errors</div>
          <div className="mt-1">{projection.validation_errors}</div>
        </div>
      )}

      {/* Execute button (preview_ready) */}
      {projection.state === 'preview_ready' && projection.plan_id && onExecute && (
        <div className="mt-4 flex justify-end">
          <button
            type="button"
            onClick={() => onExecute(projection.plan_id!)}
            className="flex items-center gap-2 rounded-full bg-[#2563EB] px-5 py-2 text-sm font-semibold text-white shadow-sm hover:bg-[#1D4ED8] transition-colors"
          >
            <PlayIcon className="size-4" weight="bold" />
            Execute Plan
          </button>
        </div>
      )}

      {/* Pause button (running) */}
      {projection.state === 'running' && projection.execution_id && onPauseAll && (
        <div className="mt-4 flex justify-end">
          <button
            type="button"
            onClick={() => onPauseAll(projection.execution_id!)}
            className="flex items-center gap-2 rounded-full bg-[#D97706] px-5 py-2 text-sm font-semibold text-white shadow-sm hover:bg-[#B45309] transition-colors"
          >
            <PauseIcon className="size-4" weight="bold" />
            Pause All
          </button>
        </div>
      )}

      {projection.state === 'completed' && (
        <div className="mt-4 rounded-[24px] border border-[#D1FAE5] bg-[#ECFDF5] p-4">
          <div className="text-xs font-bold uppercase tracking-[0.16em] text-[#15803D]">
            Final Delivery
          </div>
          {projection.result_summary && (
            <div className="mt-2 text-sm leading-6 text-[#166534]">
              {projection.result_summary}
            </div>
          )}
          {projection.outputs.length > 0 && (
            <div className="mt-3 flex flex-wrap gap-2">
              {projection.outputs.map((output) => (
                <span
                  key={output}
                  className="rounded-full bg-white/80 px-3 py-1 text-xs font-medium text-[#166534]"
                >
                  {output}
                </span>
              ))}
            </div>
          )}
        </div>
      )}

      {projection.state === 'failed' && projection.error_message && (
        <div className="mt-4 rounded-[24px] border border-[#FECACA] bg-[#FEF2F2] p-4 text-sm leading-6 text-[#991B1B]">
          {projection.error_message}
        </div>
      )}
    </div>
  );
}
