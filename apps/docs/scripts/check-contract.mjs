// The documentation release gate's contract checks (plan §9): the published
// OpenAPI is a valid document whose schemas compile; every copyable example
// matches it (a curl request's path, required headers and body; a problem
// document or a chat response; an SSE transcript's events and their data); and every versioned
// reference names the published version. Run from apps/docs:
//
//   node scripts/check-contract.mjs
//
// Prints each failure with its file and line and exits non-zero if any.

import { readFileSync, existsSync } from 'node:fs';
import { join, relative } from 'node:path';
import { execFileSync } from 'node:child_process';
import SwaggerParser from '@apidevtools/swagger-parser';
import Ajv2020 from 'ajv/dist/2020.js';
import addFormats from 'ajv-formats';

const ROOT = new URL('..', import.meta.url).pathname;
const SPEC_PATH = join(ROOT, 'public/openapi/inference-v1.json');
const failures = [];
const fail = (where, message) => failures.push(`${where}: ${message}`);

// The document itself, then its schemas, compiled as JSON Schema 2020-12
// (OpenAPI 3.1's dialect).
const raw = JSON.parse(readFileSync(SPEC_PATH, 'utf8'));
try {
  await SwaggerParser.validate(structuredClone(raw));
} catch (error) {
  fail('public/openapi/inference-v1.json', `not a valid OpenAPI document: ${error.message}`);
}
const ajv = new Ajv2020({ strict: false, allErrors: true });
addFormats(ajv);
ajv.addSchema({ $id: 'spec', components: raw.components });
const schemaFor = ref => {
  const name = ref.replace('#/components/schemas/', '');
  return ajv.getSchema(`spec#/components/schemas/${name}`);
};
for (const name of Object.keys(raw.components.schemas)) {
  try {
    if (!schemaFor(name)) fail('public/openapi/inference-v1.json', `schema ${name} does not compile`);
  } catch (error) {
    fail('public/openapi/inference-v1.json', `schema ${name} does not compile: ${error.message}`);
  }
}
const resolve = node => (node?.$ref ? resolve(node.$ref.split('/').slice(1).reduce((at, key) => at[key], raw)) : node);
const errorsOf = validate => ajv.errorsText(validate.errors, { dataVar: 'example' });

