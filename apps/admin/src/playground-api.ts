// The playground's calls to the public inference API (D-025). They carry an
// ordinary `gw_` API key as a bearer token and never the admin session:
// `credentials: 'omit'` keeps the session and CSRF cookies off them, so the
// gateway authorizes, reserves, budgets and rate-limits them like any
// application's request. The key lives only in the caller's memory.

import { CHAT_PATH, STREAM_PATH, requestBody, type PlaygroundRequest, type ToolCall } from './playground-code';
import type { CatalogPrice } from './api';

export type Usage = {
  input_tokens: number;
  output_tokens: number;
  cached_input_tokens?: number;
  reasoning_tokens?: number;
  other_tokens?: number;
  quality?: string;
  completeness?: string;
};

export type PlaygroundResult = {
  content: string;
  refusal: string;
  /** The calls the model made (D-032), in order; empty when it made none. */
  toolCalls: ToolCall[];
  finishReason: string | null;
  usage: Usage | null;
  model: string | null;
  requestId: string | null;
  /** Milliseconds from sending to the first content, for a stream. */
  firstTokenMs: number | null;
  totalMs: number;
};

/** A failed call: the gateway's problem, or a network failure (status 0). */
export class InferenceError extends Error {
  constructor(public status: number, public code: string, public detail: string, public requestId: string | null) {
    super(detail || code);
  }
}

type Callbacks = {
  /** Each piece of streamed text as it arrives. */
  onDelta?: (text: string) => void;
  /** The calls streamed so far, each time one grows. */
  onToolCalls?: (calls: ToolCall[]) => void;
  signal?: AbortSignal;
  now?: () => number;
};

function headers(apiKey: string, accept: string): Headers {
  const result = new Headers();
  result.set('Authorization', `Bearer ${apiKey}`);
  result.set('Content-Type', 'application/json');
  result.set('Accept', accept);
  result.set('Idempotency-Key', crypto.randomUUID());
  return result;
}

async function failure(response: Response): Promise<InferenceError> {
  const requestId = response.headers.get('X-Request-Id');
  let code = 'request_failed';
  let detail = `The gateway answered ${response.status}.`;
  try {
    const problem = await response.json() as { code?: string; detail?: string; title?: string; request_id?: string };
    code = problem.code ?? code;
    detail = problem.detail || problem.title || detail;
    return new InferenceError(response.status, code, detail, requestId ?? problem.request_id ?? null);
  } catch {
    return new InferenceError(response.status, code, detail, requestId);
  }
}

/** Sends `request` once, unary or streamed, with `apiKey`. */
export async function sendChat(
  apiKey: string,
  request: PlaygroundRequest,
  stream: boolean,
  { onDelta, onToolCalls, signal, now = () => performance.now() }: Callbacks = {},
): Promise<PlaygroundResult> {
  const started = now();
  let response: Response;
  try {
    response = await fetch(stream ? STREAM_PATH : CHAT_PATH, {
      method: 'POST',
      // The admin session must never reach the inference API (D-007).
      credentials: 'omit',
      cache: 'no-store',
      headers: headers(apiKey, stream ? 'text/event-stream' : 'application/json'),
      body: JSON.stringify(requestBody(request)),
      signal,
    });
  } catch (caught) {
    if (caught instanceof DOMException && caught.name === 'AbortError') throw caught;
    throw new InferenceError(0, 'network_error', 'The gateway could not be reached.', null);
  }
  if (!response.ok) throw await failure(response);
  const requestId = response.headers.get('X-Request-Id');

  if (!stream) {
    const answer = await response.json() as {
      model?: string;
      choices?: { message?: { content?: string | null; tool_calls?: unknown }; refusal?: string | null; finish_reason?: string | null }[];
      usage?: Usage;
    };
    const choice = answer.choices?.[0];
    return {
      content: choice?.message?.content ?? '',
      refusal: choice?.refusal ?? '',
      toolCalls: toolCallsOf(choice?.message?.tool_calls),
      finishReason: choice?.finish_reason ?? null,
      usage: answer.usage ?? null,
      model: answer.model ?? null,
      requestId,
      firstTokenMs: null,
      totalMs: now() - started,
    };
  }

  const result: PlaygroundResult = {
    content: '', refusal: '', toolCalls: [], finishReason: null, usage: null, model: null, requestId, firstTokenMs: null, totalMs: 0,
  };
  if (!response.body) throw new InferenceError(response.status, 'stream_unavailable', 'The browser gave no response stream.', requestId);
  let completed = false;
  for await (const event of readEvents(response.body)) {
    const data = event.data as Record<string, unknown>;
    switch (event.event) {
      case 'response.started':
        result.model = typeof data.model === 'string' ? data.model : result.model;
        break;
      case 'choice.delta': {
        if (data.choice_index !== undefined && data.choice_index !== 0) break;
        const text = typeof data.content_delta === 'string' ? data.content_delta : '';
        const refusal = typeof data.refusal_delta === 'string' ? data.refusal_delta : '';
        if ((text || refusal) && result.firstTokenMs === null) result.firstTokenMs = now() - started;
        result.content += text;
        result.refusal += refusal;
        if (text) onDelta?.(text);
        break;
      }
      case 'tool_call.delta': {
        if (data.choice_index !== undefined && data.choice_index !== 0) break;
        const at = typeof data.tool_call_index === 'number' ? data.tool_call_index : result.toolCalls.length;
        if (!Number.isInteger(at) || at < 0 || at > result.toolCalls.length) break;
        if (result.firstTokenMs === null) result.firstTokenMs = now() - started;
        const calls = [...result.toolCalls];
        const call = calls[at] ?? { id: '', type: 'function' as const, function: { name: '', arguments: '' } };
        calls[at] = {
          ...call,
          id: typeof data.id === 'string' ? data.id : call.id,
          function: {
            name: typeof data.name === 'string' ? data.name : call.function.name,
            arguments: call.function.arguments + (typeof data.arguments_delta === 'string' ? data.arguments_delta : ''),
          },
        };
        result.toolCalls = calls;
        onToolCalls?.(calls);
        break;
      }
      case 'choice.finished':
        if (data.choice_index === undefined || data.choice_index === 0) {
          result.finishReason = typeof data.finish_reason === 'string' ? data.finish_reason : null;
        }
        break;
      case 'usage':
        result.usage = data as unknown as Usage;
        break;
      case 'response.completed':
        completed = true;
        if (data.usage) result.usage = data.usage as Usage;
        if (typeof data.model === 'string') result.model = data.model;
        break;
      case 'error':
        throw new InferenceError(
          typeof data.status === 'number' ? data.status : 502,
          typeof data.code === 'string' ? data.code : 'stream_error',
          typeof data.detail === 'string' ? data.detail : 'The stream ended with an error.',
          requestId,
        );
    }
  }
  // A stream closed before `response.completed` (a proxy, a dropped
  // connection) is an unfinished reply, not a complete one.
  if (!completed) {
    throw new InferenceError(0, 'stream_interrupted', 'The stream ended before the reply was complete. Try again.', requestId);
  }
  // A call made without arguments goes back as an empty object, which is
  // what the gateway takes in a conversation.
  result.toolCalls = result.toolCalls.map(call => (call.function.arguments ? call : { ...call, function: { ...call.function, arguments: '{}' } }));
  result.totalMs = now() - started;
  return result;
}

