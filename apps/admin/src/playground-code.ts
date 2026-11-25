// Code that sends the playground's request (D-025): curl, Python,
// JavaScript, Rust, Go, Java and C/C++, through the gateway's client
// libraries (`clients/`) or as raw HTTP, unary or streamed.
//
// Every snippet reads the key from GATEWAY_API_KEY and never contains one,
// names the gateway's own origin, sends an Idempotency-Key, and escapes the
// prompt for its language, so any text typed in the playground yields code
// that compiles and sends exactly that text.

export type ChatRole = 'system' | 'developer' | 'user' | 'assistant' | 'tool';
/** A call the model made (D-032), as a response returns it. */
export type ToolCall = { id: string; type: 'function'; function: { name: string; arguments: string } };
/** A message. An assistant message that called tools carries its
 * `tool_calls` (an empty `content` is then sent as null); a `tool` message
 * answers one of them by `tool_call_id`. */
export type ChatMessage = { role: ChatRole; content: string; tool_calls?: ToolCall[]; tool_call_id?: string };
/** A function the model may call; `parameters` is a JSON Schema object. */
export type PlaygroundTool = { type: 'function'; function: { name: string; description?: string; parameters?: Record<string, unknown> } };
export type ToolChoice = 'auto' | 'none' | 'required' | { type: 'function'; function: { name: string } };

export type PlaygroundRequest = {
  model: string;
  messages: ChatMessage[];
  max_tokens?: number;
  temperature?: number;
  tools?: PlaygroundTool[];
  tool_choice?: ToolChoice;
  parallel_tool_calls?: boolean;
};

export type Language = 'curl' | 'python' | 'javascript' | 'rust' | 'go' | 'java' | 'cpp';
export type CodeStyle = 'client' | 'raw';

export const languages: readonly (readonly [Language, string])[] = [
  ['curl', 'curl'],
  ['python', 'Python'],
  ['javascript', 'JavaScript'],
  ['rust', 'Rust'],
  ['go', 'Go'],
  ['java', 'Java'],
  ['cpp', 'C/C++'],
];

export type Snippet = {
  language: Language;
  /** The style shown: a language without a client is always raw. */
  style: CodeStyle;
  /** For a code block's language class. */
  syntax: string;
  /** How to get what the code needs, shown above it. */
  setup: string;
  code: string;
  /** Why the chosen style is not the one shown, when it is not. */
  note?: string;
};

export const CHAT_PATH = '/api/inference/v1/chat/completions';
export const STREAM_PATH = '/api/inference/v1/chat/completions/stream';
const REPO = 'https://github.com/qucelerate/ai-gateway';

/** Languages whose gateway client exists. */
export function hasClient(language: Language): boolean {
  return language !== 'curl' && language !== 'javascript';
}

/** The JSON body the gateway receives, fields in the contract's order. */
export function requestBody(request: PlaygroundRequest): Record<string, unknown> {
  const body: Record<string, unknown> = { model: request.model, messages: request.messages.map(wireMessage) };
  if (request.temperature !== undefined) body.temperature = request.temperature;
  if (request.max_tokens !== undefined) body.max_tokens = request.max_tokens;
  if (request.tools?.length) body.tools = request.tools.map(wireTool);
  if (request.tool_choice !== undefined) body.tool_choice = request.tool_choice;
  if (request.parallel_tool_calls !== undefined) body.parallel_tool_calls = request.parallel_tool_calls;
  return body;
}

/** A message as the contract has it: only the fields it uses. */
function wireMessage(message: ChatMessage): Record<string, unknown> {
  const calls = message.tool_calls?.length ? message.tool_calls : undefined;
  return {
    role: message.role,
    content: calls && message.content === '' ? null : message.content,
    ...(calls ? { tool_calls: calls.map(call => ({ id: call.id, type: 'function', function: { name: call.function.name, arguments: call.function.arguments } })) } : {}),
    ...(message.tool_call_id !== undefined ? { tool_call_id: message.tool_call_id } : {}),
  };
}

function wireTool(tool: PlaygroundTool): Record<string, unknown> {
  const { name, description, parameters } = tool.function;
  return {
    type: 'function',
    function: { name, ...(description !== undefined ? { description } : {}), ...(parameters !== undefined ? { parameters } : {}) },
  };
}

const hasTools = (request: PlaygroundRequest) => Boolean(request.tools?.length);
/** The function a `tool_choice` names, when it names one. */
const choiceName = (choice: ToolChoice | undefined) => (typeof choice === 'object' ? choice.function.name : null);

/** A JSON value as a literal of a language whose arrays and objects are
 * written as JSON's: `string` writes strings, `keyword` true, false and
 * null. Objects and arrays go on one line, `{ … }` spaced when `spaced`. */
function literal(value: unknown, string: (text: string) => string, keyword: (word: 'true' | 'false' | 'null') => string, spaced = false): string {
  const self = (inner: unknown) => literal(inner, string, keyword, spaced);
  if (value === null || value === undefined) return keyword('null');
  if (typeof value === 'boolean') return keyword(value ? 'true' : 'false');
  if (typeof value === 'number') return String(value);
  if (typeof value === 'string') return string(value);
  if (Array.isArray(value)) return `[${value.map(self).join(', ')}]`;
  const entries = Object.entries(value as Record<string, unknown>).map(([key, inner]) => `${string(key)}: ${self(inner)}`);
  if (entries.length === 0) return '{}';
  return spaced ? `{ ${entries.join(', ')} }` : `{${entries.join(', ')}}`;
}

export function snippet(language: Language, style: CodeStyle, request: PlaygroundRequest, options: { baseUrl: string; stream: boolean }): Snippet {
  const baseUrl = options.baseUrl.replace(/\/+$/, '');
  const shown: CodeStyle = hasClient(language) ? style : 'raw';
  const note = style === 'client' && shown === 'raw'
    ? language === 'javascript'
      ? 'There is no JavaScript client yet, so this is raw fetch for Node.js 18 or later.'
      : 'curl is raw HTTP; choose another language for a client.'
    : undefined;
  const make = generators[language][shown];
  const { setup, code } = make(request, baseUrl, options.stream);
  return { language, style: shown, syntax: syntaxOf[language], setup, code, ...(note ? { note } : {}) };
}

const syntaxOf: Record<Language, string> = {
  curl: 'sh', python: 'python', javascript: 'javascript', rust: 'rust', go: 'go', java: 'java', cpp: 'cpp',
};

type Generator = (request: PlaygroundRequest, baseUrl: string, stream: boolean) => { setup: string; code: string };

// ------------------------------------------------------------- escaping

