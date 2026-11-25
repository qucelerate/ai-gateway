import { afterEach, describe, expect, it, vi } from 'vitest';
import { estimateCost, InferenceError, readEvents, sendChat } from './playground-api';
import type { CatalogPrice } from './api';

const request = { model: 'chat-default', messages: [{ role: 'user' as const, content: 'Hi' }], max_tokens: 16 };
const usage = { input_tokens: 10, output_tokens: 4, cached_input_tokens: 2, reasoning_tokens: 0, other_tokens: 0 };

function bytes(...chunks: string[]): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder();
  return new ReadableStream({
    start(controller) {
      for (const chunk of chunks) controller.enqueue(encoder.encode(chunk));
      controller.close();
    },
  });
}

async function collect(stream: ReadableStream<Uint8Array>) {
  const events = [];
  for await (const event of readEvents(stream)) events.push(event);
  return events;
}

afterEach(() => {
  vi.unstubAllGlobals();
  localStorage.clear();
  sessionStorage.clear();
});

describe('playground inference calls', () => {
  it('sends the key as a bearer token, never the session, and stores nothing', async () => {
    const fetchMock = vi.fn(async (_url: string, _init: RequestInit) => new Response(JSON.stringify({
      id: 'r', model: 'chat-default', choices: [{ index: 0, message: { role: 'assistant', content: 'Hello!' }, finish_reason: 'stop' }], usage,
    }), { status: 200, headers: { 'Content-Type': 'application/json', 'X-Request-Id': 'req-1' } }));
    vi.stubGlobal('fetch', fetchMock);
    let clock = 0;
    const result = await sendChat('gw_secret', request, false, { now: () => (clock += 50) });

    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe('/api/inference/v1/chat/completions');
    expect(init.credentials).toBe('omit');
    const headers = init.headers as Headers;
    expect(headers.get('Authorization')).toBe('Bearer gw_secret');
    expect(headers.get('Idempotency-Key')).toMatch(/^[0-9a-f-]{36}$/);
    expect(headers.has('X-CSRF-Token')).toBe(false);
    expect(JSON.parse(init.body as string)).toEqual({ model: 'chat-default', messages: [{ role: 'user', content: 'Hi' }], max_tokens: 16 });
    expect(result).toMatchObject({ content: 'Hello!', finishReason: 'stop', usage, requestId: 'req-1', firstTokenMs: null, totalMs: 50 });
    expect(localStorage.length).toBe(0);
    expect(sessionStorage.length).toBe(0);
    expect(JSON.stringify(result)).not.toContain('gw_secret');
  });

  it('streams deltas, the finish reason and usage', async () => {
    const body = bytes(
      'event: response.started\ndata: {"model":"chat-default"}\n\n',
      'event: choice.delta\ndata: {"choice_index":0,"content_delta":"Hel"}\n\nevent: choice.de',
      'lta\r\ndata: {"choice_index":0,"content_delta":"lo"}\r',
      '\n\r\n: keep-alive\n\n',
      'event: choice.finished\ndata: {"choice_index":0,"finish_reason":"length"}\n\n',
      `event: response.completed\ndata: ${JSON.stringify({ model: 'chat-default', usage })}\n\n`,
    );
    const fetchMock = vi.fn(async () => new Response(body, { status: 200, headers: { 'Content-Type': 'text/event-stream', 'X-Request-Id': 'req-2' } }));
    vi.stubGlobal('fetch', fetchMock);
    const deltas: string[] = [];
    let clock = 0;
    const result = await sendChat('gw_k', request, true, { onDelta: text => deltas.push(text), now: () => (clock += 10) });
    expect((fetchMock.mock.calls[0] as unknown[])[0]).toBe('/api/inference/v1/chat/completions/stream');
    expect(deltas).toEqual(['Hel', 'lo']);
    expect(result).toMatchObject({ content: 'Hello', finishReason: 'length', usage, requestId: 'req-2', model: 'chat-default', firstTokenMs: 10 });
  });

  it('turns a stream error event and a refused request into errors with the request id', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => new Response(bytes('event: error\ndata: {"code":"provider_unavailable","status":503}\n\n'), {
      status: 200, headers: { 'X-Request-Id': 'req-3' },
    })));
    await expect(sendChat('gw_k', request, true)).rejects.toMatchObject({ status: 503, code: 'provider_unavailable', requestId: 'req-3' });

    vi.stubGlobal('fetch', vi.fn(async () => new Response(JSON.stringify({ code: 'budget_exhausted', detail: 'The budget is spent.', status: 429 }), {
      status: 429, headers: { 'Content-Type': 'application/problem+json', 'X-Request-Id': 'req-4' },
    })));
    const refused = await sendChat('gw_k', request, false).catch(error => error);
    expect(refused).toBeInstanceOf(InferenceError);
    expect(refused).toMatchObject({ status: 429, code: 'budget_exhausted', detail: 'The budget is spent.', requestId: 'req-4' });

    vi.stubGlobal('fetch', vi.fn(async () => { throw new TypeError('offline'); }));
    await expect(sendChat('gw_k', request, false)).rejects.toMatchObject({ status: 0, code: 'network_error' });
  });

  it('reads a refusal from the choice, as the gateway sends it', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => new Response(JSON.stringify({
      id: 'r', model: 'chat-default',
      choices: [{ index: 0, message: { role: 'assistant', content: null }, finish_reason: 'refusal', tool_calls: [], refusal: 'I can’t help with that.', safety: [], annotations: [] }],
      usage,
    }), { status: 200, headers: { 'Content-Type': 'application/json' } })));
    expect(await sendChat('gw_k', request, false)).toMatchObject({ content: '', refusal: 'I can’t help with that.', finishReason: 'refusal' });
  });

  it('reports a stream that closes before response.completed as interrupted', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => new Response(bytes('event: choice.delta\ndata: {"choice_index":0,"content_delta":"Hal"}\n\n'), {
      status: 200, headers: { 'X-Request-Id': 'req-5' },
    })));
    await expect(sendChat('gw_k', request, true)).rejects.toMatchObject({ code: 'stream_interrupted', requestId: 'req-5' });
  });

  it('parses server-sent events split anywhere, with multi-line data', async () => {
    expect(await collect(bytes('event: a\ndata: {"x":', '1}\n\nevent: b\ndata: line one\ndata: line two\n\n', 'data: {"tail":true}'))).toEqual([
      { event: 'a', data: { x: 1 } },
      { event: 'b', data: 'line one\nline two' },
      { event: 'message', data: { tail: true } },
    ]);
  });

  it('estimates cost with the settlement formula', () => {
    const price: CatalogPrice = {
      id: 'p', currency: 'USD', input_minor_per_token: 100, output_minor_per_token: 400, cached_input_minor_per_token: 10,
      reasoning_minor_per_token: 0, other_minor_per_token: 0, source: 'list', source_version: '1', confidence: 'known',
    };
    expect(estimateCost(usage, price)).toBe((10 - 2) * 100 + 4 * 400 + 2 * 10);
    // Reasoning without its own price bills as output.
    expect(estimateCost({ ...usage, reasoning_tokens: 3 }, price)).toBe((10 - 2) * 100 + 4 * 400 + 2 * 10 + 3 * 400);
    expect(estimateCost(usage, null)).toBeNull();
    expect(estimateCost(null, price)).toBeNull();
  });
});

