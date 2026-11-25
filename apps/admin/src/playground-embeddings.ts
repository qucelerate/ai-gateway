// The playground's embeddings mode: the aliases routed for embeddings, the
// call to the public embeddings API (with the same key rules as chat: an
// ordinary `gw_` key, never the admin session), and the code that makes it.

import { CLIENT_VERSION } from './client-versions';
import type { CatalogDeployment, CatalogPrice, CatalogRoute } from './api';
import { InferenceError, type Usage } from './playground-api';
import type { CodeStyle, Language, Snippet } from './playground-code';

export const EMBEDDINGS_PATH = '/api/inference/v1/embeddings';
/** Most inputs one embeddings request takes. */
export const MAX_EMBEDDING_INPUTS = 128;
const REPO = 'https://github.com/qucelerate/ai-gateway';

/** An alias routed for embeddings, with its preferred deployment's price. */
export type EmbeddingModel = {
  alias: string;
  model: string;
  deploymentName: string;
  location: string | null;
  deployments: number;
  price: CatalogPrice | null;
};

/** Embedding aliases of a release, by alias. An alias routed to several
 * deployments (D-023) shows its preferred one: lowest priority, then
 * highest weight. */
export function embeddingModels(routes: CatalogRoute[], deployments: CatalogDeployment[]): EmbeddingModel[] {
  const byId = new Map(deployments.map(deployment => [deployment.id, deployment]));
  const models = new Map<string, EmbeddingModel & { rank: [number, number]; ids: Set<string> }>();
  for (const route of routes) {
    if (route.operation !== 'embeddings') continue;
    const deployment = byId.get(route.deployment_id);
    const price = deployment?.operations.find(operation => operation.operation === 'embeddings')?.price ?? null;
    const rank: [number, number] = [route.priority ?? 1, -(route.weight ?? 1)];
    const existing = models.get(route.alias);
    const ids = existing?.ids ?? new Set<string>();
    ids.add(route.deployment_id);
    const better = !existing || rank[0] < existing.rank[0] || (rank[0] === existing.rank[0] && rank[1] < existing.rank[1]);
    if (existing && !better) {
      existing.deployments = ids.size;
      continue;
    }
    models.set(route.alias, {
      alias: route.alias,
      model: route.model,
      deploymentName: route.deployment_name,
      location: deployment?.location ?? null,
      deployments: ids.size,
      price,
      rank,
      ids,
    });
  }
  return [...models.values()]
    .map(({ rank: _rank, ids: _ids, ...model }) => model)
    .sort((a, b) => a.alias.localeCompare(b.alias));
}

/** The texts to embed: one per non-empty line. */
export function embeddingInputs(text: string): { inputs: string[]; error?: string } {
  const inputs = text.split(/\r\n|\r|\n/).filter(line => line.trim());
  if (inputs.length === 0) return { inputs, error: 'Write at least one line to embed.' };
  if (inputs.length > MAX_EMBEDDING_INPUTS) return { inputs, error: `A request embeds at most ${MAX_EMBEDDING_INPUTS} lines.` };
  return { inputs };
}

export type EmbeddingResult = {
  vectors: number[][];
  usage: Usage | null;
  model: string | null;
  requestId: string | null;
  totalMs: number;
};

/** Sends one embeddings request with `apiKey`. */
export async function sendEmbeddings(
  apiKey: string,
  model: string,
  input: string[],
  { signal, now = () => performance.now() }: { signal?: AbortSignal; now?: () => number } = {},
): Promise<EmbeddingResult> {
  const started = now();
  let response: Response;
  try {
    response = await fetch(EMBEDDINGS_PATH, {
      method: 'POST',
      // The admin session must never reach the inference API (D-007).
      credentials: 'omit',
      cache: 'no-store',
      headers: {
        Authorization: `Bearer ${apiKey}`,
        'Content-Type': 'application/json',
        Accept: 'application/json',
        'Idempotency-Key': crypto.randomUUID(),
      },
      body: JSON.stringify({ model, input }),
      signal,
    });
  } catch (caught) {
    if (caught instanceof DOMException && caught.name === 'AbortError') throw caught;
    throw new InferenceError(0, 'network_error', 'The gateway could not be reached.', null);
  }
  const requestId = response.headers.get('X-Request-Id');
  if (!response.ok) {
    let code = 'request_failed';
    let detail = `The gateway answered ${response.status}.`;
    let problemId: string | null = null;
    try {
      const problem = await response.json() as { code?: string; detail?: string; title?: string; request_id?: string };
      code = problem.code ?? code;
      detail = problem.detail || problem.title || detail;
      problemId = problem.request_id ?? null;
    } catch {
      // Not a problem document: keep the status.
    }
    throw new InferenceError(response.status, code, detail, requestId ?? problemId);
  }
  const body = await response.json() as { model?: string; data?: { index?: number; embedding?: number[] }[]; usage?: Usage };
  const vectors: number[][] = [];
  for (const item of body.data ?? []) vectors[item.index ?? vectors.length] = item.embedding ?? [];
  return { vectors, usage: body.usage ?? null, model: body.model ?? null, requestId, totalMs: now() - started };
}