/** A double-quoted string literal in C-like languages: `\uXXXX` for what
 * the language accepts it for, else `fallback(code)` for control
 * characters. */
function quoted(text: string, control: (code: number) => string): string {
  let out = '"';
  for (const char of text) {
    const code = char.codePointAt(0)!;
    if (char === '"') out += '\\"';
    else if (char === '\\') out += '\\\\';
    else if (char === '\n') out += '\\n';
    else if (char === '\r') out += '\\r';
    else if (char === '\t') out += '\\t';
    else if (code < 0x20 || code === 0x7f || code === 0x2028 || code === 0x2029) out += control(code);
    else out += char;
  }
  return out + '"';
}

const hex4 = (code: number) => code.toString(16).padStart(4, '0');
/** JSON, JavaScript, Python and Go accept `\uXXXX`. */
const jsonString = (text: string) => quoted(text, code => `\\u${hex4(code)}`);
/** Rust wants `\u{…}`. */
const rustString = (text: string) => quoted(text, code => `\\u{${code.toString(16)}}`);
/** Java translates `\uXXXX` before lexing (so `\u000a` would end the
 * line); octal escapes are safe. */
const javaString = (text: string) => quoted(text, code => (code < 0x100 ? `\\${code.toString(8).padStart(3, '0')}` : `\\u${hex4(code)}`));
/** C and C++: octal escapes take at most three digits, unlike greedy `\x`;
 * U+2028 and U+2029 stay themselves in a UTF-8 literal. */
const cString = (text: string) =>
  quoted(text, code => (code < 0x100 ? `\\${code.toString(8).padStart(3, '0')}` : String.fromCodePoint(code)))
    // `??` starts a trigraph in older C.
    .replace(/\?(?=\?)/g, '?\\');
/** A POSIX shell single-quoted word. */
const shellQuoted = (text: string) => `'${text.replace(/'/g, `'\\''`)}'`;

/** A number as every language here writes a float literal. */
const float = (value: number) => (Number.isInteger(value) ? value.toFixed(1) : String(value));

/** The body as JSON, with U+2028 and U+2029 escaped so no source shows
 * them raw. */
function json(request: PlaygroundRequest, indent = 0): string {
  return JSON.stringify(requestBody(request), null, indent || undefined)
    .replace(/\u2028/g, '\\u2028')
    .replace(/\u2029/g, '\\u2029');
}

// ----------------------------------------------------------------- curl

const curl: Record<CodeStyle, Generator> = {
  raw: (request, baseUrl, stream) => ({
    setup: 'Needs curl and uuidgen. Set GATEWAY_API_KEY to an API key with the Inference scope.',
    code: [
      `curl ${stream ? '-N ' : ''}${shellQuoted(`${baseUrl}${stream ? STREAM_PATH : CHAT_PATH}`)} \\`,
      '  -H "Authorization: Bearer $GATEWAY_API_KEY" \\',
      '  -H "Content-Type: application/json" \\',
      '  -H "Idempotency-Key: $(uuidgen)" \\',
      `  -d ${shellQuoted(json(request, 2))}`,
    ].join('\n'),
  }),
  client: (...args) => curl.raw(...args),
};

// --------------------------------------------------------------- Python

const pyKeywords = { true: 'True', false: 'False', null: 'None' } as const;
/** A JSON value as a Python literal. */
const pyLiteral = (value: unknown) => literal(value, jsonString, word => pyKeywords[word]);
const pyList = (items: unknown[]) => `[\n${items.map(item => `    ${pyLiteral(item)},`).join('\n')}\n]`;
const pyMessages = (request: PlaygroundRequest) => pyList(request.messages.map(wireMessage));
const pyTools = (request: PlaygroundRequest) => pyList((request.tools ?? []).map(wireTool));

function pyKwargs(request: PlaygroundRequest): string {
  const kwargs = [];
  if (request.temperature !== undefined) kwargs.push(`temperature=${float(request.temperature)}`);
  if (request.max_tokens !== undefined) kwargs.push(`max_tokens=${request.max_tokens}`);
  if (hasTools(request)) kwargs.push('tools=tools');
  if (request.tool_choice !== undefined) kwargs.push(`tool_choice=${pyLiteral(request.tool_choice)}`);
  if (request.parallel_tool_calls !== undefined) kwargs.push(`parallel_tool_calls=${pyLiteral(request.parallel_tool_calls)}`);
  return kwargs.map(kwarg => `, ${kwarg}`).join('');
}

const python: Record<CodeStyle, Generator> = {
  client: (request, baseUrl, stream) => ({
    setup: `pip install qucelerate-ai-gateway  # Python 3.9+`,
    code: [
      'import os',
      '',
      stream
        ? `from qucelerate_ai_gateway import ChoiceDelta, Client, ResponseCompleted${hasTools(request) ? ', ToolCallDelta' : ''}`
        : 'from qucelerate_ai_gateway import Client',
      '',
      `messages = ${pyMessages(request)}`,
      ...(hasTools(request) ? [`tools = ${pyTools(request)}`] : []),
      '',
      `with Client(${jsonString(baseUrl)}, os.environ["GATEWAY_API_KEY"]) as client:`,
      ...(stream
        ? [
            `    with client.chat_stream(${jsonString(request.model)}, messages${pyKwargs(request)}) as stream:`,
            '        for event in stream:',
            '            if isinstance(event, ChoiceDelta):',
            '                print(event.content_delta or "", end="", flush=True)',
            ...(hasTools(request)
              ? [
                  '            elif isinstance(event, ToolCallDelta):',
                  '                if event.name:',
                  '                    print("\\ncall", event.name, end=" ")',
                  '                print(event.arguments_delta, end="", flush=True)',
                ]
              : []),
            '            elif isinstance(event, ResponseCompleted):',
            '                print("\\nusage:", event.usage)',
          ]
        : [
            `    response = client.chat(${jsonString(request.model)}, messages${pyKwargs(request)})`,
            '    print(response.content)',
            ...(hasTools(request)
              ? [
                  '    # Run each call, then send its result as a "tool" message.',
                  '    for call in response.choices[0].message.tool_calls:',
                  '        print("call", call.name, call.arguments)',
                ]
              : []),
            '    print("usage:", response.usage)',
          ]),
    ].join('\n'),
  }),
  raw: (request, baseUrl, stream) => ({
    setup: 'Standard library only (Python 3.8+).',
    code: [
      'import json',
      'import os',
      'import urllib.request',
      'import uuid',
      '',
      `body = ${pyBody(request)}`,
      'request = urllib.request.Request(',
      `    ${jsonString(`${baseUrl}${stream ? STREAM_PATH : CHAT_PATH}`)},`,
      '    data=json.dumps(body).encode(),',
      '    headers={',
      '        "Authorization": "Bearer " + os.environ["GATEWAY_API_KEY"],',
      '        "Content-Type": "application/json",',
      '        "Idempotency-Key": str(uuid.uuid4()),',
      '    },',
      ')',
      ...(stream
        ? [
            '# Server-sent events: an `event:` line names each `data:` line.',
            'with urllib.request.urlopen(request) as response:',
            '    event = None',
            '    for raw in response:',
            '        line = raw.decode().rstrip("\\r\\n")',
            '        if line.startswith("event:"):',
            '            event = line[len("event:"):].strip()',
            '        elif line.startswith("data:"):',
            '            data = json.loads(line[len("data:"):])',
            '            if event == "choice.delta":',
            '                print(data.get("content_delta") or "", end="", flush=True)',
            ...(hasTools(request)
              ? [
                  '            elif event == "tool_call.delta":',
                  '                if data.get("name"):',
                  '                    print("\\ncall", data["name"], end=" ")',
                  '                print(data["arguments_delta"], end="", flush=True)',
                ]
              : []),
            '            elif event == "response.completed":',
            '                print("\\nusage:", data.get("usage"))',
            '            elif event == "error":',
            '                raise RuntimeError(data)',
          ]
        : [
            'with urllib.request.urlopen(request) as response:',
            '    answer = json.load(response)',
            'print(answer["choices"][0]["message"]["content"])',
            ...(hasTools(request) ? ['print("tool calls:", answer["choices"][0]["message"].get("tool_calls"))'] : []),
            'print("usage:", answer.get("usage"))',
          ]),
    ].join('\n'),
  }),
};

