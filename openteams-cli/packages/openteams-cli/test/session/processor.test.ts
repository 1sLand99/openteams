import { describe, expect, spyOn, test } from "bun:test"
import { Instance } from "../../src/project/instance"
import type { Provider } from "../../src/provider/provider"
import { ModelID, ProviderID } from "../../src/provider/schema"
import { Session } from "../../src/session"
import { LLM } from "../../src/session/llm"
import { MessageV2 } from "../../src/session/message-v2"
import { SessionProcessor } from "../../src/session/processor"
import { MessageID } from "../../src/session/schema"
import { Log } from "../../src/util/log"
import { tmpdir } from "../fixture/fixture"

Log.init({ print: false })

describe("session.processor tool calls", () => {
  test("handles a tool-call event without a preceding tool-input-start event", async () => {
    await using tmp = await tmpdir({ git: true })

    await Instance.provide({
      directory: tmp.path,
      fn: async () => {
        const session = await Session.create({})
        const providerID = ProviderID.make("ark")
        const modelID = ModelID.make("glm-test")
        const user = await Session.updateMessage({
          id: MessageID.ascending(),
          sessionID: session.id,
          role: "user",
          time: { created: Date.now() },
          agent: "default",
          model: { providerID, modelID },
        })
        const assistant = (await Session.updateMessage({
          id: MessageID.ascending(),
          sessionID: session.id,
          role: "assistant",
          parentID: user.id,
          modelID,
          providerID,
          mode: "default",
          agent: "default",
          path: {
            cwd: tmp.path,
            root: tmp.path,
          },
          cost: 0,
          tokens: {
            input: 0,
            output: 0,
            reasoning: 0,
            cache: { read: 0, write: 0 },
          },
          time: { created: Date.now() },
        })) as MessageV2.Assistant
        const model = {
          id: modelID,
          providerID,
          api: {
            id: modelID,
            url: "https://example.test",
            npm: "@ai-sdk/openai",
          },
          name: "GLM test",
          capabilities: {
            temperature: true,
            reasoning: true,
            attachment: false,
            toolcall: true,
            input: {
              text: true,
              audio: false,
              image: false,
              video: false,
              pdf: false,
            },
            output: {
              text: true,
              audio: false,
              image: false,
              video: false,
              pdf: false,
            },
            interleaved: true,
          },
          cost: {
            input: 0,
            output: 0,
            cache: { read: 0, write: 0 },
          },
          limit: {
            context: 128_000,
            output: 8_192,
          },
          status: "active",
          options: {},
          headers: {},
          release_date: "2026-01-01",
        } satisfies Provider.Model

        async function* fullStream() {
          yield {
            type: "tool-call",
            toolCallId: "call-1",
            toolName: "read",
            input: { filePath: "README.md" },
          }
          yield {
            type: "tool-result",
            toolCallId: "call-1",
            input: { filePath: "README.md" },
            output: {
              output: "contents",
              title: "Read README.md",
              metadata: {},
              attachments: [],
            },
          }
        }

        const stream = spyOn(LLM, "stream").mockResolvedValue({
          fullStream: fullStream(),
        } as never)

        try {
          const processor = SessionProcessor.create({
            assistantMessage: assistant,
            sessionID: session.id,
            model,
            abort: new AbortController().signal,
          })
          const result = await processor.process({} as LLM.StreamInput)
          const parts = await MessageV2.parts(assistant.id)
          const tool = parts.find((part) => part.type === "tool")

          expect(result).toBe("continue")
          expect(tool).toMatchObject({
            type: "tool",
            callID: "call-1",
            tool: "read",
            state: {
              status: "completed",
              input: { filePath: "README.md" },
              output: "contents",
            },
          })
        } finally {
          stream.mockRestore()
          await Session.remove(session.id)
        }
      },
    })
  })
})
