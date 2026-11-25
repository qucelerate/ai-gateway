/// <reference types="node" />
// Compiles the playground's snippets with real toolchains, for a prompt that
// holds everything a string literal must escape. Slow and dependent on what
// is installed, so it runs only when asked:
//
//   PLAYGROUND_CHECK_SNIPPETS=1 npx vitest run src/playground-snippets.check.test.ts
//
// Each check is skipped when its tool is missing. Rust needs the crates in
// Cargo's cache (it builds offline); Go and the Java client (which needs
// Jackson) are checked only where `go` and `mvn` exist.

import { execFileSync, spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { describe, expect, it } from 'vitest';
import { requestBody, snippet, type CodeStyle, type Language, type PlaygroundRequest } from './playground-code';

const enabled = Boolean(process.env.PLAYGROUND_CHECK_SNIPPETS);
const repo = resolve(__dirname, '../../..');
const hostile = 'Quote " and \' and ` and \\ and \\u0041 and $HOME ${x} %s ??= ???- \nnext\r\ttab \u0001\u0007\u007f   \u{1F600} é';
const request: PlaygroundRequest = {
  model: 'chat-"default"',
  messages: [{ role: 'system', content: 'Be brief.' }, { role: 'user', content: hostile }, { role: 'assistant', content: 'ok' }, { role: 'developer', content: 'x' }],
  max_tokens: 64,
  temperature: 1,
};
// A conversation with tools: a call, its result, and every option set.
const toolRequest: PlaygroundRequest = {
  model: 'chat-default',
  messages: [
    { role: 'user', content: hostile },
    { role: 'assistant', content: '', tool_calls: [{ id: 'call_"1"', type: 'function', function: { name: 'get_weather', arguments: '{"city":"Par\\"is"}' } }] },
    { role: 'tool', content: hostile, tool_call_id: 'call_"1"' },
  ],
  tools: [
    { type: 'function', function: { name: 'get_weather', description: hostile, parameters: { type: 'object', properties: { city: { type: 'string', description: hostile } }, required: ['city'], additionalProperties: false } } },
    { type: 'function', function: { name: 'now' } },
  ],
  tool_choice: { type: 'function', function: { name: 'get_weather' } },
  parallel_tool_calls: false,
};
const base = 'https://gateway.example.com';
const variants = (language: Language, style: CodeStyle) => [request, toolRequest].flatMap((shown, at) => [false, true].map(stream => ({
  name: `${language}_${style}_${stream ? 'stream' : 'unary'}${at ? '_tools' : ''}`,
  stream,
  code: snippet(language, style, shown, { baseUrl: base, stream }).code,
})));

function has(tool: string): boolean {
  return spawnSync('sh', ['-c', `command -v ${tool}`]).status === 0;
}

function run(command: string, args: string[], cwd?: string) {
  const result = spawnSync(command, args, { cwd, encoding: 'utf8', timeout: 600_000 });
  if (result.status !== 0) throw new Error(`${command} ${args.join(' ')}\n${result.stdout}\n${result.stderr}`);
}

const dir = enabled ? mkdtempSync(join(tmpdir(), 'playground-snippets-')) : '';

describe.runIf(enabled)('playground snippets compile', () => {
  it.runIf(has('bash'))('curl (bash -n)', () => {
    for (const variant of variants('curl', 'raw')) {
      const file = join(dir, `${variant.name}.sh`);
      writeFileSync(file, variant.code);
      run('bash', ['-n', file]);
    }
  });

  it.runIf(has('python3'))('Python (py_compile)', () => {
    for (const style of ['client', 'raw'] as const) {
      for (const variant of variants('python', style)) {
        const file = join(dir, `${variant.name}.py`);
        writeFileSync(file, variant.code);
        run('python3', ['-m', 'py_compile', file]);
      }
    }
    // The raw body is a Python literal equal to the JSON the gateway gets.
    const raw = variants('python', 'raw')[0].code;
    const literal = raw.slice(raw.indexOf('body = ') + 7, raw.indexOf('\nrequest = '));
    const parsed = execFileSync('python3', ['-c', 'import ast, json, sys; print(json.dumps(ast.literal_eval(sys.stdin.read())))'], { input: literal, encoding: 'utf8' });
    expect(JSON.parse(parsed).messages[1].content).toBe(hostile);
    const rawTools = variants('python', 'raw')[2].code;
    const toolsLiteral = rawTools.slice(rawTools.indexOf('body = ') + 7, rawTools.indexOf('\nrequest = '));
    const toolsParsed = execFileSync('python3', ['-c', 'import ast, json, sys; print(json.dumps(ast.literal_eval(sys.stdin.read())))'], { input: toolsLiteral, encoding: 'utf8' });
    expect(JSON.parse(toolsParsed)).toEqual(requestBody(toolRequest));
  });

  it('JavaScript (node --check)', () => {
    for (const variant of variants('javascript', 'raw')) {
      const file = join(dir, `${variant.name}.mjs`);
      writeFileSync(file, variant.code);
      run(process.execPath, ['--check', file]);
    }
  });

  it.runIf(has('cargo'))('Rust (cargo check --offline)', () => {
    for (const style of ['client', 'raw'] as const) {
      const crate = join(dir, `rust_${style}`);
      mkdirSync(join(crate, 'src', 'bin'), { recursive: true });
      const dependencies = style === 'client'
        ? `qucelerate-ai-gateway = { path = ${JSON.stringify(join(repo, 'clients/rust'))} }\ntokio = { version = "1", features = ["macros", "rt-multi-thread"] }\nfutures-util = "0.3"\nserde_json = "1"\n`
        : 'reqwest = { version = "0.12", default-features = false, features = ["blocking", "json", "rustls-tls"] }\nserde_json = "1"\n';
      writeFileSync(join(crate, 'Cargo.toml'), `[package]\nname = "check_${style}"\nversion = "0.0.0"\nedition = "2021"\n\n[workspace]\n\n[dependencies]\n${dependencies}`);
      for (const variant of variants('rust', style)) writeFileSync(join(crate, 'src', 'bin', `${variant.name}.rs`), variant.code);
      run('cargo', ['check', '--offline', '--quiet', '--bins'], crate);
    }
  }, 900_000);

  it.runIf(has('go'))('Go (go vet)', () => {
    for (const style of ['client', 'raw'] as const) {
      for (const variant of variants('go', style)) {
        const module = join(dir, variant.name);
        mkdirSync(module, { recursive: true });
        writeFileSync(join(module, 'main.go'), variant.code);
        writeFileSync(join(module, 'go.mod'), `module check\n\ngo 1.22\n${style === 'client' ? `\nrequire github.com/qucelerate/ai-gateway/clients/go v0.0.0\nreplace github.com/qucelerate/ai-gateway/clients/go => ${join(repo, 'clients/go')}\n` : ''}`);
        run('go', ['vet', '.'], module);
      }
    }
  });

  it.runIf(has('javac'))('Java raw HTTP (javac)', () => {
    for (const variant of variants('java', 'raw')) {
      const folder = join(dir, variant.name);
      mkdirSync(folder, { recursive: true });
      writeFileSync(join(folder, 'Chat.java'), variant.code);
      run('javac', ['-d', folder, join(folder, 'Chat.java')]);
    }
  });

  it.runIf(has('mvn') && has('javac'))('Java client (javac against the built client)', () => {
    run('mvn', ['-q', '-DskipTests', 'package', 'dependency:build-classpath', `-Dmdep.outputFile=${join(dir, 'cp.txt')}`], join(repo, 'clients/java'));
    const classpath = `${join(repo, 'clients/java/target/classes')}:${execFileSync('cat', [join(dir, 'cp.txt')], { encoding: 'utf8' }).trim()}`;
    for (const variant of variants('java', 'client')) {
      const folder = join(dir, variant.name);
      mkdirSync(folder, { recursive: true });
      writeFileSync(join(folder, 'Chat.java'), variant.code);
      run('javac', ['-cp', classpath, '-d', folder, join(folder, 'Chat.java')]);
    }
  });

  it.runIf(has('g++'))('C++ client (g++ -fsyntax-only)', () => {
    for (const variant of variants('cpp', 'client')) {
      const file = join(dir, `${variant.name}.cpp`);
      writeFileSync(file, variant.code);
      run('g++', ['-std=c++17', '-Wall', '-Wtrigraphs', '-Werror', '-fsyntax-only', '-I', join(repo, 'clients/c/include'), file]);
    }
  });

  it.runIf(has('gcc'))('C raw HTTP (gcc -fsyntax-only, strict C11 so trigraphs apply)', () => {
    // A stand-in for libcurl's header where it is not installed.
    const include = join(dir, 'include');
    if (spawnSync('sh', ['-c', 'echo "#include <curl/curl.h>" | gcc -fsyntax-only -x c -']).status !== 0) {
      mkdirSync(join(include, 'curl'), { recursive: true });
      writeFileSync(join(include, 'curl', 'curl.h'), [
        '#include <stddef.h>',
        'typedef void CURL; typedef int CURLcode; typedef int CURLoption;',
        'struct curl_slist;',
        'enum { CURLE_OK = 0, CURL_GLOBAL_DEFAULT = 3, CURLOPT_URL = 1, CURLOPT_HTTPHEADER, CURLOPT_POSTFIELDS, CURLOPT_WRITEFUNCTION, CURLOPT_FAILONERROR };',
        'CURLcode curl_global_init(long); void curl_global_cleanup(void); CURL *curl_easy_init(void); void curl_easy_cleanup(CURL *);',
        'CURLcode curl_easy_setopt(CURL *, CURLoption, ...); CURLcode curl_easy_perform(CURL *); const char *curl_easy_strerror(CURLcode);',
        'struct curl_slist *curl_slist_append(struct curl_slist *, const char *); void curl_slist_free_all(struct curl_slist *);',
      ].join('\n'));
    }
    for (const variant of variants('cpp', 'raw')) {
      const file = join(dir, `${variant.name}.c`);
      writeFileSync(file, variant.code);
      run('gcc', ['-std=c11', '-Wall', '-Wextra', '-Werror', '-fsyntax-only', '-I', include, file]);
    }
  });
});