/** The body as a Python literal. */
function pyBody(request: PlaygroundRequest): string {
  const fields = [`    "model": ${jsonString(request.model)},`, `    "messages": ${pyMessages(request).replace(/\n/g, '\n    ')},`];
  if (request.temperature !== undefined) fields.push(`    "temperature": ${float(request.temperature)},`);
  if (request.max_tokens !== undefined) fields.push(`    "max_tokens": ${request.max_tokens},`);
  if (hasTools(request)) fields.push(`    "tools": ${pyTools(request).replace(/\n/g, '\n    ')},`);
  if (request.tool_choice !== undefined) fields.push(`    "tool_choice": ${pyLiteral(request.tool_choice)},`);
  if (request.parallel_tool_calls !== undefined) fields.push(`    "parallel_tool_calls": ${pyLiteral(request.parallel_tool_calls)},`);
  return `{\n${fields.join('\n')}\n}`;
}

// ----------------------------------------------------------- JavaScript

const javascript: Record<CodeStyle, Generator> = {
  raw: (request, baseUrl, stream) => ({
    setup: 'Node.js 18 or later, no dependencies: save as chat.mjs and run GATEWAY_API_KEY=… node chat.mjs. (In a browser, never ship a key to the page; call the gateway from your server.)',
    code: [
      `const response = await fetch(${jsonString(`${baseUrl}${stream ? STREAM_PATH : CHAT_PATH}`)}, {`,
      '  method: "POST",',
      '  headers: {',
      '    Authorization: `Bearer ${process.env.GATEWAY_API_KEY}`,',
      '    "Content-Type": "application/json",',
      '    "Idempotency-Key": crypto.randomUUID(),',
      '  },',
      `  body: JSON.stringify(${jsBody(request)}),`,
      '});',
      'if (!response.ok) throw new Error(`${response.status}: ${await response.text()}`);',
      ...(stream
        ? [
            '// Server-sent events: an `event:` line names each `data:` line.',
            'const reader = response.body.getReader();',
            'const decoder = new TextDecoder();',
            'let buffered = "";',
            'let event = null;',
            'for (;;) {',
            '  const { done, value } = await reader.read();',
            '  if (done) break;',
            '  buffered += decoder.decode(value, { stream: true });',
            '  const lines = buffered.split("\\n");',
            '  buffered = lines.pop();',
            '  for (const line of lines.map(text => text.replace(/\\r$/, ""))) {',
            '    if (line.startsWith("event:")) event = line.slice(6).trim();',
            '    else if (line.startsWith("data:")) {',
            '      const data = JSON.parse(line.slice(5));',
            '      if (event === "choice.delta") process.stdout.write(data.content_delta ?? "");',
            ...(hasTools(request)
              ? ['      else if (event === "tool_call.delta") process.stdout.write((data.name ? `\\ncall ${data.name} ` : "") + data.arguments_delta);']
              : []),
            '      else if (event === "response.completed") console.log("\\nusage:", data.usage);',
            '      else if (event === "error") throw new Error(JSON.stringify(data));',
            '    }',
            '  }',
            '}',
          ]
        : [
            'const answer = await response.json();',
            'console.log(answer.choices[0].message.content);',
            ...(hasTools(request) ? ['console.log("tool calls:", answer.choices[0].message.tool_calls);'] : []),
            'console.log("usage:", answer.usage);',
          ]),
    ].join('\n'),
  }),
  client: (...args) => javascript.raw(...args),
};

function jsBody(request: PlaygroundRequest): string {
  return json(request, 2).replace(/\n/g, '\n  ');
}

// ----------------------------------------------------------------- Rust

/** A JSON value as a `serde_json::json!` literal. */
const rustLiteral = (value: unknown) => literal(value, rustString, word => word, true);

function rustMessage(m: ChatMessage): string {
  if (m.role === 'tool') return `Message::tool(${rustString(m.tool_call_id ?? '')}, ${rustString(m.content)})`;
  if (m.tool_calls?.length) {
    const calls = m.tool_calls.map(call => `ToolCall::new(${rustString(call.id)}, ${rustString(call.function.name)}, ${rustString(call.function.arguments)})`);
    return `Message::assistant_tool_calls(${rustString(m.content)}, vec![${calls.join(', ')}])`;
  }
  return `Message::${m.role}(${rustString(m.content)})`;
}

function rustTool(tool: PlaygroundTool): string {
  const { name, description, parameters } = tool.function;
  return [
    `Tool::function(${rustString(name)})`,
    ...(description !== undefined ? [`.description(${rustString(description)})`] : []),
    ...(parameters !== undefined ? [`.parameters(serde_json::json!(${rustLiteral(parameters)}))`] : []),
  ].join('');
}

function rustChoice(choice: ToolChoice): string {
  if (typeof choice === 'object') return `ToolChoice::Function(${rustString(choice.function.name)}.into())`;
  return `ToolChoice::${choice === 'auto' ? 'Auto' : choice === 'none' ? 'None' : 'Required'}`;
}

