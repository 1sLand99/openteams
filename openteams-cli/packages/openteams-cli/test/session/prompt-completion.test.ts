import { describe, expect, test } from "bun:test"
import { SessionPrompt } from "../../src/session/prompt"

describe("session.prompt completion", () => {
  test("treats Ark-compatible unknown text completions as terminal", () => {
    expect(SessionPrompt.isTerminalCompletion("unknown", false)).toBe(true)
  })

  test("keeps tool-call responses in the agent loop", () => {
    expect(SessionPrompt.isTerminalCompletion("tool-calls", true)).toBe(false)
    expect(SessionPrompt.isTerminalCompletion("unknown", true)).toBe(false)
  })

  test("stops malformed tool-call completions without tool activity", () => {
    expect(SessionPrompt.isTerminalCompletion("tool-calls", false)).toBe(true)
  })

  test("treats standard text finish reasons as terminal", () => {
    expect(SessionPrompt.isTerminalCompletion("stop", false)).toBe(true)
    expect(SessionPrompt.isTerminalCompletion("length", false)).toBe(true)
    expect(SessionPrompt.isTerminalCompletion(undefined, false)).toBe(false)
  })
})
