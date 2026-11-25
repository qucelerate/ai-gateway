// A small syntax highlighter for the playground's snippets: strings,
// comments, numbers and keywords, nothing more. It only colours text; the
// text itself is exactly the snippet's, so copying a block copies the code.
import React from 'react';

export type TokenKind = 'keyword' | 'string' | 'comment' | 'number' | 'plain';
export type Token = { kind: TokenKind; text: string };

const keywords: Record<string, readonly string[]> = {
  sh: ['curl', 'export', 'if', 'then', 'fi', 'echo'],
  python: ['import', 'from', 'as', 'def', 'return', 'for', 'in', 'if', 'else', 'with', 'print', 'None', 'True', 'False', 'and', 'or', 'not', 'try', 'except', 'raise'],
  javascript: ['const', 'let', 'var', 'await', 'async', 'function', 'return', 'for', 'of', 'if', 'else', 'new', 'import', 'from', 'throw', 'true', 'false', 'null'],
  rust: ['use', 'fn', 'let', 'mut', 'async', 'await', 'match', 'if', 'else', 'for', 'in', 'while', 'loop', 'return', 'pub', 'struct', 'impl', 'Some', 'None', 'Ok', 'Err', 'true', 'false'],
  go: ['package', 'import', 'func', 'var', 'const', 'if', 'else', 'for', 'range', 'return', 'defer', 'nil', 'err', 'true', 'false', 'type', 'struct'],
  java: ['import', 'package', 'public', 'private', 'static', 'final', 'class', 'void', 'var', 'new', 'return', 'if', 'else', 'for', 'try', 'catch', 'throws', 'throw', 'null', 'true', 'false'],
  cpp: ['#include', 'int', 'auto', 'const', 'return', 'if', 'else', 'for', 'while', 'std', 'nullptr', 'true', 'false', 'static', 'struct', 'char', 'void', 'size_t'],
};

/** The pattern of one language's strings and comments, in priority order at
 * the same position (a longer form before its prefix). */
function patternOf(syntax: string): RegExp {
  const hash = syntax === 'sh' || syntax === 'python';
  const parts: string[] = [];
  if (!hash) parts.push(String.raw`(?<comment>\/\*[\s\S]*?\*\/|\/\/[^\n]*)`);
  else parts.push(String.raw`(?<comment>#[^\n]*)`);
  const strings: string[] = [];
  if (syntax === 'python' || syntax === 'java') strings.push(String.raw`"""[\s\S]*?"""`);
  if (syntax === 'rust') strings.push(String.raw`r#+"[\s\S]*?"#+`);
  if (syntax === 'cpp') strings.push(String.raw`R"[^(\s"]*\([\s\S]*?\)[^"\s]*"`);
  if (syntax === 'javascript' || syntax === 'go') strings.push(String.raw`\x60(?:\\[\s\S]|[^\x60\\])*\x60`);
  strings.push(String.raw`"(?:\\[\s\S]|[^"\\\n])*"`);
  // A shell's single quotes span lines (a JSON body); elsewhere they are
  // strings (Python, JavaScript) or one-character literals.
  strings.push(syntax === 'sh' || syntax === 'python' || syntax === 'javascript'
    ? String.raw`'(?:\\[\s\S]|[^'\\])*'`
    : String.raw`'(?:\\.|[^'\\\n])'`);
  parts.push(`(?<string>${strings.join('|')})`);
  parts.push(String.raw`(?<number>\b\d+(?:\.\d+)?\b)`);
  parts.push(String.raw`(?<word>#?[A-Za-z_][A-Za-z0-9_]*)`);
  return new RegExp(parts.join('|'), 'g');
}

/** `code` as tokens whose texts, joined, are `code` exactly. */
export function tokenize(code: string, syntax: string): Token[] {
  const words = new Set(keywords[syntax] ?? []);
  const tokens: Token[] = [];
  let at = 0;
  for (const match of code.matchAll(patternOf(syntax))) {
    const start = match.index ?? 0;
    if (start > at) tokens.push({ kind: 'plain', text: code.slice(at, start) });
    const groups = match.groups ?? {};
    const kind: TokenKind = groups.comment !== undefined ? 'comment'
      : groups.string !== undefined ? 'string'
        : groups.number !== undefined ? 'number'
          : words.has(match[0]) ? 'keyword' : 'plain';
    tokens.push({ kind, text: match[0] });
    at = start + match[0].length;
  }
  if (at < code.length) tokens.push({ kind: 'plain', text: code.slice(at) });
  return tokens;
}

/** The tokens of `code` split into lines, each token kept whole within a
 * line (a multi-line string becomes one piece per line). */
export function tokenLines(code: string, syntax: string): Token[][] {
  const lines: Token[][] = [[]];
  for (const token of tokenize(code, syntax)) {
    token.text.split('\n').forEach((piece, index) => {
      if (index > 0) lines.push([]);
      if (piece) lines[lines.length - 1].push({ kind: token.kind, text: piece });
    });
  }
  return lines;
}

/** `code` highlighted, one numbered line per span (numbers come from CSS, so
 * they are not part of the text). */
export function HighlightedCode({ code, syntax }: { code: string; syntax: string }) {
  const lines = tokenLines(code, syntax);
  return (
    <code className={`language-${syntax} code-lines`}>
      {lines.map((line, index) => (
        <span className="code-line" key={index}>
          {line.map((token, at) => (token.kind === 'plain'
            ? <React.Fragment key={at}>{token.text}</React.Fragment>
            : <span key={at} className={`tok-${token.kind}`}>{token.text}</span>))}
          {index < lines.length - 1 ? '\n' : ''}
        </span>
      ))}
    </code>
  );
}