/** The client's names a Rust snippet uses. */
function rustImports(request: PlaygroundRequest, stream: boolean): string {
  const names = ['ChatRequest', 'Client', 'Message'];
  if (stream) names.push('StreamEvent');
  if (hasTools(request)) names.push('Tool');
  if (request.messages.some(m => m.role !== 'tool' && m.tool_calls?.length)) names.push('ToolCall');
  if (request.tool_choice !== undefined) names.push('ToolChoice');
  return `use qucelerate_ai_gateway::{${names.join(', ')}};`;
}

const rust: Record<CodeStyle, Generator> = {
  client: (request, baseUrl, stream) => {
    const builder = [
      `ChatRequest::new(`,
      `        ${rustString(request.model)},`,
      `        vec![`,
      ...request.messages.map(m => `            ${rustMessage(m)},`),
      `        ],`,
      `    )`,
      ...(request.temperature !== undefined ? [`    .temperature(${float(request.temperature)})`] : []),
      ...(request.max_tokens !== undefined ? [`    .max_tokens(${request.max_tokens})`] : []),
      ...(hasTools(request) ? [`    .tools(vec![`, ...(request.tools ?? []).map(tool => `        ${rustTool(tool)},`), `    ])`] : []),
      ...(request.tool_choice !== undefined ? [`    .tool_choice(${rustChoice(request.tool_choice)})`] : []),
      ...(request.parallel_tool_calls !== undefined ? [`    .parallel_tool_calls(${request.parallel_tool_calls})`] : []),
    ];
    return {
      setup: [
        '# Cargo.toml',
        '[dependencies]',
        'qucelerate-ai-gateway = "0.2"',
        'tokio = { version = "1", features = ["macros", "rt-multi-thread"] }',
        ...(stream ? ['futures-util = "0.3"'] : []),
        ...(request.tools?.some(tool => tool.function.parameters !== undefined) ? ['serde_json = "1"'] : []),
      ].join('\n'),
      code: [
        ...(stream ? ['use futures_util::StreamExt;'] : []),
        rustImports(request, stream),
        '',
        '#[tokio::main]',
        'async fn main() -> Result<(), Box<dyn std::error::Error>> {',
        `    let client = Client::builder(${rustString(baseUrl)})`,
        '        .api_key(std::env::var("GATEWAY_API_KEY")?)',
        '        .build()?;',
        `    let request = ${builder.join('\n    ')};`,
        ...(stream
          ? [
              '',
              '    let mut stream = client.chat_stream(&request).await?;',
              '    while let Some(event) = stream.next().await {',
              '        match event? {',
              '            StreamEvent::ChoiceDelta(delta) => print!("{}", delta.content_delta.unwrap_or_default()),',
              ...(hasTools(request)
                ? [
                    '            StreamEvent::ToolCallDelta(call) => {',
                    '                if let Some(name) = call.name {',
                    '                    print!("\\ncall {name} ");',
                    '                }',
                    '                print!("{}", call.arguments_delta);',
                    '            }',
                  ]
                : []),
              '            StreamEvent::ResponseCompleted(done) => println!("\\nusage: {:?}", done.usage),',
              '            _ => {}',
              '        }',
              '    }',
            ]
          : [
              '',
              '    let response = client.chat(&request).await?;',
              '    println!("{}", response.content().unwrap_or_default());',
              ...(hasTools(request)
                ? [
                    '    // Run each call, then send its result with Message::tool.',
                    '    for call in &response.choices[0].message.tool_calls {',
                    '        println!("call {} {}", call.function.name, call.function.arguments);',
                    '    }',
                  ]
                : []),
              '    println!("usage: {:?}", response.usage);',
            ]),
        '    Ok(())',
        '}',
      ].join('\n'),
    };
  },
  raw: (request, baseUrl, stream) => ({
    setup: [
      '# Cargo.toml',
      '[dependencies]',
      'reqwest = { version = "0.12", default-features = false, features = ["blocking", "json", "rustls-tls"] }',
      'serde_json = "1"',
    ].join('\n'),
    code: [
      ...(stream ? ['use std::io::{BufRead, BufReader, Write};'] : []),
      'use std::collections::hash_map::RandomState;',
      'use std::hash::{BuildHasher, Hasher};',
      'use std::time::{SystemTime, UNIX_EPOCH};',
      '',
      'fn main() -> Result<(), Box<dyn std::error::Error>> {',
      '    // Any string unique to this call; retries of it reuse it.',
      '    // RandomState is seeded from the operating system per process.',
      '    let random = RandomState::new().build_hasher().finish();',
      '    let idempotency_key = format!("{:016x}{:x}", random, SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos());',
      `    let body = serde_json::json!(${rustJson(request)});`,
      '    let response = reqwest::blocking::Client::new()',
      `        .post(${rustString(`${baseUrl}${stream ? STREAM_PATH : CHAT_PATH}`)})`,
      '        .bearer_auth(std::env::var("GATEWAY_API_KEY")?)',
      '        .header("Idempotency-Key", idempotency_key)',
      '        .json(&body)',
      '        .send()?',
      '        .error_for_status()?;',
      ...(stream
        ? [
            '    // Server-sent events: an `event:` line names each `data:` line.',
            '    let mut event = String::new();',
            '    for line in BufReader::new(response).lines() {',
            '        let line = line?;',
            '        if let Some(name) = line.strip_prefix("event:") {',
            '            event = name.trim().to_owned();',
            '        } else if let Some(data) = line.strip_prefix("data:") {',
            '            let data: serde_json::Value = serde_json::from_str(data)?;',
            '            match event.as_str() {',
            '                "choice.delta" => {',
            '                    print!("{}", data["content_delta"].as_str().unwrap_or_default());',
            '                    std::io::stdout().flush()?;',
            '                }',
            ...(hasTools(request)
              ? [
                  '                "tool_call.delta" => {',
                  '                    if let Some(name) = data["name"].as_str() {',
                  '                        print!("\\ncall {name} ");',
                  '                    }',
                  '                    print!("{}", data["arguments_delta"].as_str().unwrap_or_default());',
                  '                    std::io::stdout().flush()?;',
                  '                }',
                ]
              : []),
            '                "response.completed" => println!("\\nusage: {}", data["usage"]),',
            '                "error" => return Err(data.to_string().into()),',
            '                _ => {}',
            '            }',
            '        }',
            '    }',
          ]
        : [
            '    let answer: serde_json::Value = response.json()?;',
            '    println!("{}", answer["choices"][0]["message"]["content"].as_str().unwrap_or_default());',
            ...(hasTools(request) ? ['    println!("tool calls: {}", answer["choices"][0]["message"]["tool_calls"]);'] : []),
            '    println!("usage: {}", answer["usage"]);',
          ]),
      '    Ok(())',
      '}',
    ].join('\n'),
  }),
};

