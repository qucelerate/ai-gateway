import { describe, expect, it } from 'vitest';
import { CLIENT_VERSION, GO_MODULE_VERSION } from './client-versions';
import { hasClient, languages, requestBody, snippet, type CodeStyle, type Language, type PlaygroundRequest } from './playground-code';

const BASE = 'https://gateway.example.com';
const request: PlaygroundRequest = {
  model: 'chat-default',
  messages: [{ role: 'system', content: 'Be brief.' }, { role: 'user', content: 'Hello' }],
  max_tokens: 256,
  temperature: 0.7,
};
// Everything a prompt can hold that a string literal has to escape.
const hostile = 'Quote " and \' and ` and \\ and $HOME and ${x} and %s\nnext line\r\ttab \u0001 bell\u0007 \u2028 sep \u{1F600} é';
const all: [Language, CodeStyle, boolean][] = languages.flatMap(([language]) =>
  (['client', 'raw'] as const).flatMap(style => [false, true].map(stream => [language, style, stream] as [Language, CodeStyle, boolean])));

describe('playground code', () => {
  it.each(all)('%s %s (stream %s) names the key by variable and never holds one', (language, style, stream) => {
    const shown = snippet(language, style, request, { baseUrl: `${BASE}/`, stream });
    expect(shown.code).toContain('GATEWAY_API_KEY');
    expect(shown.code).not.toMatch(/gw_[A-Za-z0-9]/);
    expect(shown.code).toContain(BASE);
    expect(shown.code).not.toContain(`${BASE}//`);
    expect(shown.code).toContain('chat-default');
    expect(shown.setup.length).toBeGreaterThan(0);
    if (shown.style === 'raw') {
      expect(shown.code).toContain('Idempotency-Key');
      expect(shown.code).toContain(stream ? '/api/inference/v1/chat/completions/stream' : '/api/inference/v1/chat/completions');
    }
  });

  it('uses each language’s gateway client where one exists, and says when there is none', () => {
    expect(hasClient('javascript')).toBe(false);
    expect(hasClient('curl')).toBe(false);
    const js = snippet('javascript', 'client', request, { baseUrl: BASE, stream: false });
    expect(js.style).toBe('raw');
    expect(js.note).toMatch(/no JavaScript client/);
    expect(js.code).toContain('fetch(');
    expect(snippet('javascript', 'raw', request, { baseUrl: BASE, stream: false }).note).toBeUndefined();

    expect(snippet('python', 'client', request, { baseUrl: BASE, stream: false }).code).toContain('from qucelerate_ai_gateway import Client');
    expect(snippet('python', 'client', request, { baseUrl: BASE, stream: true }).code).toContain('client.chat_stream(');
    expect(snippet('rust', 'client', request, { baseUrl: BASE, stream: true }).code).toContain('client.chat_stream(&request)');
    expect(snippet('rust', 'client', request, { baseUrl: BASE, stream: false }).code).toContain('.temperature(0.7)');
    expect(snippet('go', 'client', request, { baseUrl: BASE, stream: false }).code).toContain('gateway.NewClient(');
    expect(snippet('go', 'client', request, { baseUrl: BASE, stream: false }).code).toContain('MaxTokens: gateway.Int(256)');
    expect(snippet('java', 'client', request, { baseUrl: BASE, stream: true }).code).toContain('client.chatStream(request)');
    expect(snippet('cpp', 'client', request, { baseUrl: BASE, stream: false }).code).toContain('#include <gateway/gateway.hpp>');
    expect(snippet('cpp', 'raw', request, { baseUrl: BASE, stream: false }).code).toContain('#include <curl/curl.h>');
  });

  it('sends only what was set', () => {
    expect(requestBody({ model: 'm', messages: [{ role: 'user', content: 'x' }] })).toEqual({ model: 'm', messages: [{ role: 'user', content: 'x' }] });
    const bare = snippet('python', 'client', { model: 'm', messages: [{ role: 'user', content: 'x' }] }, { baseUrl: BASE, stream: false });
    expect(bare.code).not.toContain('temperature');
    expect(bare.code).not.toContain('max_tokens');
    // A whole-number temperature stays a float literal.
    expect(snippet('go', 'client', { ...request, temperature: 1 }, { baseUrl: BASE, stream: false }).code).toContain('gateway.Float(1.0)');
  });

  it('carries any prompt exactly: JavaScript and the JSON body', () => {
    const hostileRequest = { ...request, messages: [{ role: 'user' as const, content: hostile }] };
    const js = snippet('javascript', 'raw', hostileRequest, { baseUrl: BASE, stream: false }).code;
    const body = /JSON\.stringify\(([\s\S]*?)\),\n\}\);/.exec(js)![1];
    // The literal is JSON, so it is also a JavaScript expression.
    expect(JSON.parse(body)).toEqual(requestBody(hostileRequest));
    // No raw line separators in source.
    expect(js).not.toContain('\u2028');
    expect(js).not.toContain('\u0001');
  });

  it('carries any prompt exactly: curl’s single-quoted body', () => {
    const hostileRequest = { ...request, messages: [{ role: 'user' as const, content: hostile }] };
    const code = snippet('curl', 'raw', hostileRequest, { baseUrl: BASE, stream: true }).code;
    expect(code).toContain('curl -N ');
    const word = /-d ('(?:[^']|'\\'')*')$/.exec(code)![1];
    const unquoted = word.slice(1, -1).replace(/'\\''/g, "'");
    expect(JSON.parse(unquoted)).toEqual(requestBody(hostileRequest));
  });

  it('escapes control characters as each language requires', () => {
    const hostileRequest = { ...request, messages: [{ role: 'user' as const, content: 'a\u0001b' }] };
    const of = (language: Language, style: CodeStyle) => snippet(language, style, hostileRequest, { baseUrl: BASE, stream: false }).code;
    expect(of('python', 'client')).toContain('"a\\u0001b"');
    expect(of('go', 'client')).toContain('"a\\u0001b"');
    expect(of('rust', 'client')).toContain('"a\\u{1}b"');
    // Java lexes \u escapes before anything else, so control characters are octal.
    expect(of('java', 'client')).toContain('"a\\001b"');
    expect(of('java', 'client')).not.toMatch(/\\u00[01]/);
    // In C, \x is greedy and would swallow the "b"; octal takes three digits.
    expect(of('cpp', 'client')).toContain('"a\\001b"');
  });

  it('makes a fresh idempotency key per run from operating-system randomness', () => {
    const c = snippet('cpp', 'raw', request, { baseUrl: BASE, stream: false }).code;
    expect(c).toContain('/dev/urandom');
    expect(c).not.toMatch(/\bsrand\(|\brand\(\)/);
    expect(snippet('rust', 'raw', request, { baseUrl: BASE, stream: false }).code).toContain('RandomState::new()');
  });

  it('labels the JavaScript example as Node.js', () => {
    const js = snippet('javascript', 'raw', request, { baseUrl: BASE, stream: true });
    expect(js.setup).toMatch(/Node\.js 18/);
    expect(js.setup).not.toMatch(/or a browser/);
  });

  it('keeps a Go raw string only when the body allows one', () => {
    const plain = snippet('go', 'raw', request, { baseUrl: BASE, stream: false }).code;
    expect(plain).toContain('body := []byte(`{');
    const backtick = snippet('go', 'raw', { ...request, messages: [{ role: 'user', content: 'a ` b' }] }, { baseUrl: BASE, stream: false }).code;
    expect(backtick).toContain('body := []byte("{');
  });

  describe('with tools', () => {
    const call = { id: 'call_1', type: 'function' as const, function: { name: 'get_weather', arguments: '{"city":"Paris"}' } };
    const withTools: PlaygroundRequest = {
      model: 'chat-default',
      messages: [
        { role: 'user', content: 'Weather in Paris?' },
        { role: 'assistant', content: '', tool_calls: [call] },
        { role: 'tool', content: '{"temperature":21}', tool_call_id: 'call_1' },
      ],
      tools: [{ type: 'function', function: { name: 'get_weather', description: 'Weather.', parameters: { type: 'object', properties: { city: { type: 'string' } } } } }],
      tool_choice: { type: 'function', function: { name: 'get_weather' } },
      parallel_tool_calls: false,
    };

    it('sends the tools, the calls and their results in the body', () => {
      expect(requestBody(withTools)).toEqual({
        model: 'chat-default',
        messages: [
          { role: 'user', content: 'Weather in Paris?' },
          { role: 'assistant', content: null, tool_calls: [call] },
          { role: 'tool', content: '{"temperature":21}', tool_call_id: 'call_1' },
        ],
        tools: withTools.tools,
        tool_choice: withTools.tool_choice,
        parallel_tool_calls: false,
      });
      // Nothing about tools when there are none.
      expect(Object.keys(requestBody(request))).toEqual(['model', 'messages', 'temperature', 'max_tokens']);
    });

    it.each(all)('%s %s (stream %s) carries the tools, the calls and the results', (language, style, stream) => {
      const { code } = snippet(language, style, withTools, { baseUrl: BASE, stream });
      expect(code).toContain('get_weather');
      expect(code).toContain('call_1');
      expect(code).toContain('temperature');
      expect(code).toMatch(/Paris/);
      expect(code).toMatch(/parallel_tool_calls|ParallelToolCalls|withParallelToolCalls/);
    });

    it('uses each client’s tool types', () => {
      const code = (language: Language, stream = false) => snippet(language, 'client', withTools, { baseUrl: BASE, stream }).code;
      expect(code('python')).toContain('tools=tools, tool_choice={"type": "function", "function": {"name": "get_weather"}}, parallel_tool_calls=False');
      expect(code('python')).toContain('{"role": "assistant", "content": None, "tool_calls": [');
      expect(code('python', true)).toContain('isinstance(event, ToolCallDelta)');
      expect(code('rust')).toContain('Message::assistant_tool_calls("", vec![ToolCall::new("call_1", "get_weather", "{\\"city\\":\\"Paris\\"}")])');
      expect(code('rust')).toContain('Message::tool("call_1", "{\\"temperature\\":21}")');
      expect(code('rust')).toContain('.tool_choice(ToolChoice::Function("get_weather".into()))');
      expect(code('rust')).toContain('.parameters(serde_json::json!({ "type": "object", "properties": { "city": { "type": "string" } } }))');
      expect(code('go')).toContain('ToolCallID: "call_1"');
      expect(code('go')).toContain('toolChoice := gateway.ToolChoiceFunction("get_weather")');
      expect(code('go')).toContain('"encoding/json"');
      expect(code('java')).toContain('Message.tool("call_1", ');
      expect(code('java')).toContain('.withToolChoice(ToolChoice.function("get_weather"))');
      expect(code('java')).toContain('throws Exception');
      expect(code('cpp')).toContain('gateway::Message::tool("call_1", ');
      expect(code('cpp')).toContain('request.tool_choice_function = "get_weather";');
      expect(code('cpp', true)).toContain('gateway::ToolCallDelta');
      // A request without tools keeps its imports as they were.
      expect(snippet('go', 'client', request, { baseUrl: BASE, stream: false }).code).not.toContain('encoding/json');
      expect(snippet('java', 'client', request, { baseUrl: BASE, stream: false }).code).not.toContain('ObjectMapper');
      expect(snippet('rust', 'client', request, { baseUrl: BASE, stream: false }).setup).not.toContain('serde_json');
    });
  });

  // Every component of a release carries one version: the snippets install
  // this console's clients. Cargo treats every 2026.x as compatible, so the
  // crate is pinned exactly; the Go module's version is v0.YYYYMM.PATCH.
  it('installs the clients of this release', () => {
    expect(CLIENT_VERSION).toMatch(/^[1-9]\d{3}\.[1-9]\d?\.\d+(-rc\.\d+)?$/);
    const [year, month, patch] = CLIENT_VERSION.split('-')[0].split('.');
    expect(GO_MODULE_VERSION).toBe(`v0.${year}${month.padStart(2, '0')}.${patch}${CLIENT_VERSION.includes('-') ? CLIENT_VERSION.slice(CLIENT_VERSION.indexOf('-')) : ''}`);
    const setup = (language: Language) => snippet(language, 'client', request, { baseUrl: BASE, stream: false }).setup;
    expect(setup('python')).toContain(`pip install qucelerate-ai-gateway==${CLIENT_VERSION}  # Python 3.10+`);
    expect(setup('rust')).toContain(`qucelerate-ai-gateway = "=${CLIENT_VERSION}"`);
    expect(setup('go')).toContain(`go get github.com/qucelerate/ai-gateway/clients/go@${GO_MODULE_VERSION}`);
    expect(setup('java')).toContain(`com.qucelerate:qucelerate-ai-gateway:${CLIENT_VERSION}`);
  });
});
