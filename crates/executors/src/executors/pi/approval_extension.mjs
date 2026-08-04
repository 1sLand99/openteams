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

export default function openteamsApprovalExtension(pi) {
  pi.on("tool_call", async (event, ctx) => {
    const prompt = approvalPrompt(event);
    const allowed = await ctx.ui.confirm(prompt.title, prompt.message);
    if (!allowed) return { block: true, reason: "Denied by OpenTeams ACP permission policy" };
  });
}
