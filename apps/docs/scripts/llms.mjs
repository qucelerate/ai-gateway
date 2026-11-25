// AI-readable copies of the site, written into the build output by the
// VitePress `buildEnd` hook (.vitepress/config.mts):
//
//   /llms.txt         an index of every page (https://llmstxt.org): title,
//                     summary, then sections of links with one-line
//                     descriptions, in sidebar order
//   /llms-full.txt    every page's Markdown, concatenated in the same order
//   /<page>.md        each page's Markdown next to its HTML (/guide/concepts
//                     is /guide/concepts.md, /guide/ is /guide/index.md)
//
// The Markdown is the page source made self-contained: frontmatter turned
// into text (the home page's hero and features), VitePress containers into
// blockquotes, inline SVG diagrams into their title and description, and
// explicit heading ids dropped. The output depends only on the sources, so
// two builds of a commit give the same files.
//
// `npm run build` writes them; nothing else needs to run.

import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, posix } from 'node:path';

const SITE = 'https://gateway.qucelerate.com';
const TITLE = 'QuCelerate AI Gateway';
const SUMMARY =
  'QuCelerate AI Gateway is a self-hosted, open-source (MIT) LLM gateway written in Rust. It puts one versioned chat, streaming, embeddings and batch API in front of OpenAI, Anthropic, Gemini, AWS Bedrock, Azure OpenAI, Azure AI Foundry and OpenAI-compatible servers, and enforces budgets and rate limits at the organization, team, service account, API key and end-user level, with versioned catalog releases, tracing and an admin console.';
const NOTES = [
  'Every page is also served as Markdown at its URL with `.md` appended (a directory index is `index.md`); `/llms-full.txt` holds all of them in one file.',
  'To install the gateway, follow the installation runbook first: it gives each command, its expected output, and what to do when the output differs. To diagnose a deployment, run `gateway doctor` (add `--json` for machine-readable output) and look up the result in the troubleshooting guide and the error-code reference.',
  'The source code and issue tracker are at https://github.com/qucelerate/ai-gateway. Budgets are a strong control, not a hard ceiling on provider bills: see the budgets page for the known gaps.'
];
// Pages listed first, in this order, under "Start here".
const START_HERE = ['install/agents.md', 'support/troubleshooting.md', 'support/errors.md', 'support/index.md', 'guide/concepts.md'];
// Section names in llms.txt, by sidebar; others take their first group's.
const SECTION_TITLES = {
  '/guide/': 'Guide',
  '/architecture/': 'Architecture',
  '/quickstarts/': 'Quickstarts',
  '/reference/': 'API reference',
  '/operations/': 'Operations',
  '/security/': 'Security',
  '/support/': 'Support',
  '/troubleshooting/': 'Troubleshooting',
  '/clients/': 'Client libraries',
  '/contributing/': 'Contributing'
};

/** The URL path a page is served at: `guide/index.md` is `/guide/`. */
const routeOf = page => `/${page.replace(/(^|\/)index\.md$/, '$1').replace(/\.md$/, '')}`;

/** `/x/y` or `/x/` as the path of its Markdown copy. */
function markdownPath(link) {
  const [path, hash] = link.split('#');
  if (!path.startsWith('/') || /\.\w+$/.test(path)) return link;
  const md = path.endsWith('/') ? `${path}index.md` : `${path}.md`;
  return hash ? `${md}#${hash}` : md;
}

function splitFrontmatter(source) {
  const match = /^---\n([\s\S]*?)\n---\n/.exec(source);
  return match ? [match[1], source.slice(match[0].length)] : ['', source];
}

/** The small YAML subset the pages' frontmatter uses: scalars, nested
 * maps, and lists of flat maps (`hero.actions`, `features`). */
function parseFrontmatter(yaml) {
  const lines = yaml.split('\n').filter(line => line.trim() && !line.trim().startsWith('#'));
  const indentOf = line => line.length - line.trimStart().length;
  const isItem = line => line.trimStart().startsWith('- ');
  let at = 0;
  const block = indent => {
    if (at >= lines.length || !isItem(lines[at]) || indentOf(lines[at]) !== indent) return map(indent);
    const list = [];
    while (at < lines.length && indentOf(lines[at]) === indent && isItem(lines[at])) {
      // `- key: value` opens a map whose other keys sit two columns in.
      lines[at] = ' '.repeat(indent + 2) + lines[at].trimStart().slice(2);
      list.push(map(indent + 2));
    }
    return list;
  };
  const map = indent => {
    const result = {};
    while (at < lines.length && indentOf(lines[at]) === indent && !isItem(lines[at])) {
      const line = lines[at++].trim();
      const colon = line.indexOf(':');
      const key = line.slice(0, colon).trim();
      const value = line.slice(colon + 1).trim();
      if (value) result[key] = unquote(value);
      else if (at < lines.length && indentOf(lines[at]) > indent) result[key] = block(indentOf(lines[at]));
      else result[key] = '';
    }
    return result;
  };
  return block(0);
}

