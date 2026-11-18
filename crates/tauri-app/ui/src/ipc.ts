//! Pure IPC decoding helpers. Kept out of the component so they can be tested
//! without a WebView.

export type UnknownRecord = Record<string, unknown>;

export type ModelInfo = { id: string; contextLength: number };

export type Ready = {
  model: string;
  level: string;
  models: ModelInfo[];
  thinkingLevels: string[];
  contextMax: number;
  cwd: string;
  provider: string;
  protocol: string;
};

export const THINKING_LEVELS = [
  "auto",
  "off",
  "minimal",
  "low",
  "medium",
  "high",
  "xhigh",
  "max",
];

export function asRecord(value: unknown): UnknownRecord | null {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as UnknownRecord)
    : null;
}

export function asString(value: unknown, fallback = ""): string {
  return typeof value === "string" ? value : fallback;
}

export function asNumber(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

export function parseReady(raw: UnknownRecord): Ready {
  return {
    model: asString(raw.model),
    level: asString(raw.level),
    models: Array.isArray(raw.models)
      ? raw.models
          .map((value) => asRecord(value))
          .filter((value): value is UnknownRecord => value !== null)
          .map((value) => ({
            id: asString(value.id),
            contextLength: asNumber(value.contextLength) ?? 0,
          }))
          .filter((value) => value.id.length > 0)
      : [],
    thinkingLevels: Array.isArray(raw.thinking)
      ? raw.thinking.filter((value): value is string => typeof value === "string")
      : [],
    contextMax: asNumber(raw.contextMax) ?? 0,
    cwd: asString(raw.cwd),
    provider: asString(raw.provider),
    protocol: asString(raw.protocol),
  };
}