/** The body as a `serde_json::json!` literal: JSON with Rust string literals. */
function rustJson(request: PlaygroundRequest): string {
  const lines = [
    '{',
    `        "model": ${rustString(request.model)},`,
    '        "messages": [',
    ...request.messages.map(m => `            ${rustLiteral(wireMessage(m))},`),
    '        ],',
    ...(request.temperature !== undefined ? [`        "temperature": ${float(request.temperature)},`] : []),
    ...(request.max_tokens !== undefined ? [`        "max_tokens": ${request.max_tokens},`] : []),
    ...(hasTools(request) ? ['        "tools": [', ...(request.tools ?? []).map(tool => `            ${rustLiteral(wireTool(tool))},`), '        ],'] : []),
    ...(request.tool_choice !== undefined ? [`        "tool_choice": ${rustLiteral(request.tool_choice)},`] : []),
    ...(request.parallel_tool_calls !== undefined ? [`        "parallel_tool_calls": ${request.parallel_tool_calls},`] : []),
    '    }',
  ];
  return lines.join('\n');
}

// ------------------------------------------------------------------- Go

const goRole: Record<ChatRole, string> = { system: 'RoleSystem', developer: 'RoleDeveloper', user: 'RoleUser', assistant: 'RoleAssistant', tool: 'RoleTool' };

function goMessage(m: ChatMessage): string {
  const fields = [`Role: gateway.${goRole[m.role]}`, `Content: ${jsonString(m.content)}`];
  if (m.tool_calls?.length) {
    const calls = m.tool_calls.map(call => `{ID: ${jsonString(call.id)}, Type: "function", Function: gateway.FunctionCall{Name: ${jsonString(call.function.name)}, Arguments: ${jsonString(call.function.arguments)}}}`);
    fields.push(`ToolCalls: []gateway.ToolCall{${calls.join(', ')}}`);
  }
  if (m.tool_call_id !== undefined) fields.push(`ToolCallID: ${jsonString(m.tool_call_id)}`);
  return `{${fields.join(', ')}}`;
}

function goTool(tool: PlaygroundTool): string {
  const { name, description, parameters } = tool.function;
  const schema = parameters === undefined ? 'nil' : `json.RawMessage(${goRawString(JSON.stringify(parameters).replace(/\u2028/g, '\\u2028').replace(/\u2029/g, '\\u2029'))})`;
  return `gateway.FunctionTool(${jsonString(name)}, ${jsonString(description ?? '')}, ${schema})`;
}

const goNeedsJson = (request: PlaygroundRequest) => Boolean(request.tools?.some(tool => tool.function.parameters !== undefined));

function goChoice(choice: ToolChoice): string {
  if (typeof choice === 'object') return `gateway.ToolChoiceFunction(${jsonString(choice.function.name)})`;
  return `gateway.ToolChoice${choice === 'auto' ? 'Auto' : choice === 'none' ? 'None' : 'Required'}`;
}