/** Cosine similarity, or null for an empty or zero vector or a length mismatch. */
export function cosine(a: number[], b: number[]): number | null {
  if (a.length === 0 || a.length !== b.length) return null;
  let dot = 0;
  let normA = 0;
  let normB = 0;
  for (let index = 0; index < a.length; index += 1) {
    dot += a[index] * b[index];
    normA += a[index] * a[index];
    normB += b[index] * b[index];
  }
  if (normA === 0 || normB === 0) return null;
  return dot / Math.sqrt(normA * normB);
}

/** The languages embeddings code is shown in. */
export const embeddingLanguages: readonly (readonly [Language, string])[] = [
  ['curl', 'curl'],
  ['python', 'Python'],
  ['javascript', 'JavaScript'],
];

const pyString = (text: string) => JSON.stringify(text).replace(/\u2028/g, '\\u2028').replace(/\u2029/g, '\\u2029');
const shellQuoted = (text: string) => `'${text.replace(/'/g, `'\\''`)}'`;

/** Code that embeds `inputs` with `model`. Only Python has a client here;
 * the others are raw HTTP. */
export function embeddingSnippet(language: Language, style: CodeStyle, model: string, inputs: string[], baseUrl: string): Snippet {
  const base = baseUrl.replace(/\/+$/, '');
  const url = `${base}${EMBEDDINGS_PATH}`;
  const shown = inputs.length ? inputs : ['The quick brown fox', 'A fast auburn fox'];
  const body = JSON.stringify({ model, input: shown }, null, 2).replace(/\u2028/g, '\\u2028').replace(/\u2029/g, '\\u2029');
  const pyList = (indent: number) =>
    `[\n${shown.map(text => `${' '.repeat(indent + 4)}${pyString(text)},`).join('\n')}\n${' '.repeat(indent)}]`;
  if (language === 'python' && style === 'client') {
    return {
      language, style, syntax: 'python',
      setup: `pip install qucelerate-ai-gateway==${CLIENT_VERSION}  # Python 3.10+`,
      code: [
        'import os',
        '',
        'from qucelerate_ai_gateway import Client',
        '',
        `inputs = ${pyList(0)}`,
        '',
        `with Client(${pyString(base)}, os.environ["GATEWAY_API_KEY"]) as client:`,
        `    response = client.embeddings(${pyString(model)}, inputs)`,
        '    for item in response.data:',
        '        print(item.index, len(item.embedding))',
        '    print("usage:", response.usage)',
      ].join('\n'),
    };
  }
  const note = style === 'client' && language !== 'python'
    ? language === 'javascript'
      ? 'There is no JavaScript client yet, so this is raw fetch for Node.js 18 or later.'
      : 'curl is raw HTTP; choose Python for a client.'
    : undefined;
  if (language === 'python') {
    return {
      language, style: 'raw', syntax: 'python',
      setup: 'Standard library only (Python 3.8+).',
      code: [
        'import json',
        'import os',
        'import urllib.request',
        'import uuid',
        '',
        'body = {',
        `    "model": ${pyString(model)},`,
        `    "input": ${pyList(4)},`,
        '}',
        'request = urllib.request.Request(',
        `    ${pyString(url)},`,
        '    data=json.dumps(body).encode(),',
        '    headers={',
        '        "Authorization": "Bearer " + os.environ["GATEWAY_API_KEY"],',
        '        "Content-Type": "application/json",',
        '        "Idempotency-Key": str(uuid.uuid4()),',
        '    },',
        ')',
        'with urllib.request.urlopen(request) as response:',
        '    answer = json.load(response)',
        'for item in answer["data"]:',
        '    print(item["index"], len(item["embedding"]))',
        'print("usage:", answer.get("usage"))',
      ].join('\n'),
      ...(note ? { note } : {}),
    };
  }
  if (language === 'javascript') {
    return {
      language, style: 'raw', syntax: 'javascript',
      setup: 'Node.js 18 or later; no packages.',
      code: [
        `const response = await fetch(${JSON.stringify(url)}, {`,
        "  method: 'POST',",
        '  headers: {',
        '    Authorization: `Bearer ${process.env.GATEWAY_API_KEY}`,',
        "    'Content-Type': 'application/json',",
        "    'Idempotency-Key': crypto.randomUUID(),",
        '  },',
        `  body: JSON.stringify(${body.replace(/\n/g, '\n  ')}),`,
        '});',
        'if (!response.ok) throw new Error(`${response.status}: ${await response.text()}`);',
        'const answer = await response.json();',
        'for (const item of answer.data) console.log(item.index, item.embedding.length);',
        "console.log('usage:', answer.usage);",
      ].join('\n'),
      ...(note ? { note } : {}),
    };
  }
  return {
    language: 'curl', style: 'raw', syntax: 'sh',
    setup: 'Needs curl and uuidgen. Set GATEWAY_API_KEY to an API key with the Embeddings scope.',
    code: [
      `curl ${shellQuoted(url)} \\`,
      '  -H "Authorization: Bearer $GATEWAY_API_KEY" \\',
      '  -H "Content-Type: application/json" \\',
      '  -H "Idempotency-Key: $(uuidgen)" \\',
      `  -d ${shellQuoted(body)}`,
    ].join('\n'),
    ...(note ? { note } : {}),
  };
}
