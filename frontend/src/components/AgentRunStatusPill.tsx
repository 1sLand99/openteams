import React from "react";
import { ThinkingOrb } from "thinking-orbs";

interface AgentRunStatusPillProps {
  label?: string;
  /** Animation speed multiplier for the thinking orb. */
  speed?: number;
}

export const AgentRunStatusPill: React.FC<AgentRunStatusPillProps> = ({
  label = "正在执行",
  speed = 1,
}) => (
  <div className="inline-flex min-h-6 items-center gap-1.5 rounded-md bg-[var(--primary-tint)] px-2 py-1 text-[var(--primary)]">
    <ThinkingOrb
      state="solving"
      size={20}
      speed={speed}
      className="shrink-0"
    />
    <span className="whitespace-nowrap font-mono text-[11px]">{label}</span>
  </div>
);