const go: Record<CodeStyle, Generator> = {
  client: (request, baseUrl, stream) => ({
    setup: 'go get github.com/qucelerate/ai-gateway/clients/go  # Go 1.22+',
    code: [
      'package main',
      '',
      'import (',
      '\t"context"',
      ...(goNeedsJson(request) ? ['\t"encoding/json"'] : []),
      '\t"fmt"',
      '\t"log"',
      '\t"os"',
      '',
      '\tgateway "github.com/qucelerate/ai-gateway/clients/go"',
      ')',
      '',
      'func main() {',
      `\tclient, err := gateway.NewClient(${jsonString(baseUrl)}, os.Getenv("GATEWAY_API_KEY"))`,
      '\tif err != nil {',
      '\t\tlog.Fatal(err)',
      '\t}',
      ...(request.tool_choice !== undefined ? [`\ttoolChoice := ${goChoice(request.tool_choice)}`] : []),
      '\trequest := gateway.ChatRequest{',
      `\t\tModel: ${jsonString(request.model)},`,
      '\t\tMessages: []gateway.Message{',
      ...request.messages.map(m => `\t\t\t${goMessage(m)},`),
      '\t\t},',
      ...(request.temperature !== undefined ? [`\t\tTemperature: gateway.Float(${float(request.temperature)}),`] : []),
      ...(request.max_tokens !== undefined ? [`\t\tMaxTokens: gateway.Int(${request.max_tokens}),`] : []),
      ...(hasTools(request) ? ['\t\tTools: []gateway.Tool{', ...(request.tools ?? []).map(tool => `\t\t\t${goTool(tool)},`), '\t\t},'] : []),
      ...(request.tool_choice !== undefined ? ['\t\tToolChoice: &toolChoice,'] : []),
      ...(request.parallel_tool_calls !== undefined ? [`\t\tParallelToolCalls: gateway.Bool(${request.parallel_tool_calls}),`] : []),
      '\t}',
      ...(stream
        ? [
            '\tstream, err := client.ChatStream(context.Background(), request)',
            '\tif err != nil {',
            '\t\tlog.Fatal(err)',
            '\t}',
            '\tdefer stream.Close()',
            '\tfor stream.Next() {',
            '\t\tswitch event := stream.Event().(type) {',
            '\t\tcase gateway.ChoiceDelta:',
            '\t\t\tif event.ContentDelta != nil {',
            '\t\t\t\tfmt.Print(*event.ContentDelta)',
            '\t\t\t}',
            ...(hasTools(request)
              ? [
                  '\t\tcase gateway.ToolCallDelta:',
                  '\t\t\tif event.Name != nil {',
                  '\t\t\t\tfmt.Print("\\ncall ", *event.Name, " ")',
                  '\t\t\t}',
                  '\t\t\tfmt.Print(event.ArgumentsDelta)',
                ]
              : []),
            '\t\tcase gateway.ResponseCompleted:',
            '\t\t\tfmt.Println()',
            '\t\t}',
            '\t}',
            '\tif err := stream.Err(); err != nil {',
            '\t\tlog.Fatal(err)',
            '\t}',
          ]
        : [
            '\tresponse, err := client.Chat(context.Background(), request)',
            '\tif err != nil {',
            '\t\tlog.Fatal(err)',
            '\t}',
            '\tfmt.Println(response.Content())',
            ...(hasTools(request)
              ? [
                  '\t// Run each call, then send its result as a RoleTool message.',
                  '\tfor _, call := range response.Choices[0].Message.ToolCalls {',
                  '\t\tfmt.Println("call", call.Function.Name, call.Function.Arguments)',
                  '\t}',
                ]
              : []),
          ]),
      '}',
    ].join('\n'),
  }),
  raw: (request, baseUrl, stream) => ({
    setup: 'Standard library only (Go 1.20+).',
    code: [
      'package main',
      '',
      'import (',
      ...(stream ? ['\t"bufio"'] : []),
      '\t"bytes"',
      '\t"crypto/rand"',
      '\t"encoding/hex"',
      '\t"encoding/json"',
      '\t"fmt"',
      '\t"io"',
      '\t"log"',
      '\t"net/http"',
      '\t"os"',
      ...(stream ? ['\t"strings"'] : []),
      ')',
      '',
      'func main() {',
      `\tbody := []byte(${goRawString(json(request, 2))})`,
      `\treq, err := http.NewRequest("POST", ${jsonString(`${baseUrl}${stream ? STREAM_PATH : CHAT_PATH}`)}, bytes.NewReader(body))`,
      '\tif err != nil {',
      '\t\tlog.Fatal(err)',
      '\t}',
      '\tkey := make([]byte, 16)',
      '\tif _, err := rand.Read(key); err != nil {',
      '\t\tlog.Fatal(err)',
      '\t}',
      '\treq.Header.Set("Authorization", "Bearer "+os.Getenv("GATEWAY_API_KEY"))',
      '\treq.Header.Set("Content-Type", "application/json")',
      '\treq.Header.Set("Idempotency-Key", hex.EncodeToString(key))',
      '\tresp, err := http.DefaultClient.Do(req)',
      '\tif err != nil {',
      '\t\tlog.Fatal(err)',
      '\t}',
      '\tdefer resp.Body.Close()',
      '\tif resp.StatusCode != http.StatusOK {',
      '\t\tproblem, _ := io.ReadAll(resp.Body)',
      '\t\tlog.Fatalf("%s: %s", resp.Status, problem)',
      '\t}',
      ...(stream
        ? [
            '\t// Server-sent events: an `event:` line names each `data:` line.',
            '\tevent := ""',
            '\tscanner := bufio.NewScanner(resp.Body)',
            '\tscanner.Buffer(make([]byte, 0, 64*1024), 1<<20)',
            '\tfor scanner.Scan() {',
            '\t\tline := strings.TrimRight(scanner.Text(), "\\r")',
            '\t\tif name, ok := strings.CutPrefix(line, "event:"); ok {',
            '\t\t\tevent = strings.TrimSpace(name)',
            '\t\t} else if data, ok := strings.CutPrefix(line, "data:"); ok {',
            '\t\t\tvar fields map[string]any',
            '\t\t\tif err := json.Unmarshal([]byte(data), &fields); err != nil {',
            '\t\t\t\tlog.Fatal(err)',
            '\t\t\t}',
            '\t\t\tswitch event {',
            '\t\t\tcase "choice.delta":',
            '\t\t\t\tif text, ok := fields["content_delta"].(string); ok {',
            '\t\t\t\t\tfmt.Print(text)',
            '\t\t\t\t}',
            ...(hasTools(request)
              ? [
                  '\t\t\tcase "tool_call.delta":',
                  '\t\t\t\tif name, ok := fields["name"].(string); ok {',
                  '\t\t\t\t\tfmt.Print("\\ncall ", name, " ")',
                  '\t\t\t\t}',
                  '\t\t\t\tfmt.Print(fields["arguments_delta"])',
                ]
              : []),
            '\t\t\tcase "response.completed":',
            '\t\t\t\tfmt.Println("\\nusage:", fields["usage"])',
            '\t\t\tcase "error":',
            '\t\t\t\tlog.Fatal(data)',
            '\t\t\t}',
            '\t\t}',
            '\t}',
            '\tif err := scanner.Err(); err != nil {',
            '\t\tlog.Fatal(err)',
            '\t}',
          ]
        : [
            '\tvar answer struct {',
            '\t\tChoices []struct {',
            '\t\t\tMessage struct {',
            '\t\t\t\tContent string `json:"content"`',
            ...(hasTools(request) ? ['\t\t\t\tToolCalls []map[string]any `json:"tool_calls"`'] : []),
            '\t\t\t} `json:"message"`',
            '\t\t} `json:"choices"`',
            '\t\tUsage map[string]any `json:"usage"`',
            '\t}',
            '\tif err := json.NewDecoder(resp.Body).Decode(&answer); err != nil {',
            '\t\tlog.Fatal(err)',
            '\t}',
            '\tfmt.Println(answer.Choices[0].Message.Content)',
            ...(hasTools(request) ? ['\tfmt.Println("tool calls:", answer.Choices[0].Message.ToolCalls)'] : []),
            '\tfmt.Println("usage:", answer.Usage)',
          ]),
      '}',
    ].join('\n'),
  }),
};

/** A Go raw string when the text allows one, else an interpreted one. */
function goRawString(text: string): string {
  return text.includes('`') || text.includes('\r') ? jsonString(text) : `\`${text}\``;
}

// ----------------------------------------------------------------- Java

function javaMessage(m: ChatMessage): string {
  if (m.role === 'tool') return `Message.tool(${javaString(m.tool_call_id ?? '')}, ${javaString(m.content)})`;
  if (m.tool_calls?.length) {
    const calls = m.tool_calls.map(call => `ToolCall.of(${javaString(call.id)}, ${javaString(call.function.name)}, ${javaString(call.function.arguments)})`);
    return `Message.assistant(${javaString(m.content)}, List.of(${calls.join(', ')}))`;
  }
  return `Message.${m.role}(${javaString(m.content)})`;
}

const javaNeedsJson = (request: PlaygroundRequest) => Boolean(request.tools?.some(tool => tool.function.parameters !== undefined));

function javaTool(tool: PlaygroundTool): string {
  const { name, description, parameters } = tool.function;
  const schema = parameters === undefined ? 'null' : `json.readTree(${javaString(JSON.stringify(parameters))})`;
  return `Tool.function(${javaString(name)}, ${description !== undefined ? javaString(description) : 'null'}, ${schema})`;
}

function javaChoice(choice: ToolChoice): string {
  if (typeof choice === 'object') return `ToolChoice.function(${javaString(choice.function.name)})`;
  return `ToolChoice.${choice.toUpperCase()}`;
}

