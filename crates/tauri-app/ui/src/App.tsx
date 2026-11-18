import {
  For,
  Show,
  createEffect,
  createSignal,
  onCleanup,
  onMount,
} from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

type Tool = {
  id: string;
  name: string;
  args: string;
  state: "running" | "ok" | "error";
  output: string;
  duration?: number;
};

type TextItem = {
  id: number;
  kind: "user" | "assistant" | "thinking" | "notice";
  text: string;
};

type ToolItem = { id: number; kind: "tool"; tool: Tool };
type Item = TextItem | ToolItem;

type Approval = { id: number; name: string; arguments: string };

type Ready = {
  model: string;
  level: string;
  contextMax: number;
  cwd: string;
  provider: string;
  protocol: string;
};

type AgentEvent =
  | { type: "turn_start" }
  | { type: "turn_end" }
  | { type: "assistant_text"; text: string }
  | { type: "thinking"; text: string }
  | {
      type: "tool_requested";
      id: string;
      name: string;
      arguments: string;
    }
  | {
      type: "tool_finished";
      id: string;
      name: string;
      output: string;
      isError: boolean;
      durationMs?: number;
    }
  | { type: "finished"; reason: string }
  | { type: "notice"; text: string }
  | { type: "aborted" };

type Incoming =
  | ({ kind: "ready" } & Ready)
  | { kind: "agent"; event: AgentEvent }
  | { kind: "turn_done"; reason: string }
  | Approval & { kind: "approval" }
  | { kind: "session"; path: string }
  | { kind: "session_cleared" }
  | { kind: "error"; message: string };

type UnknownRecord = Record<string, unknown>;

function asRecord(value: unknown): UnknownRecord | null {
  return typeof value === "object" && value !== null ? (value as UnknownRecord) : null;
}

function asString(value: unknown, fallback = ""): string {
  return typeof value === "string" ? value : fallback;
}

