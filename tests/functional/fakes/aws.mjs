// Fake AWS for the functional suite: STS AssumeRole, the Bedrock control
// plane's model listings and Bedrock Runtime's Converse and ConverseStream. It shares the
// gateway container's network, so the gateway reaches it on loopback (the
// only plain-HTTP endpoints it accepts). Signatures are not checked; what
// each request carried is recorded for the suite to assert on.
import { createServer } from 'node:http';
import { crc32 } from 'node:zlib';

const PORT = Number(process.env.FAKES_PORT ?? 4599);
const ASSUMED_KEY = 'ASIAFUNCTIONALTEST01';

export const FOUNDATION_MODELS = [
  { modelId: 'anthropic.claude-3-5-haiku-20241022-v1:0', modelName: 'Claude 3.5 Haiku' },
  { modelId: 'amazon.nova-micro-v1:0', modelName: 'Nova Micro' },
];
export const INFERENCE_PROFILES = [
  { inferenceProfileId: 'us.anthropic.claude-3-5-haiku-20241022-v1:0', inferenceProfileName: 'US Claude 3.5 Haiku' },
];

let requests = [];

function send(response, status, type, body, headers = {}) {
  response.writeHead(status, { 'content-type': type, ...headers });
  response.end(body);
}

/** The access key a SigV4 Authorization header was signed with. */
function signedWith(header) {
  return /Credential=([^/]+)\//.exec(header ?? '')?.[1] ?? null;
}

function assumeRole(response, form) {
  if (form.get('Action') !== 'AssumeRole') return send(response, 400, 'text/xml', '<Error><Code>InvalidAction</Code></Error>');
  const expiration = new Date(Date.now() + 3_600_000).toISOString();
  send(response, 200, 'text/xml', `<AssumeRoleResponse><AssumeRoleResult><Credentials>
<AccessKeyId>${ASSUMED_KEY}</AccessKeyId><SecretAccessKey>functional-secret</SecretAccessKey>
<SessionToken>functional-session</SessionToken><Expiration>${expiration}</Expiration>
</Credentials></AssumeRoleResult></AssumeRoleResponse>`);
}

function converse(response, model, body) {
  const request = JSON.parse(body);
  const prompt = request.messages?.at(-1)?.content?.map(block => block.text ?? '').join('') ?? '';
  send(response, 200, 'application/json', JSON.stringify({
    output: { message: { role: 'assistant', content: [{ text: `fake ${model} heard: ${prompt}` }] } },
    stopReason: 'end_turn',
    usage: { inputTokens: 12, outputTokens: 7, totalTokens: 19 },
  }), { 'x-amzn-requestid': `fake-${requests.length}` });
}

/** One AWS event-stream message: a prelude, string headers, the payload and CRCs. */
function eventFrame(eventType, payload) {
  const headers = Buffer.concat(Object.entries({
    ':message-type': 'event',
    ':event-type': eventType,
    ':content-type': 'application/json',
  }).map(([name, value]) => {
    const nameBytes = Buffer.from(name);
    const valueBytes = Buffer.from(value);
    const length = Buffer.alloc(2);
    length.writeUInt16BE(valueBytes.length);
    return Buffer.concat([Buffer.from([nameBytes.length]), nameBytes, Buffer.from([7]), length, valueBytes]);
  }));
  const body = Buffer.from(JSON.stringify(payload));
  const prelude = Buffer.alloc(8);
  prelude.writeUInt32BE(12 + headers.length + body.length + 4, 0);
  prelude.writeUInt32BE(headers.length, 4);
  const preludeCrc = Buffer.alloc(4);
  preludeCrc.writeUInt32BE(crc32(prelude));
  const message = Buffer.concat([prelude, preludeCrc, headers, body]);
  const messageCrc = Buffer.alloc(4);
  messageCrc.writeUInt32BE(crc32(message));
  return Buffer.concat([message, messageCrc]);
}

/** ConverseStream: the same answer as converse, a word per delta. */
function converseStream(response, model, body) {
  const request = JSON.parse(body);
  const prompt = request.messages?.at(-1)?.content?.map(block => block.text ?? '').join('') ?? '';
  const words = `fake ${model} heard: ${prompt}`.split(/(?<= )/);
  response.writeHead(200, { 'content-type': 'application/vnd.amazon.eventstream', 'x-amzn-requestid': `fake-${requests.length}` });
  response.write(eventFrame('messageStart', { role: 'assistant' }));
  for (const text of words) {
    response.write(eventFrame('contentBlockDelta', { contentBlockIndex: 0, delta: { text } }));
  }
  response.write(eventFrame('contentBlockStop', { contentBlockIndex: 0 }));
  response.write(eventFrame('messageStop', { stopReason: 'end_turn' }));
  response.end(eventFrame('metadata', { usage: { inputTokens: 12, outputTokens: 7, totalTokens: 19 }, metrics: { latencyMs: 1 } }));
}

const server = createServer((request, response) => {
  const chunks = [];
  request.on('data', chunk => chunks.push(chunk));
  request.on('end', () => {
    const body = Buffer.concat(chunks).toString('utf8');
    const url = new URL(request.url ?? '/', 'http://fakes');
    const path = decodeURIComponent(url.pathname);
    if (path === '/__requests') return send(response, 200, 'application/json', JSON.stringify(requests));
    if (path === '/__reset' && request.method === 'POST') {
      requests = [];
      return send(response, 204, 'text/plain', '');
    }
    const record = { method: request.method, path, signedWith: signedWith(request.headers.authorization) };
    requests.push(record);
    if (path === '/sts/' && request.method === 'POST') {
      const form = new URLSearchParams(body);
      Object.assign(record, { roleArn: form.get('RoleArn'), externalId: form.get('ExternalId') });
      return assumeRole(response, form);
    }
    if (path === '/control/foundation-models') {
      return send(response, 200, 'application/json', JSON.stringify({ modelSummaries: FOUNDATION_MODELS }));
    }
    if (path === '/control/inference-profiles') {
      return send(response, 200, 'application/json', JSON.stringify({ inferenceProfileSummaries: INFERENCE_PROFILES }));
    }
    const runtime = /^\/runtime\/model\/(.+)\/(converse|converse-stream)$/.exec(path);
    if (runtime && request.method === 'POST') {
      record.model = runtime[1];
      record.stream = runtime[2] === 'converse-stream';
      return (record.stream ? converseStream : converse)(response, runtime[1], body);
    }
    send(response, 404, 'application/json', JSON.stringify({ message: `fake AWS has no ${request.method} ${path}` }));
  });
});

// Every interface, so the port the suite reads requests from can be
// published; the gateway itself calls 127.0.0.1.
server.listen(PORT, '0.0.0.0', () => console.log(`fake AWS on port ${PORT}`));