const java: Record<CodeStyle, Generator> = {
  client: (request, baseUrl, stream) => {
    const chain = [
      ...(request.temperature !== undefined ? [`.withTemperature(${float(request.temperature)})`] : []),
      ...(request.max_tokens !== undefined ? [`.withMaxTokens(${request.max_tokens})`] : []),
      ...(hasTools(request) ? [`\n                .withTools(List.of(\n${(request.tools ?? []).map(tool => `                        ${javaTool(tool)}`).join(',\n')}))`] : []),
      ...(request.tool_choice !== undefined ? [`\n                .withToolChoice(${javaChoice(request.tool_choice)})`] : []),
      ...(request.parallel_tool_calls !== undefined ? [`\n                .withParallelToolCalls(${request.parallel_tool_calls})`] : []),
    ].join('');
    const toolNames = [
      ...(hasTools(request) ? ['Tool'] : []),
      ...((hasTools(request) && !stream) || request.messages.some(m => m.tool_calls?.length) ? ['ToolCall'] : []),
      ...(request.tool_choice !== undefined ? ['ToolChoice'] : []),
    ];
    return {
      setup: `Java 17+. Depend on com.qucelerate:qucelerate-ai-gateway:0.2.0 (Maven Central).`,
      code: [
        'import com.qucelerate.aigateway.ChatRequest;',
        ...(stream ? ['import com.qucelerate.aigateway.ChatStream;'] : ['import com.qucelerate.aigateway.ChatResponse;']),
        'import com.qucelerate.aigateway.GatewayClient;',
        'import com.qucelerate.aigateway.Message;',
        ...(stream ? ['import com.qucelerate.aigateway.StreamEvent;'] : []),
        ...toolNames.map(name => `import com.qucelerate.aigateway.${name};`),
        ...(javaNeedsJson(request) ? ['import com.fasterxml.jackson.databind.ObjectMapper;'] : []),
        'import java.util.List;',
        '',
        'public class Chat {',
        `    public static void main(String[] args)${javaNeedsJson(request) ? ' throws Exception' : ''} {`,
        ...(javaNeedsJson(request) ? ['        ObjectMapper json = new ObjectMapper();'] : []),
        `        GatewayClient client = GatewayClient.builder(${javaString(baseUrl)})`,
        '                .apiKey(System.getenv("GATEWAY_API_KEY"))',
        '                .build();',
        `        ChatRequest request = new ChatRequest(${javaString(request.model)}, List.of(`,
        request.messages.map(m => `                ${javaMessage(m)}`).join(',\n'),
        `        ))${chain};`,
        ...(stream
          ? [
              '        try (ChatStream stream = client.chatStream(request)) {',
              '            for (StreamEvent event : stream) {',
              '                if (event instanceof StreamEvent.ChoiceDelta delta && delta.contentDelta() != null) {',
              '                    System.out.print(delta.contentDelta());',
              ...(hasTools(request)
                ? [
                    '                } else if (event instanceof StreamEvent.ToolCallDelta call) {',
                    '                    if (call.name() != null) {',
                    '                        System.out.print("\\ncall " + call.name() + " ");',
                    '                    }',
                    '                    System.out.print(call.argumentsDelta());',
                  ]
                : []),
              '                } else if (event instanceof StreamEvent.ResponseCompleted) {',
              '                    System.out.println();',
              '                }',
              '            }',
              '        }',
            ]
          : [
              '        ChatResponse response = client.chat(request);',
              '        System.out.println(response.content());',
              ...(hasTools(request)
                ? [
                    '        // Run each call, then send its result with Message.tool.',
                    '        for (ToolCall call : response.choices().get(0).message().toolCalls()) {',
                    '            System.out.println("call " + call.function().name() + " " + call.function().arguments());',
                    '        }',
                  ]
                : []),
              '        System.out.println("usage: " + response.usage());',
            ]),
        '    }',
        '}',
      ].join('\n'),
    };
  },
  raw: (request, baseUrl, stream) => ({
    setup: 'JDK 17+ only (java.net.http). Parse the JSON with the library you use; this prints it.',
    code: [
      'import java.net.URI;',
      'import java.net.http.HttpClient;',
      'import java.net.http.HttpRequest;',
      'import java.net.http.HttpResponse;',
      'import java.util.UUID;',
      '',
      'public class Chat {',
      '    public static void main(String[] args) throws Exception {',
      `        String body = ${javaString(json(request))};`,
      '        HttpRequest request = HttpRequest.newBuilder()',
      `                .uri(URI.create(${javaString(`${baseUrl}${stream ? STREAM_PATH : CHAT_PATH}`)}))`,
      '                .header("Authorization", "Bearer " + System.getenv("GATEWAY_API_KEY"))',
      '                .header("Content-Type", "application/json")',
      '                .header("Idempotency-Key", UUID.randomUUID().toString())',
      '                .POST(HttpRequest.BodyPublishers.ofString(body))',
      '                .build();',
      '        HttpClient http = HttpClient.newHttpClient();',
      ...(stream
        ? [
            '        // Server-sent events: an `event:` line names each `data:` line.',
            '        HttpResponse<java.util.stream.Stream<String>> response =',
            '                http.send(request, HttpResponse.BodyHandlers.ofLines());',
            '        if (response.statusCode() != 200) {',
            '            throw new IllegalStateException("HTTP " + response.statusCode());',
            '        }',
            '        String[] event = {""};',
            '        response.body().forEach(line -> {',
            '            if (line.startsWith("event:")) {',
            '                event[0] = line.substring(6).trim();',
            '            } else if (line.startsWith("data:")) {',
            '                System.out.println(event[0] + " " + line.substring(5).trim());',
            '            }',
            '        });',
          ]
        : [
            '        HttpResponse<String> response = http.send(request, HttpResponse.BodyHandlers.ofString());',
            '        System.out.println(response.statusCode() + " " + response.body());',
          ]),
      '    }',
      '}',
    ].join('\n'),
  }),
};

// ---------------------------------------------------------------- C/C++

function cppMessage(m: ChatMessage): string {
  if (m.role === 'tool') return `gateway::Message::tool(${cString(m.tool_call_id ?? '')}, ${cString(m.content)})`;
  if (m.tool_calls?.length) {
    const calls = m.tool_calls.map(call => `{${cString(call.id)}, ${cString(call.function.name)}, ${cString(call.function.arguments)}}`);
    return `gateway::Message::assistant(${cString(m.content)}, {${calls.join(', ')}})`;
  }
  return `gateway::Message::${m.role}(${cString(m.content)})`;
}

function cppTool(tool: PlaygroundTool): string {
  const { name, description, parameters } = tool.function;
  const optional = (text: string | undefined) => (text === undefined ? 'std::nullopt' : cString(text));
  return `{${cString(name)}, ${optional(description)}, ${optional(parameters === undefined ? undefined : JSON.stringify(parameters))}}`;
}