// The version the published paths carry.
const versions = new Set(Object.keys(raw.paths).map(path => /^\/api\/inference\/(v\d+)\//.exec(path)?.[1]).filter(Boolean));
if (versions.size !== 1) fail('public/openapi/inference-v1.json', `paths carry versions ${[...versions].join(', ') || 'none'}, expected one`);
const [VERSION] = versions;

const pages = execFileSync('git', ['ls-files', '*.md'], { cwd: ROOT, encoding: 'utf8' })
  .split('\n')
  .filter(file => file && !file.startsWith('node_modules/') && file !== 'README.md');

for (const page of pages) {
  const text = readFileSync(join(ROOT, page), 'utf8');
  const lines = text.split('\n');

  // Versioned references: the inference prefix and the OpenAPI artifact.
  lines.forEach((line, index) => {
    for (const match of line.matchAll(/\/api\/inference\/(v\d+)\b/g)) {
      if (match[1] !== VERSION) fail(`${page}:${index + 1}`, `names /api/inference/${match[1]}, but the published contract is ${VERSION}`);
    }
    for (const match of line.matchAll(/\/openapi\/([\w.-]+\.json)/g)) {
      if (!existsSync(join(ROOT, 'public/openapi', match[1]))) fail(`${page}:${index + 1}`, `links /openapi/${match[1]}, which is not published`);
    }
  });

  // Fenced examples, with the line each starts on.
  for (const block of text.matchAll(/^```(\w*)\n([\s\S]*?)^```/gm)) {
    const at = `${page}:${text.slice(0, block.index).split('\n').length}`;
    const [, lang, body] = block;
    if (lang === 'sh' && /\bcurl\b/.test(body) && body.includes('/api/')) checkCurl(at, body);
    else if (lang === 'json') checkJson(at, body);
    else if (lang === 'text' && /^event: /m.test(body)) checkSse(at, body);
  }
}

function checkCurl(at, body) {
  const commands = shellCommands(body).filter(command => /^\s*curl\b/.test(command));
  // A fence that calls the gateway but yields no command was misparsed.
  if (commands.length === 0) return fail(at, 'shell example mentions curl but no curl command could be read');
  for (const command of commands) checkCurlCommand(at, command);
}

/** A shell snippet's commands: split at newlines outside quotes, with
 * backslash continuations joined, so a quoted multi-line body stays whole. */
function shellCommands(body) {
  const commands = [];
  let current = '';
  let quote = null;
  for (let index = 0; index < body.length; index++) {
    const char = body[index];
    if (!quote && char === '#' && (current === '' || /\s$/.test(current))) {
      // A comment runs to the end of the line; its quotes are not quotes.
      while (index + 1 < body.length && body[index + 1] !== '\n') index++;
    } else if (!quote && char === '\\' && body[index + 1] === '\n') {
      current += ' ';
      index++;
    } else if (!quote && char === '\n') {
      commands.push(current);
      current = '';
    } else {
      if (quote ? char === quote : char === "'" || char === '"') quote = quote ? null : char;
      current += char;
    }
  }
  commands.push(current);
  return commands.filter(command => command.trim());
}

function checkCurlCommand(at, command) {
  const path = /\$GATEWAY_BASE_URL(\/[^"'\s]*)/.exec(command)?.[1];
  // The method as curl decides it: -X/--request, else -I/-G, else POST for -d.
  const explicit = /(?:^|\s)(?:-X\s*|--request[\s=])["']?([A-Za-z]+)/.exec(command)?.[1];
  const method = (explicit ?? (/(?:^|\s)(?:-I|--head)\b/.test(command) ? 'HEAD' : /(?:^|\s)(?:-G|--get)\b/.test(command) ? 'GET' : 'POST')).toLowerCase();
  const operation = path && raw.paths[path]?.[method];
  if (!operation) return fail(at, `curl sends ${method.toUpperCase()} ${path ?? 'to no gateway path'}, which the contract does not define`);
  const headers = new Set([...command.matchAll(/-H\s+"([^:"]+):/g)].map(match => match[1].toLowerCase()));
  for (const parameter of (operation.parameters ?? []).map(resolve)) {
    if (parameter.in === 'header' && parameter.required && !headers.has(parameter.name.toLowerCase())) {
      fail(at, `curl ${path} omits the required ${parameter.name} header`);
    }
  }
  if (!headers.has('authorization')) fail(at, `curl ${path} sends no Authorization header`);
  const schema = operation.requestBody?.content?.['application/json']?.schema;
  const data = /(?:-d|--data(?:-raw|-binary)?)\s+'([^']*)'/.exec(command)?.[1];
  if (!schema) return;
  if (data === undefined) return fail(at, `curl ${path} sends no -d body`);
  let json;
  try {
    json = JSON.parse(data);
  } catch (error) {
    return fail(at, `curl ${path} body is not JSON: ${error.message}`);
  }
  const validate = schemaFor(schema.$ref);
  if (!validate(json)) fail(at, `curl body does not match ${path}: ${errorsOf(validate)}`);
}

function checkJson(at, body) {
  let json;
  try {
    json = JSON.parse(body);
  } catch (error) {
    return fail(at, `JSON example does not parse: ${error.message}`);
  }
  // A chat response is shown on its own too: an object with `choices`.
  if (json && typeof json === 'object' && !Array.isArray(json) && 'choices' in json) {
    const chat = schemaFor('ChatResponse');
    if (!chat(json)) fail(at, `chat response example does not match ChatResponse: ${errorsOf(chat)}`);
    return;
  }
  // A problem document is the other response shape shown on its own: any
  // object using its fields is one, and is checked whole, so dropping a
  // required field fails rather than escaping the check.
  const problemFields = Object.keys(raw.components.schemas.Problem.properties ?? {});
  if (!json || typeof json !== 'object' || Array.isArray(json) || !Object.keys(json).some(key => problemFields.includes(key))) {
    return fail(at, 'JSON example matches no schema the checks know (only problem documents are expected)');
  }
  const validate = schemaFor('Problem');
  if (!validate(json)) fail(at, `problem example does not match Problem: ${errorsOf(validate)}`);
}

function checkSse(at, body) {
  const events = new Map(Object.values(raw.paths).flatMap(item => item.post?.['x-sse-events'] ?? []).map(event => [event.name, event]));
  for (const frame of body.trim().split(/\n\s*\n/)) {
    const name = /^event: (.+)$/m.exec(frame)?.[1];
    const data = /^data: (.+)$/m.exec(frame)?.[1];
    const event = events.get(name);
    if (!event) {
      fail(at, `SSE example sends event ${name}, which the contract does not define`);
      continue;
    }
    let json;
    try {
      json = JSON.parse(data ?? '');
    } catch (error) {
      fail(at, `SSE ${name} data is not JSON: ${error.message}`);
      continue;
    }
    const validate = schemaFor(event.schema.$ref);
    if (!validate(json)) fail(at, `SSE ${name} data does not match ${event.schema.$ref.split('/').pop()}: ${errorsOf(validate)}`);
  }
}

if (failures.length) {
  console.error(`${failures.length} documentation contract check(s) failed:`);
  for (const failure of failures) console.error(`  ${failure}`);
  process.exit(1);
}
console.log(`documentation contract checks passed: ${pages.length} pages against ${relative(ROOT, SPEC_PATH)} (${VERSION})`);