function asNumber(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

function parseReady(raw: UnknownRecord): Ready {
  return {
    model: asString(raw.model),
    level: asString(raw.level),
    contextMax: asNumber(raw.contextMax) ?? 0,
    cwd: asString(raw.cwd),
    provider: asString(raw.provider),
    protocol: asString(raw.protocol),
  };
}

function parseAgentEvent(raw: unknown): AgentEvent | null {
  const value = asRecord(raw);
  if (!value) return null;
  switch (value.type) {
    case "turn_start":
      return { type: "turn_start" };
    case "turn_end":
      return { type: "turn_end" };
    case "assistant_text":
      return { type: "assistant_text", text: asString(value.text) };
    case "thinking":
      return { type: "thinking", text: asString(value.text) };
    case "tool_requested":
      return {
        type: "tool_requested",
        id: asString(value.id),
        name: asString(value.name),
        arguments: asString(value.arguments),
      };
    case "tool_finished":
      return {
        type: "tool_finished",
        id: asString(value.id),
        name: asString(value.name),
        output: asString(value.output),
        isError: value.isError === true,
        durationMs: asNumber(value.durationMs),
      };
    case "finished":
      return { type: "finished", reason: asString(value.reason, "ready") };
    case "notice":
      return { type: "notice", text: asString(value.text) };
    case "aborted":
      return { type: "aborted" };
    default:
      return null;
  }
}

function parseIncoming(raw: unknown): Incoming | null {
  const value = asRecord(raw);
  if (!value) return null;
  switch (value.kind) {
    case "ready":
      return { kind: "ready", ...parseReady(value) };
    case "agent": {
      const event = parseAgentEvent(value.event);
      return event ? { kind: "agent", event } : null;
    }
    case "turn_done":
      return { kind: "turn_done", reason: asString(value.reason, "ready") };
    case "approval":
      return {
        kind: "approval",
        id: asNumber(value.id) ?? 0,
        name: asString(value.name),
        arguments: asString(value.arguments),
      };
    case "session":
      return { kind: "session", path: asString(value.path) };
    case "session_cleared":
      return { kind: "session_cleared" };
    case "error":
      return { kind: "error", message: asString(value.message, "unknown error") };
    default:
      return null;
  }
}

let nextId = 1;

function App() {
  const [items, setItems] = createSignal<Item[]>([]);
  const [input, setInput] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [status, setStatus] = createSignal("starting");
  const [ready, setReady] = createSignal<Ready | null>(null);
  const [approval, setApproval] = createSignal<Approval | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  const [sessionPath, setSessionPath] = createSignal("");
  const [follow, setFollow] = createSignal(true);

  let viewport!: HTMLDivElement;
  let composer!: HTMLTextAreaElement;

  const append = (kind: TextItem["kind"], text: string, stream = false) => {
    setItems((prev) => {
      const last = prev[prev.length - 1];
      if (stream && last && last.kind === kind) {
        return [...prev.slice(0, -1), { ...last, text: last.text + text }];
      }
      return [...prev, { id: nextId++, kind, text }];
    });
  };

  const applyReady = (value: Ready) => {
    setReady(value);
    setStatus("ready");
  };

  const handleAgent = (event: AgentEvent) => {
    switch (event.type) {
      case "turn_start":
        setStatus("thinking");
        break;
      case "assistant_text":
        append("assistant", event.text, true);
        break;
      case "thinking":
        append("thinking", event.text, true);
        break;
      case "tool_requested":
        setItems((prev) => [
          ...prev,
          {
            id: nextId++,
            kind: "tool",
            tool: {
              id: event.id,
              name: event.name,
              args: event.arguments,
              state: "running",
              output: "",
            },
          },
        ]);
        break;
      case "tool_finished":
        setItems((prev) =>
          prev.map((item) => {
            if (item.kind !== "tool" || item.tool.id !== event.id) return item;
            return {
              ...item,
              tool: {
                ...item.tool,
                state: event.isError ? "error" : "ok",
                output: event.output,
                duration: event.durationMs,
              },
            };
          }),
        );
        break;
      case "finished":
        setBusy(false);
        setStatus(event.reason);
        break;
      case "aborted":
        setBusy(false);
        setStatus("aborted");
        break;
      case "notice":
        append("notice", event.text);
        break;
      default:
        break;
    }
  };

  const handle = (payload: Incoming) => {
    switch (payload.kind) {
      case "ready":
        applyReady(payload);
        break;
      case "agent":
        handleAgent(payload.event);
        break;
      case "turn_done":
        setBusy(false);
        setStatus(payload.reason);
        break;
      case "approval":
        setApproval(payload);
        break;
      case "session":
        setSessionPath(payload.path);
        break;
      case "session_cleared":
        setItems([]);
        setSessionPath("");
        break;
      case "error":
        setError(payload.message);
        setBusy(false);
        setStatus("error");
        break;
    }
  };

  onMount(async () => {
    let unlisten: UnlistenFn | undefined;
    try {
      unlisten = await listen<unknown>("argent://event", (event) => {
        const payload = parseIncoming(event.payload);
        if (payload) handle(payload);
      });
      const state = await invoke<unknown>("get_state");
      const stateRecord = asRecord(state);
      const readyRecord = stateRecord ? asRecord(stateRecord.ready) : null;
      if (readyRecord) applyReady(parseReady(readyRecord));
    } catch (err) {
      setError(String(err));
    }
    onCleanup(() => unlisten?.());
  });

  createEffect(() => {
    items();
    if (follow() && viewport) {
      requestAnimationFrame(() => {
        viewport.scrollTop = viewport.scrollHeight;
      });
    }
  });

  const send = async () => {
    const text = input().trim();
    if (!text || busy()) return;
    setItems((prev) => [...prev, { id: nextId++, kind: "user", text }]);
    setInput("");
    setBusy(true);
    setStatus("working");
    setError(null);
    composer.style.height = "auto";
    try {
      await invoke("send_prompt", { text });
    } catch (err) {
      setError(String(err));
      setBusy(false);
      setStatus("error");
    }
  };

  const abort = async () => {
    try {
      await invoke("abort");
    } catch (err) {
      setError(String(err));
    }
  };

  const newSession = async () => {
    if (busy()) return;
    try {
      await invoke("new_session");
      setItems([]);
      setSessionPath("");
    } catch (err) {
      setError(String(err));
    }
  };

  const answer = async (allow: boolean) => {
    const pending = approval();
    if (!pending) return;
    setApproval(null);
    try {
      await invoke("answer_approval", { id: pending.id, allow });
    } catch (err) {
      setError(String(err));
    }
  };

  const onComposerKey = (event: KeyboardEvent) => {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      void send();
    }
  };

  const resizeComposer = (event: InputEvent) => {
    const target = event.currentTarget as HTMLTextAreaElement | null;
    if (!target) return;
    setInput(target.value);
    target.style.height = "auto";
    target.style.height = `${Math.min(target.scrollHeight, 180)}px`;
  };

  return (
    <div class="app">
      <aside class="sidebar">
        <div class="brand">argent</div>
        <div class="side-head">
          <span>任务</span>
          <button class="icon" title="新任务" onClick={() => void newSession()}>
            ＋
          </button>
        </div>
        <nav class="side-list">
          <button class="side-item active">
            <span class="side-title">当前会话</span>
            <span class="side-meta">{sessionPath() || ready()?.cwd || "新任务"}</span>
          </button>
        </nav>
        <div class="side-foot">
          <span class="dot" classList={{ live: busy() }} />
          {busy() ? "运行中" : "就绪"}
        </div>
      </aside>

      <main class="main">
        <header class="topbar">
          <div class="title-block">
            <div class="title">Argent</div>
            <div class="path">{ready()?.cwd || "D:\\code\\mpi\\argent"}</div>
          </div>
          <div class="top-meta">
            <span class="label">模型</span>
            <span class="pill">{ready()?.model || "—"}</span>
            <span class="label">思考</span>
            <span class="pill">{ready()?.level || "auto"}</span>
          </div>
        </header>

        <section
          class="transcript"
          ref={viewport}
          onScroll={() => {
            const atEnd =
              viewport.scrollHeight - viewport.scrollTop - viewport.clientHeight < 48;
            setFollow(atEnd);
          }}
        >
          <Show
            when={items().length > 0}
            fallback={
              <div class="welcome">
                <div class="welcome-card">
                  <div class="welcome-mark">argent</div>
                  <p>问点什么，或者输入 / 看命令。</p>
                  <div class="path">{ready()?.cwd || "D:\\code\\mpi\\argent"}</div>
                  <div class="shortcuts">
                    <button
                      onClick={() =>
                        setInput(
                          "Read the repository structure and summarize the main modules.",
                        )
                      }
                    >
                      读懂仓库
                    </button>
                    <button
                      onClick={() =>
                        setInput(
                          "Find one likely bug in this repository and explain how to fix it.",
                        )
                      }
                    >
                      找一个 bug
                    </button>
                    <button
                      onClick={() =>
                        setInput("Explain the architecture of this repository.")
                      }
                    >
                      解释架构
                    </button>
                  </div>
                </div>
              </div>
            }
          >
            <For each={items()}>
              {(item) => {
                if (item.kind === "tool") {
                  return <ToolCard tool={item.tool} />;
                }
                if (item.kind === "user") {
                  return (
                    <div class="user-row">
                      <div class="user-bubble">
                        <div class="role">你</div>
                        <div class="text">{item.text}</div>
                      </div>
                    </div>
                  );
                }
                if (item.kind === "thinking") {
                  return (
                    <div class="thinking">
                      <div class="role">思考</div>
                      <div class="text">{item.text}</div>
                    </div>
                  );
                }
                if (item.kind === "notice") {
                  return <div class="notice">{item.text}</div>;
                }
                return (
                  <div class="assistant">
                    <div class="role">argent</div>
                    <div class="text">{item.text}</div>
                  </div>
                );
              }}
            </For>
          </Show>
          <div class="anchor" />
        </section>

        <Show when={approval()}>
          {(pending) => (
            <div class="approval">
              <div class="approval-head">
                <span class="approval-mark">!</span>
                <strong>允许工具调用？</strong>
                <code>{pending().name}</code>
              </div>
              <pre>{pending().arguments}</pre>
              <div class="approval-actions">
                <button class="ghost" onClick={() => void answer(false)}>
                  拒绝
                </button>
                <button class="primary" onClick={() => void answer(true)}>
                  允许
                </button>
              </div>
            </div>
          )}
        </Show>

        <Show when={error()}>
          <div class="error-bar">
            <span>{error()}</span>
            <button class="icon" onClick={() => setError(null)}>
              ×
            </button>
          </div>
        </Show>

        <footer class="composer-wrap">
          <div class="composer">
            <textarea
              ref={composer}
              value={input()}
              placeholder="给 argent 发消息…"
              rows={1}
              onInput={resizeComposer}
              onKeyDown={onComposerKey}
            />
            <div class="composer-actions">
              <span class="hint">Enter 发送 · Shift+Enter 换行</span>
              <div class="buttons">
                <button class="ghost" disabled={!busy()} onClick={() => void abort()}>
                  停止
                </button>
                <button
                  class="primary"
                  disabled={!input().trim() || busy()}
                  onClick={() => void send()}
                >
                  发送
                </button>
              </div>
            </div>
          </div>
          <div class="statusbar">
            <span class="dot" classList={{ live: busy() }} />
            <span>{status()}</span>
            <span class="sep">·</span>
            <span>{ready()?.provider || "—"}</span>
            <span class="sep">·</span>
            <span>{ready()?.model || "—"}</span>
            <span class="spacer" />
            <span>ctx — / {ready()?.contextMax || "—"}</span>
          </div>
        </footer>
      </main>
    </div>
  );
}

function ToolCard(props: { tool: Tool }) {
  const stateLabel = () => {
    if (props.tool.state === "running") return "运行中";
    if (props.tool.state === "error") return "失败";
    return "完成";
  };
  return (
    <div
      class="tool"
      classList={{
        failed: props.tool.state === "error",
        running: props.tool.state === "running",
      }}
    >
      <div class="tool-head">
        <span class="tool-state">
          {props.tool.state === "error"
            ? "✗"
            : props.tool.state === "running"
              ? "·"
              : "✓"}
        </span>
        <code class="tool-name">{props.tool.name}</code>
        <span class="tool-args">{props.tool.args}</span>
        <span class="spacer" />
        <span class="tool-meta">
          {stateLabel()}
          {props.tool.duration !== undefined ? ` · ${props.tool.duration}ms` : ""}
        </span>
      </div>
      <Show when={props.tool.output}>
        <pre class="tool-output">{props.tool.output}</pre>
      </Show>
    </div>
  );
}

export default App;