const unquote = value => value.replace(/^(['"])(.*)\1$/, '$2');

/** The home page's hero and features as Markdown. */
function homeMarkdown(meta, body) {
  const hero = meta.hero ?? {};
  const out = [`# ${hero.name ?? TITLE}`, ''];
  if (hero.text) out.push(`**${hero.text}**`, '');
  if (hero.tagline) out.push(hero.tagline, '');
  const actions = (hero.actions ?? []).filter(action => action.link?.startsWith('/'));
  if (actions.length) out.push(actions.map(action => `[${action.text}](${markdownPath(action.link)})`).join(' · '), '');
  if (meta.features?.length) {
    out.push('## Features', '');
    for (const feature of meta.features) {
      const more = feature.link ? ` ([${feature.linkText ?? 'More'}](${markdownPath(feature.link)}))` : '';
      out.push(`- **${feature.title}.** ${feature.details}${more}`);
    }
    out.push('');
  }
  return out.join('\n') + body;
}

/** `markdown` with every relative or root-relative link made absolute on
 * the published site, resolved against `page`'s own directory, so it still
 * works once the page is copied into llms-full.txt. In-page anchors point
 * at the page's own Markdown copy. Fenced code is left alone. */
export function absoluteLinks(markdown, page) {
  const base = posix.dirname(page);
  const resolve = target => {
    if (/^[a-z][a-z0-9+.-]*:/i.test(target) || target.startsWith('//')) return target;
    if (target.startsWith('#')) return `${SITE}/${page}${target}`;
    const path = target.startsWith('/') ? posix.normalize(target) : `/${posix.join(base, target)}`;
    return path.startsWith('/..') ? target : `${SITE}${path}`;
  };
  let fence = null;
  return markdown
    .split('\n')
    .map(line => {
      const fenceMatch = /^\s*(```+|~~~+)/.exec(line);
      if (fenceMatch && (!fence || line.trim().startsWith(fence))) {
        fence = fence ? null : fenceMatch[1];
        return line;
      }
      if (fence) return line;
      return line
        .replace(/(\]\()([^)\s]+)((?:\s+"[^"]*")?\))/g, (_, open, target, close) => open + resolve(target) + close)
        .replace(/^(\s*\[[^\]]+\]:\s+)(\S+)/, (_, open, target) => open + resolve(target));
    })
    .join('\n');
}

/** A page's source as self-contained Markdown. */
export function toMarkdown(source) {
  const [yaml, body] = splitFrontmatter(source);
  const meta = parseFrontmatter(yaml);
  let text = meta.layout === 'home' ? homeMarkdown(meta, body) : body;
  // Inline SVG diagrams: their title and description, and the caption.
  text = text.replace(/<figure[^>]*>([\s\S]*?)<\/figure>/g, (_, inner) => {
    const pick = tag => new RegExp(`<${tag}[^>]*>([\\s\\S]*?)</${tag}>`).exec(inner)?.[1].replace(/<[^>]+>/g, '').trim();
    const words = [pick('desc'), pick('figcaption')].filter(Boolean).join(' ');
    return `> **Diagram: ${pick('title') ?? 'untitled'}.** ${words}`;
  });
  // Source comments (such as a generated page's notice) are not content.
  text = text.replace(/^<!--[\s\S]*?-->\n*/gm, '');
  // VitePress containers (::: tip Title … :::) as blockquotes. Fenced code
  // is left alone.
  const lines = text.split('\n');
  const out = [];
  let fence = null;
  let container = false;
  for (const line of lines) {
    const fenceMatch = /^(```+|~~~+)/.exec(line);
    if (fenceMatch && (!fence || line.startsWith(fence))) fence = fence ? null : fenceMatch[1];
    if (!fence) {
      const open = /^:::\s*(\w+)\s*(.*)$/.exec(line);
      if (!container && open) {
        container = true;
        const label = open[2] || open[1][0].toUpperCase() + open[1].slice(1);
        out.push(`> **${label}:**`);
        continue;
      }
      if (container && /^:::\s*$/.test(line)) {
        container = false;
        continue;
      }
      // Explicit heading ids ({#id}) are VitePress syntax.
      const heading = line.replace(/^(#{1,6} .*?)\s*\{#[\w-]+\}\s*$/, '$1');
      out.push(container ? (heading ? `> ${heading}` : '>') : heading);
      continue;
    }
    out.push(container ? `> ${line}` : line);
  }
  return out.join('\n').replace(/\n{3,}/g, '\n\n').trim() + '\n';
}

/** The page's title: frontmatter, else its first `#` heading. */
function titleOf(source, markdown) {
  const [yaml] = splitFrontmatter(source);
  const meta = parseFrontmatter(yaml);
  return meta.title ?? /^# (.+)$/m.exec(markdown)?.[1].trim() ?? 'Untitled';
}

/** One line about the page: its frontmatter description, else the first
 * sentence of its first paragraph, without Markdown links. */
function descriptionOf(source, markdown) {
  const [yaml] = splitFrontmatter(source);
  const meta = parseFrontmatter(yaml);
  if (meta.description) return meta.description;
  const paragraph = markdown
    .split(/\n\s*\n/)
    .map(block => block.trim())
    .find(block => block && !/^(#|>|```|\||- |\d+\. |<)/.test(block));
  if (!paragraph) return '';
  const plain = paragraph
    .replace(/\[([^\]]+)\]\([^)]+\)/g, '$1')
    .replace(/\*\*([^*]+)\*\*/g, '$1')
    .replace(/\s+/g, ' ');
  const sentence = /^.*?[.!?](?=\s|$)/.exec(plain)?.[0] ?? plain;
  return sentence.length > 220 ? `${sentence.slice(0, 217).trimEnd()}…` : sentence;
}

/** Sidebar groups as sections: [{ title, pages: [relative .md path] }]. */
function sections(sidebar, pages) {
  const known = new Set(pages);
  const seen = new Set();
  const result = [];
  const take = page => {
    if (!known.has(page) || seen.has(page)) return false;
    seen.add(page);
    return true;
  };
  const start = START_HERE.filter(take);
  if (start.length) result.push({ title: 'Start here', pages: start });
  // Each sidebar lists its own pages under its section; links it makes to
  // other sections are listed there.
  for (const [prefix, groups] of Object.entries(sidebar ?? {})) {
    const items = groups
      .flatMap(group => group.items ?? [])
      .map(item => item.link)
      .filter(link => link?.startsWith(prefix) && !/\.\w+$/.test(link.replace(/\/$/, '')))
      .map(link => markdownPath(link).slice(1))
      .filter(take);
    if (items.length) result.push({ title: SECTION_TITLES[prefix] ?? groups[0].text, pages: items });
  }
  const rest = pages.filter(take).filter(page => page !== 'index.md');
  if (rest.length) result.push({ title: 'Other pages', pages: rest });
  return result;
}

/**
 * Writes the Markdown copies, llms.txt and llms-full.txt into `outDir`.
 * `pages` are the source pages relative to `srcDir` (VitePress's
 * siteConfig.pages); `sidebar` orders them.
 */
export function writeLlmsFiles({ srcDir, outDir, pages, sidebar }) {
  const sorted = [...pages].filter(page => page !== 'README.md').sort();
  const docs = new Map();
  for (const page of sorted) {
    const source = readFileSync(join(srcDir, page), 'utf8');
    const markdown = toMarkdown(source);
    docs.set(page, { markdown, title: titleOf(source, markdown), description: descriptionOf(source, markdown) });
    const target = join(outDir, page);
    mkdirSync(dirname(target), { recursive: true });
    writeFileSync(target, markdown);
  }

  const ordered = sections(sidebar, sorted);
  const index = [`# ${TITLE}`, '', `> ${SUMMARY}`, '', ...NOTES.flatMap(note => [note, '']).slice(0, -1)];
  for (const section of ordered) {
    index.push('', `## ${section.title}`, '');
    for (const page of section.pages) {
      const { title, description } = docs.get(page);
      index.push(`- [${title}](${SITE}/${page})${description ? `: ${description}` : ''}`);
    }
  }
  index.push('', '## Optional', '');
  index.push(`- [Home page](${SITE}/index.md): the product overview and a short quickstart.`);
  index.push(`- [Inference OpenAPI](${SITE}/openapi/inference-v1.json): the versioned inference contract as OpenAPI 3.1.`);
  index.push(`- [All pages in one file](${SITE}/llms-full.txt): every page above, concatenated as Markdown.`);
  writeFileSync(join(outDir, 'llms.txt'), index.join('\n') + '\n');

  const full = [`# ${TITLE}: full documentation`, '', `> ${SUMMARY}`, '', NOTES[0], ''];
  const order = ['index.md', ...ordered.flatMap(section => section.pages)];
  for (const page of order) {
    const doc = docs.get(page);
    if (!doc) continue;
    full.push('---', '', `Source: ${SITE}${routeOf(page)}`, '', absoluteLinks(doc.markdown, page));
  }
  writeFileSync(join(outDir, 'llms-full.txt'), full.join('\n'));
  return { pages: sorted.length, sections: ordered.length };
}