const cpp: Record<CodeStyle, Generator> = {
  client: (request, baseUrl, stream) => ({
    setup: `C++17. Build and install the C client from ${REPO} (clients/c: cmake -B build && cmake --build build && cmake --install build), then link qucelerate::ai-gateway or pkg-config qucelerate-ai-gateway.`,
    code: [
      '#include <gateway/gateway.hpp>',
      '',
      '#include <cstdlib>',
      '#include <iostream>',
      ...(stream ? ['#include <variant>'] : []),
      '',
      'int main() {',
      '    const char *api_key = std::getenv("GATEWAY_API_KEY");',
      '    if (!api_key) {',
      '        std::cerr << "set GATEWAY_API_KEY\\n";',
      '        return 2;',
      '    }',
      '    try {',
      `        gateway::Client client({${cString(baseUrl)}, api_key});`,
      `        gateway::ChatRequest request{${cString(request.model)}, {`,
      request.messages.map(m => `            ${cppMessage(m)}`).join(',\n'),
      '        }};',
      ...(request.temperature !== undefined ? [`        request.temperature = ${float(request.temperature)};`] : []),
      ...(request.max_tokens !== undefined ? [`        request.max_tokens = ${request.max_tokens};`] : []),
      ...(hasTools(request) ? ['        request.tools = {', ...(request.tools ?? []).map(tool => `            ${cppTool(tool)},`), '        };'] : []),
      ...(typeof request.tool_choice === 'string' ? [`        request.tool_choice = ${cString(request.tool_choice)};`] : []),
      ...(choiceName(request.tool_choice) !== null ? [`        request.tool_choice_function = ${cString(choiceName(request.tool_choice)!)};`] : []),
      ...(request.parallel_tool_calls !== undefined ? [`        request.parallel_tool_calls = ${request.parallel_tool_calls};`] : []),
      ...(stream
        ? [
            '        auto stream = client.chat_stream(request);',
            '        while (auto event = stream.next()) {',
            '            if (auto *delta = std::get_if<gateway::ChoiceDelta>(&*event); delta && delta->content_delta) {',
            '                std::cout << *delta->content_delta << std::flush;',
            ...(hasTools(request)
              ? [
                  '            } else if (auto *call = std::get_if<gateway::ToolCallDelta>(&*event)) {',
                  '                if (call->name) std::cout << "\\ncall " << *call->name << " ";',
                  '                std::cout << call->arguments_delta << std::flush;',
                ]
              : []),
            '            }',
            '        }',
            '        std::cout << "\\n";',
          ]
        : [
            '        auto response = client.chat(request);',
            '        std::cout << response.content() << "\\n";',
            ...(hasTools(request)
              ? [
                  '        // Run each call, then send its result with gateway::Message::tool.',
                  '        for (const auto &call : response.choices[0].tool_calls) {',
                  '            std::cout << "call " << call.name << " " << call.arguments << "\\n";',
                  '        }',
                ]
              : []),
          ]),
      '    } catch (const gateway::Error &error) {',
      '        std::cerr << error.what() << "\\n";',
      '        return 1;',
      '    }',
      '    return 0;',
      '}',
    ].join('\n'),
  }),
  raw: (request, baseUrl, stream) => ({
    setup: 'C with libcurl: cc chat.c $(pkg-config --cflags --libs libcurl). It prints the response as it arrives; parse the JSON with the library you use.',
    code: [
      '#include <curl/curl.h>',
      '#include <stdio.h>',
      '#include <stdlib.h>',
      '#include <string.h>',
      '',
      'static size_t print_chunk(char *data, size_t size, size_t count, void *unused) {',
      '    (void)unused;',
      '    fwrite(data, size, count, stdout);',
      '    fflush(stdout);',
      '    return size * count;',
      '}',
      '',
      'int main(void) {',
      '    const char *api_key = getenv("GATEWAY_API_KEY");',
      '    if (!api_key) {',
      '        fputs("set GATEWAY_API_KEY\\n", stderr);',
      '        return 2;',
      '    }',
      `    const char *body = ${cString(json(request))};`,
      '    char authorization[512];',
      '    snprintf(authorization, sizeof authorization, "Authorization: Bearer %s", api_key);',
      '    /* Any string unique to this call; retries of it reuse it. */',
      '    /* 16 random bytes from the operating system (POSIX). */',
      '    unsigned char random[16];',
      '    FILE *urandom = fopen("/dev/urandom", "rb");',
      '    if (!urandom || fread(random, 1, sizeof random, urandom) != sizeof random) {',
      '        fputs("cannot read /dev/urandom\\n", stderr);',
      '        return 2;',
      '    }',
      '    fclose(urandom);',
      '    char idempotency[64] = "Idempotency-Key: ";',
      '    size_t used = strlen(idempotency);',
      '    for (size_t i = 0; i < sizeof random; i++) {',
      '        snprintf(idempotency + used + 2 * i, sizeof idempotency - used - 2 * i, "%02x", random[i]);',
      '    }',
      '',
      '    curl_global_init(CURL_GLOBAL_DEFAULT);',
      '    CURL *curl = curl_easy_init();',
      '    struct curl_slist *headers = NULL;',
      '    headers = curl_slist_append(headers, authorization);',
      '    headers = curl_slist_append(headers, "Content-Type: application/json");',
      '    headers = curl_slist_append(headers, idempotency);',
      `    curl_easy_setopt(curl, CURLOPT_URL, ${cString(`${baseUrl}${stream ? STREAM_PATH : CHAT_PATH}`)});`,
      '    curl_easy_setopt(curl, CURLOPT_HTTPHEADER, headers);',
      '    curl_easy_setopt(curl, CURLOPT_POSTFIELDS, body);',
      '    curl_easy_setopt(curl, CURLOPT_WRITEFUNCTION, print_chunk);',
      '    curl_easy_setopt(curl, CURLOPT_FAILONERROR, 1L);',
      '    CURLcode result = curl_easy_perform(curl);',
      '    if (result != CURLE_OK) {',
      '        fprintf(stderr, "%s\\n", curl_easy_strerror(result));',
      '    }',
      '    curl_slist_free_all(headers);',
      '    curl_easy_cleanup(curl);',
      '    curl_global_cleanup();',
      '    return result == CURLE_OK ? 0 : 1;',
      '}',
    ].join('\n'),
  }),
};

const generators: Record<Language, Record<CodeStyle, Generator>> = {
  curl, python, javascript, rust, go, java, cpp,
};
