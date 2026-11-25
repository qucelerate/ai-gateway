// Generates support/errors.md, the error-code reference, from the sources of
// truth so it cannot drift:
//
// - inference API: clients/conformance/errors.json (code, status, whether the
//   request was reserved, retry class, meaning). The gateway's own tests keep
//   that file equal to the codes it serves; this script also checks it against
//   the `ProblemCode::code` match in src/transport/inference.rs.
// - admin API: the problem codes the admin transport serves, read from the
//   Rust source (src/transport/*.rs other than the inference ones, and the
//   `problem_code` functions in src/domain/*.rs), with their HTTP statuses.
//
// Likely causes and what to do live in support/error-notes.json, keyed by code.
// The script fails when a served code has no notes, when notes name a code
// that is not served, or when the statuses the notes give for an admin code
// differ from the ones the source returns it with.
//
//   node scripts/generate-errors.mjs          write support/errors.md
//   node scripts/generate-errors.mjs --check  exit 1 if support/errors.md is stale
//
// Run from apps/docs; paths resolve relative to this file.
import { readFileSync, writeFileSync, readdirSync, existsSync } from 'node:fs';
import { join, dirname, basename } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const DOCS = join(HERE, '..');
const REPO = join(DOCS, '..', '..');
const CATALOG_PATH = join(REPO, 'clients/conformance/errors.json');
const NOTES_PATH = join(DOCS, 'support/error-notes.json');
const OUTPUT_PATH = join(DOCS, 'support/errors.md');
const TRANSPORT = join(REPO, 'src/transport');
const DOMAIN = join(REPO, 'src/domain');
// Inference-side transport: its codes come from ProblemCode / errors.json.
const INFERENCE_FILES = new Set(['inference.rs', 'batches.rs']);

const STATUS = {
  BAD_REQUEST: 400,
  UNAUTHORIZED: 401,
  FORBIDDEN: 403,
  NOT_FOUND: 404,
  METHOD_NOT_ALLOWED: 405,
  CONFLICT: 409,
  GONE: 410,
  PRECONDITION_FAILED: 412,
  PAYLOAD_TOO_LARGE: 413,
  UNSUPPORTED_MEDIA_TYPE: 415,
  UNPROCESSABLE_ENTITY: 422,
  PRECONDITION_REQUIRED: 428,
  TOO_MANY_REQUESTS: 429,
  INTERNAL_SERVER_ERROR: 500,
  BAD_GATEWAY: 502,
  SERVICE_UNAVAILABLE: 503,
  GATEWAY_TIMEOUT: 504,
};

const errors = [];
const problem = message => errors.push(message);

/** Index just past the `{ … }` block that opens at `open`, skipping string,
 * raw-string and char literals and comments. */
function blockEnd(text, open) {
  let depth = 0;
  for (let index = open; index < text.length; index++) {
    const rest = text.slice(index, index + 3);
    if (rest.startsWith('//')) index = text.indexOf('\n', index);
    else if (rest.startsWith('/*')) index = text.indexOf('*/', index) + 1;
    else if (/^r#*"/.test(rest)) {
      const hashes = /^r(#*)"/.exec(text.slice(index))[1];
      index = text.indexOf(`"${hashes}`, index + hashes.length + 2) + hashes.length;
    } else if (text[index] === '"') {
      for (index++; text[index] !== '"'; index++) if (text[index] === '\\') index++;
    } else if (/^'(\\.|[^\\'])'/.test(text.slice(index, index + 4))) index = text.indexOf("'", index + 2);
    else if (text[index] === '{') depth++;
    else if (text[index] === '}' && --depth === 0) return index + 1;
    if (index < 0) break;
  }
  throw new Error('unbalanced braces');
}

