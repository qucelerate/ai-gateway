import { execFile } from 'node:child_process';
import { mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { promisify } from 'node:util';
import { expect, test } from '@playwright/test';
import { MODEL, ALIAS, provisionAwsTenant } from '../support/aws-tenant.js';
import { fakeRequests, resetFakes } from '../support/fakes.js';
import { seedOwner, signIn } from '../support/seed.js';

// Every client library against this gateway: each client's example (one
// chat call and one streamed call) runs with a tenant's API key, through the
// gateway to the faked Bedrock, and must print the model's answer for both.
// The result, with the client and gateway versions, is the compatibility
// record a client release carries.
//
// Runs only when CLIENT_EXAMPLES names a JSON file of the clients to run:
//   {"rust": {"version": "0.1.0", "command": ["/path/to/chat"]}, ...}
// Each command is called with GATEWAY_URL and GATEWAY_API_KEY set and the
// arguments <model> <prompt>. CLIENT_RESULTS names where the results go;
// GATEWAY_UNDER_TEST describes the gateway (an image reference).

type Example = { version: string; command: string[] };
type Result = { version: string; passed: boolean; detail?: string };

const examplesFile = process.env.CLIENT_EXAMPLES;
const examples: Record<string, Example> = examplesFile ? JSON.parse(readFileSync(examplesFile, 'utf8')) : {};
const run = promisify(execFile);
// Each client's outcome is its own file, written as its test ends: a failed
// test replaces Playwright's worker, and a new worker's memory would forget
// the outcomes before it. afterAll merges whatever files exist.
const resultsDir = process.env.CLIENT_RESULTS ? `${process.env.CLIENT_RESULTS}.d` : '';

function record(name: string, result: Result): void {
  if (!resultsDir) return;
  mkdirSync(resultsDir, { recursive: true });
  writeFileSync(join(resultsDir, `${name}.json`), JSON.stringify(result));
}

test.describe('clients against this gateway', () => {
  test.skip(!examplesFile, 'CLIENT_EXAMPLES is not set');

  let secret = '';
  test.beforeAll(async ({ browser }) => {
    test.setTimeout(120_000);
    // The run's first worker starts the record afresh; a replacement worker
    // (after a failure) keeps the outcomes before it.
    if (resultsDir && test.info().workerIndex === 0) rmSync(resultsDir, { recursive: true, force: true });
    const baseURL = process.env.GATEWAY_URL ?? 'http://localhost:18180';
    const context = await browser.newContext({ baseURL });
    const owner = await seedOwner('clients');
    await signIn(context, owner, baseURL);
    secret = await provisionAwsTenant(await context.newPage(), owner, 'functional-clients');
    await context.close();
  });

  test.afterAll(() => {
    if (!process.env.CLIENT_RESULTS) return;
    const results: Record<string, Result> = {};
    for (const name of Object.keys(examples)) {
      try {
        results[name] = JSON.parse(readFileSync(join(resultsDir, `${name}.json`), 'utf8'));
      } catch {
        // Not run (yet): absent from the record, never reported as passing.
      }
    }
    writeFileSync(process.env.CLIENT_RESULTS, JSON.stringify({
      gateway: process.env.GATEWAY_UNDER_TEST ?? 'unknown',
      contract: '1.0.0',
      tested_at: new Date().toISOString(),
      clients: results,
    }, null, 2) + '\n');
  });

  for (const [name, example] of Object.entries(examples)) {
    test(`the ${name} client ${example.version} chats and streams`, async () => {
      test.setTimeout(180_000);
      const result: Result = { version: example.version, passed: false };
      try {
        await check(result);
      } finally {
        record(name, result);
      }
    });

    async function check(result: Result): Promise<void> {
      await resetFakes();
      const prompt = `ping from ${name}`;
      const answer = `fake ${MODEL} heard: ${prompt}`;
      const redact = (text: string) => text.split(secret).join('<api key>');
      const [program, ...args] = example.command;
      let stdout = '';
      try {
        ({ stdout } = await run(program, [...args, ALIAS, prompt], {
          env: { ...process.env, GATEWAY_URL: process.env.GATEWAY_URL ?? 'http://localhost:18180', GATEWAY_API_KEY: secret },
          timeout: 150_000,
        }));
      } catch (error) {
        const failed = error as { stdout?: string; stderr?: string; message: string };
        result.detail = redact(`${failed.message}\n${failed.stdout ?? ''}\n${failed.stderr ?? ''}`).slice(0, 4000);
        throw new Error(result.detail);
      }
      const lines = redact(stdout).split('\n');
      result.detail = lines.join('\n').slice(0, 4000);
      expect(lines.some(line => line.startsWith(`chat: ${answer}`)), `chat answer in:\n${stdout}`).toBe(true);
      expect(lines.some(line => line.startsWith(`stream: ${answer}`)), `streamed answer in:\n${stdout}`).toBe(true);
      // One Converse and one ConverseStream reached Bedrock, for the model behind the alias.
      const calls = (await fakeRequests()).filter(item => item.model);
      expect(calls.map(call => ({ model: call.model, stream: call.stream }))).toEqual([
        { model: MODEL, stream: false },
        { model: MODEL, stream: true },
      ]);
      result.passed = true;
    }
  }
});
