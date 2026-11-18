import { describe, expect, it } from "vitest";
import { asNumber, asRecord, asString, parseReady } from "./ipc";

describe("IPC decoders", () => {
  it("coerces unknown payloads without throwing", () => {
    expect(asString(42, "fallback")).toBe("fallback");
    expect(asNumber(Number.NaN)).toBeUndefined();
    expect(asRecord([])).toBeNull();
  });

  it("keeps model windows and engine-provided thinking levels", () => {
    const ready = parseReady({
      model: "e2e-model",
      level: "low",
      contextMax: 4096,
      cwd: "/work",
      provider: "local",
      protocol: "openai-chat-completions",
      models: [
        { id: "a", contextLength: 960_000 },
        { id: "", contextLength: 1 },
        "not-an-object",
      ],
      thinking: ["low", "high", 7],
    });

    expect(ready.model).toBe("e2e-model");
    expect(ready.models).toEqual([{ id: "a", contextLength: 960_000 }]);
    expect(ready.thinkingLevels).toEqual(["low", "high"]);
    expect(ready.contextMax).toBe(4096);
  });
});