/** Rust source without `#[cfg(test)]` items and whole-line comments. */
function source(path) {
  let text = readFileSync(path, 'utf8');
  for (let at = text.indexOf('#[cfg(test)]'); at >= 0; at = text.indexOf('#[cfg(test)]', at)) {
    const open = text.indexOf('{', at);
    const semicolon = text.indexOf(';', at);
    const end = semicolon >= 0 && semicolon < open ? semicolon + 1 : blockEnd(text, open);
    text = text.slice(0, at) + text.slice(end);
  }
  return text
    .split('\n')
    .filter(line => !/^\s*\/\//.test(line))
    .join('\n');
}

function statusOf(name, where) {
  const status = STATUS[name];
  if (status === undefined) problem(`${where}: StatusCode::${name} is not in the script's status table`);
  return status;
}

// ---------------------------------------------------------------------------
// Inference codes: the conformance catalog, checked against ProblemCode.
// ---------------------------------------------------------------------------

const catalog = JSON.parse(readFileSync(CATALOG_PATH, 'utf8'));
const inferenceCodes = catalog.codes;
{
  const text = source(join(TRANSPORT, 'inference.rs'));
  const start = text.search(/impl ProblemCode \{\s*pub\(crate\) const fn code\(self\)/);
  if (start < 0) {
    problem('src/transport/inference.rs: cannot find `impl ProblemCode { fn code }`');
  } else {
    const body = text.slice(start, text.indexOf('\n    }\n', start));
    const served = new Set([...body.matchAll(/Self::\w+\s*=>\s*"([a-z][a-z0-9_]*)"/g)].map(match => match[1]));
    const listed = new Set(inferenceCodes.map(entry => entry.code));
    for (const code of served) if (!listed.has(code)) problem(`inference code ${code} is served but missing from clients/conformance/errors.json`);
    for (const code of listed) if (!served.has(code)) problem(`clients/conformance/errors.json lists ${code}, which ProblemCode does not serve`);
  }
}

// ---------------------------------------------------------------------------
// Admin codes: extracted from the Rust source.
// ---------------------------------------------------------------------------

/** code -> Set of HTTP statuses (may stay empty when the source does not say). */
const adminCodes = new Map();
const note = (code, status) => {
  if (!adminCodes.has(code)) adminCodes.set(code, new Set());
  if (status !== undefined) adminCodes.get(code).add(status);
};

for (const file of readdirSync(TRANSPORT).filter(name => name.endsWith('.rs')).sort()) {
  if (INFERENCE_FILES.has(file)) continue;
  const where = `src/transport/${file}`;
  const text = source(join(TRANSPORT, file));
  // problem(StatusCode::X, "code", …), error(StatusCode::X, "code"), and
  // fail(failure, StatusCode::X, "code").
  for (const match of text.matchAll(/StatusCode::([A-Z_]+)\s*,\s*"([a-z][a-z0-9_]*)"/g)) note(match[2], statusOf(match[1], where));
  // match status { StatusCode::X => "code", … }
  for (const match of text.matchAll(/StatusCode::([A-Z_]+)\s*=>\s*"([a-z][a-z0-9_]*)"/g)) note(match[2], statusOf(match[1], where));
  // A hand-built problem body: json!({ "code": "code", … }), its status set
  // nearby with `let status = StatusCode::X`.
  for (const match of text.matchAll(/"code"\s*:\s*"([a-z][a-z0-9_]*)"/g)) {
    const before = text.slice(Math.max(0, match.index - 400), match.index);
    const status = [...before.matchAll(/let status = StatusCode::([A-Z_]+)/g)].pop()?.[1];
    note(match[1], status && statusOf(status, where));
  }
  // Helpers whose first parameter is the code: `fn name(code: &'static str…`
  // or `let name = |code…|`, with the status they answer.
  const helpers = [
    ...text.matchAll(/fn (\w+)\(\s*code: &'static str/g),
    ...text.matchAll(/let (\w+)\s*=\s*\|\s*code\b/g),
  ];
  for (const helper of helpers) {
    const status = /StatusCode::([A-Z_]+)/.exec(text.slice(helper.index, helper.index + 400))?.[1];
    for (const call of text.matchAll(new RegExp(`\\b${helper[1]}\\(\\s*"([a-z][a-z0-9_]*)"`, 'g'))) {
      note(call[1], status && statusOf(status, where));
    }
  }
}
// Domain errors that name their own problem code (pagination, versions).
for (const file of readdirSync(DOMAIN).filter(name => name.endsWith('.rs')).sort()) {
  const text = source(join(DOMAIN, file));
  for (const fn of text.matchAll(/fn problem_code\(self\)[^{]*\{/g)) {
    const body = text.slice(fn.index, text.indexOf('\n    }\n', fn.index));
    for (const match of body.matchAll(/=>\s*"([a-z][a-z0-9_]*)"/g)) note(match[1]);
  }
}

// ---------------------------------------------------------------------------
// Notes, checked against both groups.
// ---------------------------------------------------------------------------

const notes = JSON.parse(readFileSync(NOTES_PATH, 'utf8'));
const RETRY = {
  same_key: 'After `Retry-After`: retry with the same `Idempotency-Key`.',
  new_key: 'Yes, as a new request with a new `Idempotency-Key`, if running it twice is safe.',
  later: 'Yes, after a short wait.',
  fix: 'No: change the request, credential or configuration first.',
  none: 'No: do not resend automatically.',
};
const RESERVED = {
  true: 'Yes: the `Idempotency-Key` is spent.',
  false: 'No: nothing was reserved.',
  maybe: 'Depends on where it failed; the `detail` says.',
};

function checkText(where, value) {
  if (typeof value !== 'string' || !value.trim()) return problem(`${where} is missing or empty`);
  // Outside code spans, `<` would start HTML and `{{` a Vue interpolation.
  const prose = value.replace(/`[^`]*`/g, '');
  if (prose.includes('<') || prose.includes('{{')) problem(`${where} has < or {{ outside a code span`);
}

function checkGroup(group, served, fields) {
  const entries = notes[group];
  if (!entries || typeof entries !== 'object') return problem(`error-notes.json has no "${group}" object`);
  for (const code of served) if (!(code in entries)) problem(`${group} code ${code} has no entry in support/error-notes.json`);
  for (const code of Object.keys(entries)) {
    if (!served.has(code)) problem(`support/error-notes.json ${group}.${code} names a code the gateway does not serve`);
    for (const field of fields) checkText(`error-notes.json ${group}.${code}.${field}`, entries[code][field]);
    const extra = Object.keys(entries[code]).filter(key => !fields.includes(key) && key !== 'status' && key !== 'retry');
    if (extra.length) problem(`error-notes.json ${group}.${code} has unknown field(s) ${extra.join(', ')}`);
  }
}

checkGroup('inference', new Set(inferenceCodes.map(entry => entry.code)), ['causes', 'action']);
checkGroup('admin', new Set(adminCodes.keys()), ['meaning', 'causes', 'action']);
for (const [code, sourceStatuses] of adminCodes) {
  const entry = notes.admin?.[code];
  if (!entry) continue;
  const given = Array.isArray(entry.status) ? entry.status : [];
  if (!given.length || !given.every(Number.isInteger)) problem(`error-notes.json admin.${code}.status must be a non-empty array of HTTP statuses`);
  const sorted = list => [...list].sort((a, b) => a - b).join(', ');
  if (sourceStatuses.size && sorted(given) !== sorted(sourceStatuses)) {
    problem(`error-notes.json admin.${code}.status is [${sorted(given)}], but the source returns it with [${sorted(sourceStatuses)}]`);
  }
  if (!(entry.retry in RETRY) || entry.retry === 'new_key') problem(`error-notes.json admin.${code}.retry must be one of fix, none, later, same_key`);
}
for (const entry of inferenceCodes) {
  if (!(entry.retry in RETRY)) problem(`errors.json ${entry.code}.retry ${entry.retry} is not a known retry class`);
  if (!(String(entry.reserved) in RESERVED)) problem(`errors.json ${entry.code}.reserved ${entry.reserved} is not true, false or maybe`);
  checkText(`errors.json ${entry.code}.description`, entry.description);
}

if (errors.length) {
  console.error(`generate-errors: ${errors.length} problem(s):`);
  for (const message of errors) console.error(`  ${message}`);
  process.exit(1);
}

// ---------------------------------------------------------------------------
// Rendering.
// ---------------------------------------------------------------------------

const cell = text => text.replace(/\|/g, '\\|').replace(/\n+/g, ' ');
const byStatusThenCode = (a, b) => a.status - b.status || a.code.localeCompare(b.code);
const inferenceSet = new Set(inferenceCodes.map(entry => entry.code));
const anchorOf = (group, code) => (group === 'admin' && inferenceSet.has(code) ? `admin-${code}` : code);

function section(group, { code, statuses, meaning, causes, action, retry, reserved }) {
  const rows = [
    ['HTTP status', statuses.join(', ')],
    ['Code', `\`${code}\``],
    ['Meaning', meaning],
    ['Likely causes', causes],
    ['What to do', action],
    ['Retryable', RETRY[retry]],
  ];
  if (reserved !== undefined) rows.push(['Reserved', RESERVED[String(reserved)]]);
  return [
    `### ${code} {#${anchorOf(group, code)}}`,
    '',
    '| Field | Value |',
    '| --- | --- |',
    ...rows.map(([name, value]) => `| ${name} | ${cell(value)} |`),
    '',
  ].join('\n');
}

function summary(group, entries) {
  return [
    '| Status | Code | Retryable |',
    '| --- | --- | --- |',
    ...entries.map(entry => `| ${entry.statuses.join(', ')} | [\`${entry.code}\`](#${anchorOf(group, entry.code)}) | ${RETRY_SHORT[entry.retry]} |`),
    '',
  ].join('\n');
}
const RETRY_SHORT = { same_key: 'after Retry-After', new_key: 'new key', later: 'yes', fix: 'no', none: 'no' };

const inference = inferenceCodes
  .map(entry => ({
    code: entry.code,
    status: entry.status,
    statuses: [entry.status],
    meaning: entry.description,
    causes: notes.inference[entry.code].causes,
    action: notes.inference[entry.code].action,
    retry: entry.retry,
    reserved: entry.reserved,
  }))
  .sort(byStatusThenCode);
const admin = [...adminCodes.keys()]
  .map(code => {
    const entry = notes.admin[code];
    const statuses = [...entry.status].sort((a, b) => a - b);
    return { code, status: statuses[0], statuses, meaning: entry.meaning, causes: entry.causes, action: entry.action, retry: entry.retry };
  })
  .sort(byStatusThenCode);

const page = `---
description: "Every problem code the inference and admin APIs return, with its HTTP status, meaning, likely causes, what to do, and whether to retry."
---

<!-- Generated by apps/docs/scripts/generate-errors.mjs from clients/conformance/errors.json, the gateway's Rust source and support/error-notes.json. Do not edit; change the sources and run \`node scripts/generate-errors.mjs\`. -->

# Error codes

Every error response the gateway sends is a problem document in the RFC 9457 shape, with \`Content-Type: application/problem+json\`:

\`\`\`json
{
  "type": "https://gateway.invalid/problems/model_unavailable",
  "code": "model_unavailable",
  "title": "Not Found",
  "status": 404,
  "detail": "model alias is not available for this operation",
  "instance": "/api/inference/v1/chat/completions",
  "request_id": "2c0f8a51-5b7e-4f53-9d1c-0f6a7e2b9c44"
}
\`\`\`

- Branch on \`status\` first, then on \`code\`. \`code\` is an open set: handle a code you do not know by its HTTP status.
- \`detail\` is for people. It can name the field or limit at fault, but its wording can change; never parse it.
- \`request_id\` is also sent as the \`X-Request-Id\` response header. Quote it when you ask for help; operators find the request in the gateway's logs by it.
- Some problems carry extra fields: \`scope\` and \`limit\` on rate limits, \`end_user\` and balances on an end user's \`quota_exceeded\`, \`errors\` on \`batch_validation_failed\`. The [OpenAPI document](/openapi/inference-v1.json) describes them.
- Where a response carries \`Retry-After\`, wait at least that many seconds before you retry.
- A stream that fails after it has started cannot change its status any more: it ends with an \`error\` event whose data is \`{"code": "…", "status": …}\`, with the same codes (\`provider_rate_limited\`, \`provider_unavailable\`, \`provider_timeout\`, \`provider_invalid_request\`, \`provider_failure\`).

Each code below has its own section; link to one as \`errors.md#<code>\`. A code both APIs return has its admin section at \`#admin-<code>\`. For handling errors in a client, see [errors in the quickstarts](../quickstarts/errors.md); for symptoms rather than codes, see [troubleshooting](./troubleshooting.md).

**Retryable** uses these classes: *after Retry-After* (nothing was reserved; retry with the same \`Idempotency-Key\`), *new key* (the key is spent; resend as a new request only if running it twice is safe), *yes* (transient; retry after a short wait), and *no* (change something first, or do not resend automatically).

## Inference API

Codes from \`/api/inference/v1\`, as listed in the client conformance catalog (\`clients/conformance/errors.json\`). **Reserved** says whether the request had been reserved, which spends its \`Idempotency-Key\`. ${inference.length} codes.

${summary('inference', inference)}
${inference.map(entry => section('inference', entry)).join('\n')}
## Admin API

Codes from \`/api/admin/v1\` (the admin console uses the same API). They carry the same fields, without the inference-only extensions. A few sign-in and session endpoints (the OIDC start and callback, \`/me\`, \`/context\`, \`/logout\`) and the MCP endpoint answer with \`{"error": "<code>"}\` instead of a problem document; their codes are listed here too. ${admin.length} codes.

${summary('admin', admin)}
${admin.map(entry => section('admin', entry)).join('\n')}`;

if (process.argv.includes('--check')) {
  const current = existsSync(OUTPUT_PATH) ? readFileSync(OUTPUT_PATH, 'utf8') : '';
  if (current !== page) {
    console.error(`${basename(OUTPUT_PATH)} is stale: run \`node scripts/generate-errors.mjs\` in apps/docs`);
    process.exit(1);
  }
  console.log(`support/errors.md is current: ${inference.length} inference and ${admin.length} admin codes`);
} else {
  writeFileSync(OUTPUT_PATH, page);
  console.log(`wrote support/errors.md: ${inference.length} inference and ${admin.length} admin codes`);
}
