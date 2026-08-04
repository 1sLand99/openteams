export function approvalPrompt(event) {
  return {
    title: `Run Pi tool: ${event.toolName ?? "tool"}`,
    message: JSON.stringify({
      toolCallId: event.toolCallId,
      toolName: event.toolName,
      input: event.input,
    }),
  };
}

export function safeProviderError(event) {
  if (event?.willRetry === true || !Array.isArray(event?.messages)) return undefined;
  const message = [...event.messages]
    .reverse()
    .find((candidate) => candidate?.role === "assistant");
  if (message?.stopReason !== "error") return undefined;

  const raw = String(message.errorMessage ?? "").toLowerCase();
  if (raw.includes("connection") || raw.includes("network") || raw.includes("timed out")) {
    return "Pi provider connection failed.";
  }
  if (raw.includes("unauthorized") || raw.includes("forbidden") || raw.includes("api key") || raw.includes("401")) {
    return "Pi provider authentication failed.";
  }
  if (raw.includes("rate limit") || raw.includes("too many requests") || raw.includes("429")) {
    return "Pi provider rate limit exceeded.";
  }
  if (raw.includes("quota") || raw.includes("credit")) {
    return "Pi provider quota exceeded.";
  }
  if (raw.includes("context") || raw.includes("maximum tokens")) {
    return "Pi provider context limit exceeded.";
  }
  return "Pi provider request failed.";
}

export default function openteamsApprovalExtension(pi) {
  pi.on("tool_call", async (event, ctx) => {
    const prompt = approvalPrompt(event);
    const allowed = await ctx.ui.confirm(prompt.title, prompt.message);
    if (!allowed) return { block: true, reason: "Denied by OpenTeams ACP permission policy" };
  });
  pi.on("agent_end", async (event, ctx) => {
    const error = safeProviderError(event);
    if (error) await ctx.ui.notify(error, "error");
  });
}