describe('playground tool calls', () => {
  const tools = [{ type: 'function' as const, function: { name: 'get_weather', parameters: { type: 'object', properties: {} } } }];

  it('reads the calls of a unary reply, and sends a conversation that answers them', async () => {
    const call = { id: 'call_1', type: 'function', function: { name: 'get_weather', arguments: '{"city":"Paris"}' } };
    const fetchMock = vi.fn(async (_url: string, _init: RequestInit) => new Response(JSON.stringify({
      id: 'r', model: 'chat-default', choices: [{ index: 0, message: { role: 'assistant', content: '', tool_calls: [call, { id: 7 }] }, finish_reason: 'tool_calls' }], usage,
    }), { status: 200, headers: { 'Content-Type': 'application/json' } }));
    vi.stubGlobal('fetch', fetchMock);
    const result = await sendChat('gw_k', {
      ...request,
      messages: [
        { role: 'user', content: 'Weather?' },
        { role: 'assistant', content: '', tool_calls: [call as never] },
        { role: 'tool', content: '{"c":21}', tool_call_id: 'call_1' },
      ],
      tools,
      tool_choice: 'required',
      parallel_tool_calls: false,
    }, false);
    // A malformed call is left out, not shown half.
    expect(result.toolCalls).toEqual([call]);
    expect(result.finishReason).toBe('tool_calls');
    expect(JSON.parse(fetchMock.mock.calls[0][1].body as string)).toEqual({
      model: 'chat-default',
      messages: [
        { role: 'user', content: 'Weather?' },
        { role: 'assistant', content: null, tool_calls: [call] },
        { role: 'tool', content: '{"c":21}', tool_call_id: 'call_1' },
      ],
      max_tokens: 16,
      tools,
      tool_choice: 'required',
      parallel_tool_calls: false,
    });
  });

  it('assembles streamed calls from their deltas, in order', async () => {
    const delta = (index: number, fields: Record<string, unknown>) =>
      `event: tool_call.delta\ndata: ${JSON.stringify({ choice_index: 0, tool_call_index: index, id: null, name: null, arguments_delta: '', ...fields })}\n\n`;
    const body = bytes(
      'event: response.started\ndata: {"model":"chat-default"}\n\n',
      delta(0, { id: 'call_a', name: 'get_weather', arguments_delta: '{"ci' }),
      delta(1, { id: 'call_b', name: 'now' }),
      delta(0, { arguments_delta: 'ty":"Paris"}' }),
      // Another choice's call, and an index out of order, change nothing.
      `event: tool_call.delta\ndata: ${JSON.stringify({ choice_index: 1, tool_call_index: 0, id: 'x', name: 'x', arguments_delta: 'x' })}\n\n`,
      delta(5, { id: 'late', name: 'late' }),
      'event: choice.finished\ndata: {"choice_index":0,"finish_reason":"tool_calls"}\n\n',
      `event: response.completed\ndata: ${JSON.stringify({ model: 'chat-default', usage })}\n\n`,
    );
    vi.stubGlobal('fetch', vi.fn(async () => new Response(body, { status: 200, headers: { 'Content-Type': 'text/event-stream' } })));
    const seen: number[] = [];
    let clock = 0;
    const result = await sendChat('gw_k', { ...request, tools }, true, { onToolCalls: calls => seen.push(calls.length), now: () => (clock += 10) });
    expect(result.toolCalls).toEqual([
      { id: 'call_a', type: 'function', function: { name: 'get_weather', arguments: '{"city":"Paris"}' } },
      // No arguments: an empty object, as the gateway takes it back.
      { id: 'call_b', type: 'function', function: { name: 'now', arguments: '{}' } },
    ]);
    expect(seen).toEqual([1, 2, 2]);
    expect(result).toMatchObject({ content: '', finishReason: 'tool_calls', firstTokenMs: 10 });
  });
});
