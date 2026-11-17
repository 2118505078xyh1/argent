/**
 * Typed client for `argent --rpc`.
 *
 * The wire format is newline-delimited JSON: one request with an `id`, then
 * any number of `event` lines followed by one `result` line with the same id.
 * Requests can be pipelined; events are routed by id, so a steering message
 * never steals another prompt's stream.
 */

import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { createInterface } from 'node:readline';

export interface ArgentEvent {
  id: number;
  type: 'event' | 'result' | 'error';
  event?: string;
  data?: Record<string, unknown>;
  message?: string;
}

export interface PromptFailure {
  reason: string;
  [key: string]: unknown;
}

export interface ModelInfo {
  id: string;
  name?: string;
  contextWindow?: number;
}

export interface Transport {
  write(line: string): void;
  close(): void;
}

/** Minimal transport used by tests and by hosts that already own the pipes. */
export function lineTransport(write: (line: string) => void): Transport {
  return { write, close() {} };
}

type Pending = {
  resolve(value: Record<string, unknown>): void;
  reject(error: Error): void;
  onEvent?: (event: ArgentEvent) => void;
};

export class ArgentClient {
  private nextId = 1;
  private pending = new Map<number, Pending>();
  private child?: ChildProcessWithoutNullStreams;

  private readonly transport: Transport;

  constructor(transport: Transport) {
    this.transport = transport;
  }

  /** Spawn `argent --rpc` and speak NDJSON over its stdio. */
  static spawn(binary = 'argent', args: string[] = ['--rpc']): ArgentClient {
    const child = spawn(binary, args, { stdio: ['pipe', 'pipe', 'pipe'] });
    let client!: ArgentClient;
    client = new ArgentClient({
      write(line) {
        child.stdin.write(line + '\n');
      },
      close() {
        child.kill();
      },
    });
    client.child = child;
    const lines = createInterface({ input: child.stdout });
    lines.on('line', (line) => client.feed(line));
    child.stderr.on('data', () => {
      // Diagnostics are for the parent process; the protocol stays on stdout.
    });
    return client;
  }

  /** Feed one server line. Exported for custom transports and tests. */
  feed(line: string): void {
    if (!line.trim()) return;
    let message: ArgentEvent;
    try {
      message = JSON.parse(line) as ArgentEvent;
    } catch {
      return;
    }
    const pending = this.pending.get(message.id);
    if (!pending) return;
    if (message.type === 'event') {
      pending.onEvent?.(message);
      return;
    }
    this.pending.delete(message.id);
    if (message.type === 'error' || message.message) {
      pending.reject(new Error(message.message ?? 'argent rpc error'));
    } else {
      pending.resolve(message.data ?? {});
    }
  }

  request<T extends Record<string, unknown> = Record<string, unknown>>(
    method: string,
    params: Record<string, unknown> = {},
    onEvent?: (event: ArgentEvent) => void,
  ): Promise<T> {
    const id = this.nextId++;
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, {
        resolve: (value) => resolve(value as T),
        reject,
        onEvent,
      });
      this.transport.write(JSON.stringify({ id, method, params }));
    });
  }

  prompt(text: string, onEvent?: (event: ArgentEvent) => void): Promise<PromptFailure> {
    return this.request('prompt', { text }, onEvent);
  }

  steer(text: string): Promise<Record<string, unknown>> {
    return this.request('steer', { text });
  }

  models(): Promise<ModelInfo[]> {
    return this.request<{ models: ModelInfo[] }>('models').then((data) => data.models ?? []);
  }

  compact(): Promise<Record<string, unknown>> {
    return this.request('compact');
  }

  newSession(): Promise<Record<string, unknown>> {
    return this.request('new_session');
  }

  setModel(model: string): Promise<Record<string, unknown>> {
    return this.request('set_model', { model });
  }

  abort(): void {
    this.child?.kill();
    this.transport.close();
    for (const pending of this.pending.values()) {
      pending.reject(new Error('aborted'));
    }
    this.pending.clear();
  }
}