/** The well-formed calls of a response's `message.tool_calls`. */
function toolCallsOf(value: unknown): ToolCall[] {
  if (!Array.isArray(value)) return [];
  return value.flatMap(item => {
    const call = item as { id?: unknown; function?: { name?: unknown; arguments?: unknown } };
    if (typeof call?.id !== 'string' || typeof call.function?.name !== 'string') return [];
    return [{ id: call.id, type: 'function' as const, function: { name: call.function.name, arguments: typeof call.function.arguments === 'string' && call.function.arguments ? call.function.arguments : '{}' } }];
  });
}

export type ServerEvent = { event: string; data: unknown };

/** Server-sent events from a byte stream: each blank-line-terminated block
 * with an `event:` name and `data:` lines (joined by newlines, as JSON). */
export async function* readEvents(body: ReadableStream<Uint8Array>): AsyncGenerator<ServerEvent> {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffered = '';
  let name = 'message';
  let data: string[] = [];
  const flush = (): ServerEvent | null => {
    if (data.length === 0) {
      name = 'message';
      return null;
    }
    const text = data.join('\n');
    const event = name;
    name = 'message';
    data = [];
    try {
      return { event, data: JSON.parse(text) };
    } catch {
      return { event, data: text };
    }
  };
  const lines = function* (final: boolean) {
    const parts = buffered.split(/\r\n|\r|\n/);
    // A trailing `\r` may be half of `\r\n`: keep it until the next chunk.
    buffered = final ? '' : parts.pop() ?? '';
    if (final && parts.length && parts[parts.length - 1] === '') parts.pop();
    yield* parts;
  };
  const handle = (line: string): ServerEvent | null => {
    if (line === '') return flush();
    if (line.startsWith(':')) return null;
    const colon = line.indexOf(':');
    const field = colon < 0 ? line : line.slice(0, colon);
    let value = colon < 0 ? '' : line.slice(colon + 1);
    if (value.startsWith(' ')) value = value.slice(1);
    if (field === 'event') name = value;
    else if (field === 'data') data.push(value);
    return null;
  };
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      buffered += decoder.decode(value, { stream: true });
      if (buffered.endsWith('\r')) continue;
      for (const line of lines(false)) {
        const event = handle(line);
        if (event) yield event;
      }
    }
    buffered += decoder.decode();
    for (const line of lines(true)) {
      const event = handle(line);
      if (event) yield event;
    }
    const last = flush();
    if (last) yield last;
  } finally {
    reader.releaseLock();
  }
}

/** What the usage costs at the catalog's price, in money units, as the
 * ledger settles it (`charge_tokens_with_breakdown`): cached tokens are part
 * of the input, and reasoning without its own price bills as output. Null
 * without a price. An estimate: the ledger's settlement is authoritative. */
export function estimateCost(usage: Usage | null, price: CatalogPrice | null | undefined): number | null {
  if (!usage || !price) return null;
  const cached = Math.min(usage.cached_input_tokens ?? 0, usage.input_tokens);
  const reasoningRate = price.reasoning_minor_per_token || price.output_minor_per_token;
  return (
    (usage.input_tokens - cached) * price.input_minor_per_token
    + usage.output_tokens * price.output_minor_per_token
    + cached * price.cached_input_minor_per_token
    + (usage.reasoning_tokens ?? 0) * reasoningRate
    + (usage.other_tokens ?? 0) * price.other_minor_per_token
  );
}
