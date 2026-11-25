import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Button as CarbonButton } from '@carbon/react';
import {
  ChevronDown, ChevronUp, Close, Code, Compare, Renew, SendFilled, StopFilled, View, ViewOff,
} from '@carbon/icons-react';
import { allPages, api, type ApiProblem, type CatalogDeployment, type CatalogPrice, type CatalogRoute, type ServiceAccount } from './api';
import { HighlightedCode } from './code-highlight';
import { toProblem } from './local-accounts';
import { formatMoney } from './money';
import { estimateCost, InferenceError, sendChat, type PlaygroundResult } from './playground-api';
import { storedKey, storeKey, type HeldKey } from './playground-key-store';
import {
  EMBEDDINGS_PATH, MAX_EMBEDDING_INPUTS, cosine, embeddingInputs, embeddingLanguages, embeddingModels, embeddingSnippet, sendEmbeddings,
  type EmbeddingModel, type EmbeddingResult,
} from './playground-embeddings';
import {
  CHAT_PATH, STREAM_PATH, hasClient, languages, snippet,
  type ChatMessage, type CodeStyle, type Language, type PlaygroundRequest, type PlaygroundTool, type ToolCall, type ToolChoice,
} from './playground-code';
import { ReportError, SessionExpired, type SignIn } from './screen-feedback';
import { emptyView, writeView } from './table-view';
import {
  AppLink, Button, CheckOption, CopyButton, Dialog, EmptyState, FormField, InlineError, Message, NoCatalog, RadioOption, Select, TextArea, TextInput,
  formatDate,
} from './ui';
import { readDashboard, writeDashboard } from './usage-dashboard';

/** A chat alias the organization routes, with what it costs and allows. */
export type PlaygroundModel = {
  alias: string;
  model: string;
  deploymentName: string;
  /** Where the preferred deployment runs, when the catalog says. */
  location: string | null;
  /** How many deployments the alias routes to (D-023). */
  deployments: number;
  price: CatalogPrice | null;
  maxOutputTokens: number | null;
  /** Whether every deployment of the alias takes tools (D-032, D-023). */
  tools: boolean;
};

type Load =
  | { state: 'loading' }
  | { state: 'failed'; problem: ApiProblem }
  | { state: 'ready'; models: PlaygroundModel[]; embeddings: EmbeddingModel[] };

/** An embeddings reply, with the inputs it embedded and the price of the alias. */
type EmbeddingRun = EmbeddingResult & { alias: string; inputs: string[]; price: CatalogPrice | null };

/** A reply with what it cost: the price of the alias it was sent to. */
type Reply = PlaygroundResult & { alias: string; price: CatalogPrice | null };
/** One turn of a conversation; an assistant turn the page received keeps
 * its reply's facts. */
type Entry = ChatMessage & { reply?: Reply };
/** One conversation with one alias. Comparing runs two or three. */
type Lane = {
  id: number;
  alias: string;
  entries: Entry[];
  /** The user message on its way, shown until its reply arrives. */
  pending: string | null;
  partial: string;
  /** The tool calls of the reply on its way, as they stream. */
  partialCalls: ToolCall[];
  failure: InferenceError | null;
  /** The message a failed request carried, to retry it. */
  failedPrompt: string | null;
  sending: boolean;
};

const MAX_TOKENS_LIMIT = 4096;
const TEMPORARY_KEY_MS = 60 * 60 * 1000;
const MAX_LANES = 3;

/** The service account a person's temporary playground keys belong to. */
export function playgroundAccountName(userId: string): string {
  return `playground-${userId}`;
}

/** Chat aliases of a release, by alias, with their deployment's chat price.
 * An alias routed to several deployments (D-023) shows its preferred one
 * (lowest priority, then highest weight) and the tightest output cap of all
 * of them, which is the cap the gateway applies. */
export function playgroundModels(routes: CatalogRoute[], deployments: CatalogDeployment[]): PlaygroundModel[] {
  const byId = new Map(deployments.map(deployment => [deployment.id, deployment]));
  const chatRoutes = routes.filter(route => route.operation === 'chat');
  const models = new Map<string, PlaygroundModel & { rank: [number, number]; ids: Set<string> }>();
  for (const route of routes) {
    if (route.operation !== 'chat' && route.operation !== 'chat_stream') continue;
    // A `chat_stream` route of a release stored before D-020 counts only
    // where the alias has no chat route.
    if (route.operation === 'chat_stream' && chatRoutes.some(other => other.alias === route.alias)) continue;
    // Streaming is priced and capped by the chat operation (D-020).
    const deployment = byId.get(route.deployment_id);
    const chat = deployment?.operations.find(operation => operation.operation === 'chat');
    const cap = chat?.capability?.max_output_tokens ?? null;
    const tools = chat?.capability?.tools === true;
    const rank: [number, number] = [route.priority ?? 1, -(route.weight ?? 1)];
    const existing = models.get(route.alias);
    const tighter = (a: number | null, b: number | null) => (a === null ? b : b === null ? a : Math.min(a, b));
    if (existing) {
      existing.maxOutputTokens = tighter(existing.maxOutputTokens, cap);
      // An alias takes tools only if every deployment it routes to does.
      existing.tools &&= tools;
      existing.ids.add(route.deployment_id);
      existing.deployments = existing.ids.size;
      const better = rank[0] < existing.rank[0] || (rank[0] === existing.rank[0] && rank[1] < existing.rank[1]);
      if (!better) continue;
      models.set(route.alias, {
        ...existing, model: route.model, deploymentName: route.deployment_name, location: deployment?.location ?? null, price: chat?.price ?? null, rank,
      });
      continue;
    }
    models.set(route.alias, {
      alias: route.alias,
      model: route.model,
      deploymentName: route.deployment_name,
      location: deployment?.location ?? null,
      deployments: 1,
      price: chat?.price ?? null,
      maxOutputTokens: cap,
      tools,
      rank,
      ids: new Set([route.deployment_id]),
    });
  }
  return [...models.values()]
    .map(({ rank: _rank, ids: _ids, ...model }) => model)
    .sort((a, b) => a.alias.localeCompare(b.alias));
}

export type Form = {
  alias: string;
  /** The instructions: the system prompt. */
  system: string;
  /** The conversation so far. */
  messages: ChatMessage[];
  /** What the composer holds: the next user message. */
  draft: string;
  maxTokens: string;
  temperature: string;
  /** The tools the model may call, as the Tools section holds them. */
  tools?: ToolDraft[];
  /** `auto`, `none`, `required`, or `function:` and a tool's name. */
  toolChoice?: string;
  /** Whether a reply may make several calls. */
  parallel?: boolean;
  /** Sends the conversation as it is, after tool results: no new message. */
  continuing?: boolean;
};

/** A tool as the Tools section holds it: its parameters as JSON text. */
export type ToolDraft = { key: number; name: string; description: string; parameters: string };

type FormErrors = Partial<Record<'maxTokens' | 'temperature' | 'messages' | 'alias' | 'tools', string>>;

const TOOL_NAME = /^[A-Za-z_][A-Za-z0-9_-]{0,63}$/;
const MAX_TOOLS = 128;

/** The tools a form defines, or why one is wrong. */
export function buildTools(drafts: ToolDraft[]): { tools: PlaygroundTool[]; problem: string | null; problems: Map<number, string> } {
  const problems = new Map<number, string>();
  const tools: PlaygroundTool[] = [];
  const seen = new Set<string>();
  for (const draft of drafts) {
    const name = draft.name.trim();
    if (!TOOL_NAME.test(name)) {
      problems.set(draft.key, 'A name of up to 64 letters, digits, _ and -, not starting with a digit or -.');
      continue;
    }
    if (seen.has(name)) {
      problems.set(draft.key, `Another tool is named ${name}.`);
      continue;
    }
    seen.add(name);
    let parameters: Record<string, unknown> | undefined;
    if (draft.parameters.trim()) {
      let parsed: unknown;
      try {
        parsed = JSON.parse(draft.parameters);
      } catch {
        problems.set(draft.key, 'The parameters are not valid JSON.');
        continue;
      }
      if (typeof parsed !== 'object' || parsed === null || Array.isArray(parsed) || (parsed as { type?: unknown }).type !== 'object') {
        problems.set(draft.key, 'The parameters are a JSON Schema whose "type" is "object".');
        continue;
      }
      parameters = parsed as Record<string, unknown>;
    }
    tools.push({
      type: 'function',
      function: { name, ...(draft.description.trim() ? { description: draft.description } : {}), ...(parameters ? { parameters } : {}) },
    });
  }
  const problem = problems.size > 0
    ? `${problems.size === 1 ? 'A tool needs' : `${problems.size} tools need`} fixing.`
    : drafts.length > MAX_TOOLS ? `A request holds at most ${MAX_TOOLS} tools.` : null;
  return { tools, problem, problems };
}

/** The calls of `messages` no tool message answers yet. */
export function unansweredCalls(messages: ChatMessage[]): ToolCall[] {
  let last = messages.length - 1;
  while (last >= 0 && !(messages[last].role === 'assistant' && messages[last].tool_calls?.length)) last -= 1;
  if (last < 0) return [];
  const later = messages.slice(last + 1);
  if (later.some(message => message.role !== 'tool')) return [];
  const answered = new Set(later.map(message => message.tool_call_id));
  return messages[last].tool_calls!.filter(call => !answered.has(call.id));
}

/** The request the form describes (the conversation, then the composer's
 * message), or what is wrong with it. */
export function buildRequest(form: Form, model: PlaygroundModel | undefined): { request: PlaygroundRequest; errors: FormErrors } {
  const errors: FormErrors = {};
  if (!form.alias) errors.alias = 'Choose a model.';
  const messages: ChatMessage[] = [
    ...(form.system.trim() ? [{ role: 'system' as const, content: form.system }] : []),
    ...form.messages.map(({ role, content, tool_calls, tool_call_id }) => ({
      role, content, ...(tool_calls?.length ? { tool_calls } : {}), ...(tool_call_id !== undefined ? { tool_call_id } : {}),
    })),
    ...(!form.continuing && form.draft.trim() ? [{ role: 'user' as const, content: form.draft }] : []),
  ];
  const awaiting = unansweredCalls(form.messages).length > 0;
  if (form.continuing) {
    if (awaiting) errors.messages = 'Enter the result of every tool call.';
  } else if (awaiting) {
    errors.messages = 'Enter the tool calls’ results first, or start a new chat.';
  } else if (!form.draft.trim()) {
    errors.messages = 'Write a message.';
  }
  if (messages.length > 128) errors.messages = 'A request holds at most 128 messages. Start a new chat.';
  const limit = Math.min(MAX_TOKENS_LIMIT, model?.maxOutputTokens ?? MAX_TOKENS_LIMIT);
  let maxTokens: number | undefined;
  if (form.maxTokens.trim()) {
    maxTokens = Number(form.maxTokens);
    if (!Number.isInteger(maxTokens) || maxTokens < 1 || maxTokens > limit) errors.maxTokens = `A whole number from 1 to ${limit}.`;
  }
  let temperature: number | undefined;
  if (form.temperature.trim()) {
    temperature = Number(form.temperature);
    if (!Number.isFinite(temperature) || temperature < 0 || temperature > 2) errors.temperature = 'A number from 0 to 2.';
  }
  const { tools, problem } = buildTools(form.tools ?? []);
  if (problem) errors.tools = problem;
  const calls = form.messages.some(message => message.role === 'tool' || message.tool_calls?.length);
  if (!problem && calls && tools.length === 0) errors.tools = 'The conversation has tool calls: define its tools, or start a new chat.';
  let toolChoice: ToolChoice | undefined;
  const choice = form.toolChoice ?? 'auto';
  if (tools.length > 0 && choice !== 'auto') {
    if (choice.startsWith('function:')) {
      const name = choice.slice('function:'.length);
      if (tools.some(tool => tool.function.name === name)) toolChoice = { type: 'function', function: { name } };
      else if (!errors.tools) errors.tools = `No tool is named ${name}; choose another tool choice.`;
    } else {
      toolChoice = choice as ToolChoice;
    }
  }
  return {
    request: {
      model: form.alias,
      messages,
      ...(maxTokens !== undefined && !errors.maxTokens ? { max_tokens: maxTokens } : {}),
      ...(temperature !== undefined && !errors.temperature ? { temperature } : {}),
      ...(tools.length > 0 ? { tools } : {}),
      ...(toolChoice !== undefined ? { tool_choice: toolChoice } : {}),
      ...(tools.length > 0 && form.parallel === false ? { parallel_tool_calls: false } : {}),
    },
    errors,
  };
}

/** The request a snippet shows: the conversation on screen, or an example
 * question while there is none, so the code always sends something. */
export function codeRequest(request: PlaygroundRequest): PlaygroundRequest {
  // Calls still waiting for their results get a placeholder for each, so
  // the code sends a conversation the gateway takes.
  const messages = request.messages.flatMap((message, index): ChatMessage[] => {
    if (message.role !== 'assistant' || !message.tool_calls?.length) return [message];
    const answered = new Set(request.messages.slice(index + 1).filter(later => later.role === 'tool').map(later => later.tool_call_id));
    const missing = message.tool_calls.filter(call => !answered.has(call.id));
    return [message, ...missing.map(call => ({ role: 'tool' as const, tool_call_id: call.id, content: `The result of ${call.function.name}.` }))];
  });
  const shown = messages.length === request.messages.length ? request : { ...request, messages };
  if (shown.messages.some(message => message.role === 'user' && message.content.trim())) return shown;
  return { ...shown, messages: [...shown.messages, { role: 'user', content: 'What can you help me with?' }] };
}

let laneIds = 0;
const newLane = (alias: string): Lane => ({ id: ++laneIds, alias, entries: [], pending: null, partial: '', partialCalls: [], failure: null, failedPrompt: null, sending: false });
const clearLane = (lane: Lane): Lane => ({ ...lane, entries: [], pending: null, partial: '', partialCalls: [], failure: null, failedPrompt: null, sending: false });
let toolKeys = 0;
const EXAMPLE_PARAMETERS = JSON.stringify({ type: 'object', properties: { city: { type: 'string', description: 'The city, such as Paris.' } }, required: ['city'] }, null, 2);

/**
 * Models → Playground (D-025, D-031): a workspace with a settings rail on the
 * left (the model, the key it calls with, the system prompt, parameters and
 * advanced options) and the conversation on the right, a composer under it
 * and each reply's tokens, cost, latency and trace beside it. *Continue in
 * code* shows the code that sends the same conversation; *Compare models* sends one prompt to two or three
 * aliases side by side. The admin session never reaches the inference API.
 */
export function PlaygroundScreen({ organizationId, userId, ...signIn }: SignIn & { organizationId: string; userId: string }) {
  const [load, setLoad] = useState<Load>({ state: 'loading' });
  const [lanes, setLanes] = useState<Lane[]>(() => [newLane('')]);
  /** Whether the primary alias is used for embeddings: an alias can route
   * both chat and embeddings, and the picker lists it under each. */
  const [embedding, setEmbedding] = useState(false);
  const [system, setSystem] = useState('');
  const [draft, setDraft] = useState('');
  const [maxTokens, setMaxTokens] = useState('256');
  const [temperature, setTemperature] = useState('');
  const [stream, setStream] = useState(true);
  const [tools, setTools] = useState<ToolDraft[]>([]);
  const [toolChoice, setToolChoice] = useState('auto');
  const [parallel, setParallel] = useState(true);
  const [touched, setTouched] = useState(false);
  // Kept for the tab: a reload keeps the key, closing the tab forgets it.
  const [key, setHeldKey] = useState<HeldKey | null>(() => storedKey(organizationId, userId));
  const keyRef = useRef(key);
  const setKey = (next: HeldKey | null) => {
    keyRef.current = next;
    setHeldKey(next);
    storeKey(organizationId, userId, next);
  };
  /** A temporary key being created for a send: a second send waits for it
   * rather than creating another. */
  const creatingKey = useRef<Promise<HeldKey | null> | null>(null);
  const [pasted, setPasted] = useState('');
  const [keyBusy, setKeyBusy] = useState(false);
  const [keyProblem, setKeyProblem] = useState<ApiProblem | null>(null);
  const [open, setOpen] = useState(() => ({ key: key === null, parameters: true, tools: false, advanced: false, instructions: true }));
  const [codeOpen, setCodeOpen] = useState(false);
  const [embedText, setEmbedText] = useState('');
  const abort = useRef<AbortController | null>(null);
  /** Bumped by *New chat*: replies of an earlier batch are dropped. */
  const generation = useRef(0);

  const loadModels = useCallback(async () => {
    setLoad({ state: 'loading' });
    try {
      const deployments = await allPages<CatalogDeployment, Awaited<ReturnType<typeof api.listDeployments>>>(token =>
        api.listDeployments(organizationId, { page_size: 100, page_token: token }));
      const releaseId = deployments.first.release_id;
      if (!releaseId) {
        setLoad({ state: 'ready', models: [], embeddings: [] });
        return;
      }
      const routes = await allPages<CatalogRoute, Awaited<ReturnType<typeof api.listRoutes>>>(token =>
        api.listRoutes(organizationId, releaseId, { page_size: 100, page_token: token }));
      const models = playgroundModels(routes.items, deployments.items);
      const embeddings = embeddingModels(routes.items, deployments.items);
      setLoad({ state: 'ready', models, embeddings });
      const known = (alias: string) => models.some(model => model.alias === alias) || embeddings.some(model => model.alias === alias);
      setLanes(current => current.map((lane, index) => (lane.alias && known(lane.alias)
        ? lane
        : { ...lane, alias: models[index]?.alias ?? models[0]?.alias ?? embeddings[0]?.alias ?? '' })));
    } catch (caught) {
      setLoad({ state: 'failed', problem: toProblem(caught) });
    }
  }, [organizationId]);
  useEffect(() => { void loadModels(); }, [loadModels]);
  // Leaving the page stops replies still streaming, and sends still waiting
  // for their key.
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      abort.current?.abort();
    };
  }, []);

  const models = load.state === 'ready' ? load.models : [];
  const embeddings = load.state === 'ready' ? load.embeddings : [];
  const modelOf = (alias: string) => models.find(candidate => candidate.alias === alias);
  const primary = lanes[0];
  const model = modelOf(primary.alias);
  /** The embedding alias chosen, when the playground is in embeddings mode:
   * an alias routed only for embeddings, or one also routed for chat whose
   * embeddings entry was picked. */
  const embeddingModel = embedding || !model ? embeddings.find(candidate => candidate.alias === primary.alias) : undefined;
  const sending = lanes.some(lane => lane.sending);
  const formFor = (lane: Lane): Form => ({ alias: lane.alias, system, messages: lane.entries, draft, maxTokens, temperature, tools, toolChoice, parallel });
  const built = lanes.map(lane => buildRequest(formFor(lane), modelOf(lane.alias)));
  const errors: FormErrors = Object.assign({}, ...built.map(entry => entry.errors).reverse());
  const valid = Object.keys(errors).length === 0;
  const shownErrors = touched ? errors : {};
  // The code sends what the conversation shows: the composer's message, or
  // else a message that failed and waits to be retried.
  const primaryRequest = draft.trim() || primary.failedPrompt === null
    ? built[0].request
    : buildRequest({ ...formFor(primary), draft: primary.failedPrompt }, model).request;

  const updateLane = (id: number, change: (lane: Lane) => Lane) => setLanes(current => current.map(lane => (lane.id === id ? change(lane) : lane)));
  const toggle = (section: keyof typeof open, value?: boolean) => setOpen(current => ({ ...current, [section]: value ?? !current[section] }));

  /** Creates a temporary key and holds it; `null`, with the problem shown,
   * when it cannot. */
  const createTemporaryKey = async (): Promise<HeldKey | null> => {
    setKeyBusy(true);
    setKeyProblem(null);
    try {
      const name = playgroundAccountName(userId);
      const accounts = await allPages<ServiceAccount, Awaited<ReturnType<typeof api.listServiceAccounts>>>(token => api.listServiceAccounts(organizationId, { page_size: 100, page_token: token }));
      let account = accounts.items.find(candidate => candidate.name === name && candidate.owner_user_id === userId && candidate.enabled);
      if (!account) account = (await api.createServiceAccount(organizationId, name)).data;
      const created = (await api.createApiKey(organizationId, account.id, {
        scopes: ['Inference', 'Embeddings'],
        expires_at: new Date(Date.now() + TEMPORARY_KEY_MS).toISOString(),
      })).data;
      if (!created.secret) throw new Error('The new key’s secret was not returned.');
      const held: HeldKey = { source: 'temporary', secret: created.secret, key: created.metadata };
      if (!mounted.current) {
        // The page was left meanwhile: a later visit may hold another key
        // by now, so this one is revoked rather than kept over it.
        void api.revokeApiKey(organizationId, created.metadata.service_account_id, created.metadata.id, created.metadata.version).catch(() => {});
        return null;
      }
      setKey(held);
      toggle('key', false);
      return held;
    } catch (caught) {
      setKeyProblem(toProblem(caught));
      toggle('key', true);
      return null;
    } finally {
      setKeyBusy(false);
    }
  };
  /** The key to send with: the one held, or else a temporary one created
   * now, so trying a model needs no setup. */
  const keyForSending = async () => {
    if (keyRef.current) return keyRef.current;
    creatingKey.current ??= createTemporaryKey().finally(() => { creatingKey.current = null; });
    return creatingKey.current;
  };
  /** A chat send waiting for its key: another submit meanwhile is ignored. */
  const awaitingKey = useRef(false);
  /** The key for a send of the current chat: `null` when there is none, when
   * another send already waits, or when *New chat* or leaving the page
   * discarded the send while it waited. */
  const keyForThisChat = async () => {
    if (awaitingKey.current) return null;
    awaitingKey.current = true;
    const batch = generation.current;
    try {
      const held = await keyForSending();
      return held && mounted.current && generation.current === batch ? held : null;
    } finally {
      awaitingKey.current = false;
    }
  };

  const forgetKey = async () => {
    if (key?.source === 'temporary') {
      setKeyBusy(true);
      setKeyProblem(null);
      try {
        await api.revokeApiKey(organizationId, key.key.service_account_id, key.key.id, key.key.version);
      } catch (caught) {
        setKeyProblem(toProblem(caught));
        setKeyBusy(false);
        return;
      }
      setKeyBusy(false);
    }
    setKey(null);
  };

  /** Sends `text` after each target lane's conversation, each its own
   * request with its own idempotency key; `null` sends the conversation as
   * it is, to continue after tool results. A batch that *New chat*
   * discarded changes nothing when it finishes. */
  const run = async (text: string | null, targets: { lane: Lane; request: PlaygroundRequest; price: CatalogPrice | null }[], secret: string) => {
    const controller = new AbortController();
    abort.current = controller;
    const batch = generation.current;
    const live = () => generation.current === batch;
    const ids = new Set(targets.map(target => target.lane.id));
    setLanes(current => current.map(lane => (ids.has(lane.id) ? { ...lane, pending: text, partial: '', partialCalls: [], failure: null, failedPrompt: null, sending: true } : lane)));
    let aborted = 0;
    await Promise.all(targets.map(async ({ lane, request, price }) => {
      try {
        const answer = await sendChat(secret, request, stream, {
          signal: controller.signal,
          onDelta: piece => { if (live()) updateLane(lane.id, current => ({ ...current, partial: current.partial + piece })); },
          onToolCalls: calls => { if (live()) updateLane(lane.id, current => ({ ...current, partialCalls: calls })); },
        });
        if (!live()) return;
        const reply: Entry = {
          role: 'assistant',
          content: answer.content,
          ...(answer.toolCalls.length ? { tool_calls: answer.toolCalls } : {}),
          reply: { ...answer, alias: request.model, price },
        };
        updateLane(lane.id, current => ({
          ...current,
          entries: [...current.entries, ...(text === null ? [] : [{ role: 'user' as const, content: text }]), reply],
          pending: null,
          partial: '',
          partialCalls: [],
          sending: false,
        }));
      } catch (caught) {
        const stopped = caught instanceof DOMException && caught.name === 'AbortError';
        if (stopped) aborted += 1;
        if (!live()) return;
        const failure = caught instanceof InferenceError
          ? caught
          : stopped
            ? null
            : new InferenceError(0, 'request_failed', caught instanceof Error ? caught.message : 'The request failed.', null);
        // A failed lane keeps its message, to retry that lane alone.
        updateLane(lane.id, current => ({ ...current, pending: null, partial: '', partialCalls: [], failure, failedPrompt: stopped ? null : text, sending: false }));
      }
    }));
    // A later batch may have started meanwhile; its controller stays.
    if (abort.current === controller) abort.current = null;
    // Stopped before anything answered: the message goes back to the composer.
    if (text !== null && live() && aborted === targets.length) setDraft(current => current || text);
  };

  /** Sends the composer's message to every lane at once. */
  const send = async () => {
    setTouched(true);
    // A setting that blocks the send is shown, not left in a closed section.
    if (errors.maxTokens || errors.temperature) toggle('parameters', true);
    if (errors.tools) toggle('tools', true);
    if (!valid || sending || awaitingKey.current) return;
    const text = draft;
    const targets = lanes.map((lane, index) => ({ lane, request: built[index].request, price: modelOf(lane.alias)?.price ?? null }));
    // The composer clears at once, so what is typed while the key is made
    // is the next message, not lost when this one goes.
    setDraft('');
    setTouched(false);
    const batch = generation.current;
    const held = await keyForThisChat();
    if (!held) {
      // Not sent, and the chat still open: the message goes back, unless
      // something new was typed.
      if (mounted.current && generation.current === batch) setDraft(current => current || text);
      return;
    }
    await run(text, targets, held.secret);
  };

  /** Sends a failed lane's message again, to that lane only. */
  const retry = async (lane: Lane) => {
    if (sending || lane.failedPrompt === null) return;
    const { request, errors: retryErrors } = buildRequest({ ...formFor(lane), draft: lane.failedPrompt }, modelOf(lane.alias));
    // Settings changed since to something invalid: show why, send nothing.
    if (retryErrors.maxTokens || retryErrors.temperature || retryErrors.alias) {
      setTouched(true);
      toggle('parameters', true);
      return;
    }
    const held = await keyForThisChat();
    if (!held) return;
    await run(lane.failedPrompt, [{ lane, request, price: modelOf(lane.alias)?.price ?? null }], held.secret);
  };

  /** Answers a lane's tool calls with `results` (by call id), then sends
   * the conversation on, to that lane only. */
  const continueWith = async (lane: Lane, results: Map<string, string>) => {
    if (sending) return;
    const calls = unansweredCalls(lane.entries);
    const answers: Entry[] = calls.map(call => ({ role: 'tool', tool_call_id: call.id, content: results.get(call.id) ?? '' }));
    const entries = [...lane.entries, ...answers];
    const { request, errors: continueErrors } = buildRequest({ ...formFor(lane), messages: entries, continuing: true }, modelOf(lane.alias));
    if (Object.keys(continueErrors).length > 0) {
      setTouched(true);
      if (continueErrors.maxTokens || continueErrors.temperature) toggle('parameters', true);
      if (continueErrors.tools) toggle('tools', true);
      return;
    }
    // The results join the conversation at once: a failed send keeps them,
    // to continue again.
    updateLane(lane.id, current => ({ ...current, entries, failure: null, failedPrompt: null }));
    const held = await keyForThisChat();
    if (!held) return;
    await run(null, [{ lane: { ...lane, entries }, request, price: modelOf(lane.alias)?.price ?? null }], held.secret);
  };

  /** Sends a lane's conversation on after tool results that did not go. */
  const resume = async (lane: Lane) => {
    if (sending) return;
    const { request, errors: resumeErrors } = buildRequest({ ...formFor(lane), continuing: true }, modelOf(lane.alias));
    if (Object.keys(resumeErrors).length > 0) {
      setTouched(true);
      if (resumeErrors.tools) toggle('tools', true);
      if (resumeErrors.maxTokens || resumeErrors.temperature) toggle('parameters', true);
      return;
    }
    const held = await keyForThisChat();
    if (!held) return;
    await run(null, [{ lane, request, price: modelOf(lane.alias)?.price ?? null }], held.secret);
  };

  const newChat = () => {
    generation.current += 1;
    abort.current?.abort();
    abort.current = null;
    setLanes(current => current.map(clearLane));
    setDraft('');
    setTouched(false);
    document.getElementById('playground-draft')?.focus();
  };

  const compare = () => {
    if (lanes.length >= MAX_LANES) return;
    const unused = models.find(candidate => !lanes.some(lane => lane.alias === candidate.alias)) ?? models[0];
    setLanes(current => [...current, newLane(unused?.alias ?? '')]);
  };

  if (!organizationId) return <EmptyState title="Waiting for organization context" text="Select a confirmed organization to try its models." />;
  if (load.state === 'loading') return <p className="muted" role="status">Loading models…</p>;
  if (load.state === 'failed') return <ReportError problem={load.problem} what="models" onRetry={() => void loadModels()} {...signIn} />;
  if (models.length === 0 && embeddings.length === 0) return <NoCatalog what="The playground sends requests to the models this organization routes." />;

  const comparing = lanes.length > 1;
  const setAlias = (id: number, alias: string) => updateLane(id, lane => ({ ...lane, alias }));
  /** The primary model; an embedding model ends a comparison, which is chat only. */
  const choosePrimary = (value: string) => {
    const { operation, alias } = pickedModel(value);
    const embeds = operation === 'embeddings';
    setEmbedding(embeds);
    if (embeds) setLanes(current => current.slice(0, 1));
    setAlias(primary.id, alias);
  };
  const keyStatus = key ? (key.source === 'temporary' ? 'Temporary key' : 'Pasted key') : 'Made on first send';

  return (
    <div className="playground">
      <div className="playground-header">
        <div className="playground-heading">
          <nav aria-label="Breadcrumb">
            <ol className="playground-breadcrumb">
              <li><AppLink to="/admin/models" className="cds--link">Models</AppLink></li>
              <li aria-current="page">{comparing ? 'Comparison' : primary.alias}</li>
            </ol>
          </nav>
          <h2>{comparing ? 'Comparing models' : primary.alias}</h2>
        </div>
        <div className="playground-header-actions">
          <CarbonButton
            type="button"
            kind="tertiary"
            size="md"
            renderIcon={Compare}
            onClick={compare}
            disabled={lanes.length >= MAX_LANES || Boolean(embeddingModel) || models.length === 0}
          >
            {comparing ? 'Add a model' : 'Compare models'}
          </CarbonButton>
          <CarbonButton type="button" kind="tertiary" size="md" renderIcon={Code} onClick={() => setCodeOpen(true)}>
            Continue in code
          </CarbonButton>
        </div>
      </div>
      <div className="playground-view">
          <div className="playground-workspace">
            <aside className="playground-rail" aria-label="Settings">
              <div className="playground-rail-group playground-model-picker">
                <Select
                  id="playground-model"
                  field={{ label: 'Model' }}
                  value={pickerValue(embeddingModel ? 'embeddings' : 'chat', primary.alias)}
                  onChange={event => choosePrimary(event.target.value)}
                >
                  {embeddings.length === 0
                    ? models.map(candidate => <option key={candidate.alias} value={pickerValue('chat', candidate.alias)}>{candidate.alias}</option>)
                    : (
                      <>
                        {models.length > 0 && (
                          <optgroup label="Chat">
                            {models.map(candidate => <option key={candidate.alias} value={pickerValue('chat', candidate.alias)}>{candidate.alias}</option>)}
                          </optgroup>
                        )}
                        <optgroup label="Embeddings">
                          {embeddings.map(candidate => <option key={candidate.alias} value={pickerValue('embeddings', candidate.alias)}>{candidate.alias}</option>)}
                        </optgroup>
                      </>
                    )}
                </Select>
                {shownErrors.alias && <div className="field-error"><span role="alert">{shownErrors.alias}</span></div>}
                {embeddingModel ? <ModelFacts model={embeddingModel} /> : model && <ModelFacts model={model} cap={model.maxOutputTokens} />}
              </div>

              <Section id="playground-key" title="API key" open={open.key} onToggle={() => toggle('key')} status={keyStatus}>
                <p className="playground-help">
                  The playground calls the inference API like any application, so budgets, rate limits and usage apply as usual. Your admin session is never sent.
                  The key is kept only on this page: reloading or leaving forgets it.
                </p>
                {key ? (
                  <div className="playground-key-held">
                    {key.source === 'temporary'
                      ? <p>Using a temporary key <code>{key.key.public_prefix}…</code> on your service account <code>{playgroundAccountName(userId)}</code>, expiring {formatDate(key.key.expires_at)}.</p>
                      : <p>Using the key you pasted.</p>}
                    <Button variant="ghost" type="button" loading={keyBusy} onClick={() => void forgetKey()}>
                      {key.source === 'temporary' ? 'Revoke key' : 'Forget key'}
                    </Button>
                  </div>
                ) : (
                  <div className="playground-key-choose">
                    <form
                      className="playground-key-paste"
                      onSubmit={event => {
                        event.preventDefault();
                        if (pasted.trim()) {
                          setKey({ source: 'pasted', secret: pasted.trim() });
                          setPasted('');
                          toggle('key', false);
                        }
                      }}
                    >
                      <FormField label="Paste an API key" htmlFor="playground-key-input" hint="A key from Identities with the Inference scope, or the Embeddings scope for embedding models.">
                        <TextInput id="playground-key-input" type="password" autoComplete="off" spellCheck={false} value={pasted} onChange={event => setPasted(event.target.value)} />
                      </FormField>
                      <Button type="submit" variant="primary" disabled={!pasted.trim()}>Use key</Button>
                    </form>
                    <div className="playground-key-temporary">
                      <p>Or create a key for an hour, on a service account of your own, that can only call models (chat and embeddings).</p>
                      <CarbonButton type="button" kind="tertiary" size="md" disabled={keyBusy} aria-busy={keyBusy || undefined} className={keyBusy ? 'btn-loading' : undefined} onClick={() => void createTemporaryKey()}>
                        Create temporary key
                      </CarbonButton>
                    </div>
                  </div>
                )}
                {keyProblem && (keyProblem.status === 401
                  ? <SessionExpired action="create a key" {...signIn} />
                  : keyProblem.status === 403
                    ? <Message kind="error">Your role cannot create keys here. Paste a key someone gave you, or ask an organization owner or admin.</Message>
                    : <InlineError problem={keyProblem} />)}
              </Section>

              {!embeddingModel && <>
                <Section id="playground-instructions" title="System prompt" open={open.instructions} onToggle={() => toggle('instructions')}>
                  <TextArea
                    id="playground-system"
                    aria-label="Instructions (system prompt)"
                    rows={5}
                    placeholder="You are an AI assistant that helps people find information."
                    value={system}
                    maxLength={131072}
                    onChange={event => setSystem(event.target.value)}
                  />
                  <p className="playground-help">Sent as the system message before the conversation; leave it empty for none.</p>
                </Section>

                <Section id="playground-parameters" title="Parameters" open={open.parameters} onToggle={() => toggle('parameters')}>
                  <FormField label="Max tokens" htmlFor="playground-max-tokens" error={shownErrors.maxTokens} hint={`Up to ${Math.min(MAX_TOKENS_LIMIT, model?.maxOutputTokens ?? MAX_TOKENS_LIMIT)}; blank for the model’s default.`}>
                    <TextInput id="playground-max-tokens" inputMode="numeric" value={maxTokens} onChange={event => setMaxTokens(event.target.value)} />
                  </FormField>
                  <FormField label="Temperature" htmlFor="playground-temperature" error={shownErrors.temperature} hint="0 to 2; blank for the model’s default.">
                    <TextInput id="playground-temperature" inputMode="decimal" value={temperature} onChange={event => setTemperature(event.target.value)} />
                  </FormField>
                </Section>

                <Section
                  id="playground-tools"
                  title="Tools"
                  open={open.tools}
                  onToggle={() => toggle('tools')}
                  status={tools.length === 0 ? 'None' : tools.length === 1 ? '1 tool' : `${tools.length} tools`}
                >
                  <ToolsEditor
                    tools={tools}
                    onTools={setTools}
                    toolChoice={toolChoice}
                    onToolChoice={setToolChoice}
                    parallel={parallel}
                    onParallel={setParallel}
                    error={shownErrors.tools}
                    unsupported={lanes.map(lane => modelOf(lane.alias)).filter((candidate): candidate is PlaygroundModel => Boolean(candidate && !candidate.tools)).map(candidate => candidate.alias)}
                  />
                </Section>

                <Section id="playground-advanced" title="Advanced" open={open.advanced} onToggle={() => toggle('advanced')} status={stream ? 'Streamed' : 'All at once'}>
                  <fieldset className="playground-mode">
                    <legend className="cds--label">Response</legend>
                    <RadioOption name="playground-stream" value="stream" checked={stream} onChange={() => setStream(true)}>Streamed</RadioOption>
                    <RadioOption name="playground-stream" value="unary" checked={!stream} onChange={() => setStream(false)}>All at once</RadioOption>
                  </fieldset>
                  <p className="playground-help">Streamed replies also report the time to the first token.</p>
                </Section>
              </>}
            </aside>

            {embeddingModel ? (
              <EmbeddingsPane
                // A new model starts a new pane: its results, and a request
                // still running, belong to the model before.
                key={embeddingModel.alias}
                model={embeddingModel}
                text={embedText}
                onText={setEmbedText}
                keyForSending={async () => (await keyForSending())?.secret ?? null}
                hasKey={key !== null}
                keyBusy={keyBusy}
                onNeedKey={() => toggle('key', true)}
              />
            ) : (
              <section className="playground-stage" aria-label="Conversation" aria-busy={sending}>
                <div className="playground-stage-bar">
                  <div className="playground-stage-title">
                    <h3>{comparing ? `${lanes.length} models` : 'Conversation'}</h3>
                    {comparing && <span className="playground-help">One message goes to each model, as its own request.</span>}
                  </div>
                  <SessionStats alias={primary.alias} lanes={lanes} />
                  <div className="playground-stage-actions">
                    {comparing && <Button type="button" variant="ghost" onClick={() => setLanes(current => current.slice(0, 1))}>Stop comparing</Button>}
                    <CarbonButton type="button" kind="ghost" size="md" renderIcon={Renew} onClick={newChat}>New chat</CarbonButton>
                  </div>
                </div>
                <div className={`playground-lanes${comparing ? ' playground-lanes-compare' : ''}`}>
                  {lanes.map((lane, index) => (
                    <LaneView
                      key={lane.id}
                      lane={lane}
                      index={index}
                      comparing={comparing}
                      models={models}
                      onAlias={alias => setAlias(lane.id, alias)}
                      onRetry={!sending && lane.failedPrompt !== null ? () => void retry(lane) : undefined}
                      onToolResults={!sending ? results => void continueWith(lane, results) : undefined}
                      onResume={!sending && lane.failedPrompt === null && lane.entries.at(-1)?.role === 'tool' ? () => void resume(lane) : undefined}
                      onRemove={index > 0 ? () => setLanes(current => current.filter(other => other.id !== lane.id)) : undefined}
                    />
                  ))}
                </div>
                <form
                  className="playground-composer"
                  noValidate
                  onSubmit={event => { event.preventDefault(); void send(); }}
                >
                  <TextArea
                    id="playground-draft"
                    aria-label="Message"
                    placeholder={comparing ? 'Message the models…' : 'Message the model…'}
                    rows={3}
                    value={draft}
                    maxLength={131072}
                    onChange={event => setDraft(event.target.value)}
                    onKeyDown={event => {
                      // Enter sends; Shift+Enter is a new line; an IME's Enter
                      // only finishes its composition.
                      if (event.key === 'Enter' && !event.shiftKey && !event.nativeEvent.isComposing) {
                        event.preventDefault();
                        event.currentTarget.form?.requestSubmit();
                      }
                    }}
                  />
                  {shownErrors.messages && <div className="field-error"><span role="alert">{shownErrors.messages}</span></div>}
                  <div className="playground-composer-bar">
                    <div className="playground-composer-notes">
                      <p className="playground-help">Enter sends, Shift+Enter adds a line. AI-generated content may be incorrect.</p>
                      {!key && <NoKeyYet onNeedKey={() => toggle('key', true)} />}
                    </div>
                    {sending
                      ? <CarbonButton type="button" kind="secondary" size="md" renderIcon={StopFilled} onClick={() => abort.current?.abort()}>Stop</CarbonButton>
                      : <CarbonButton type="submit" kind="primary" size="md" renderIcon={SendFilled} disabled={keyBusy}>Send</CarbonButton>}
                  </div>
                </form>
              </section>
            )}
          </div>
      </div>

      <CodeDialog
        open={codeOpen}
        onClose={() => setCodeOpen(false)}
        alias={primary.alias}
        request={codeRequest(primaryRequest)}
        stream={stream}
        heldKey={key?.secret ?? null}
        embeddingInputs={embeddingModel ? embeddingInputs(embedText).inputs.slice(0, MAX_EMBEDDING_INPUTS) : null}
      />
    </div>
  );
}

/** A square Carbon icon button, named for assistive technology. */
function IconButton({ label, children, onClick, pressed, type = 'button', disabled }: {
  label: string;
  children: React.ReactNode;
  onClick?: () => void;
  pressed?: boolean;
  type?: 'button' | 'submit';
  disabled?: boolean;
}) {
  return (
    <button
      type={type}
      className={`cds--btn cds--btn--ghost cds--btn--icon-only cds--btn--md playground-icon${pressed ? ' is-pressed' : ''}`}
      aria-label={label}
      title={label}
      aria-pressed={pressed}
      disabled={disabled}
      onClick={onClick}
    >
      {React.isValidElement(children) ? React.cloneElement(children as React.ReactElement<{ 'aria-hidden'?: boolean }>, { 'aria-hidden': true }) : children}
    </button>
  );
}

/** A collapsible group of the settings rail, in the manner of Carbon's
 * accordion: the heading's button opens it, a short status sits beside it. */
function Section({ id, title, open, onToggle, status, children }: {
  id: string;
  title: string;
  open: boolean;
  onToggle: () => void;
  status?: string;
  children: React.ReactNode;
}) {
  return (
    <section className="playground-section" aria-labelledby={`${id}-heading`}>
      <div className="playground-section-head">
        <h3 id={`${id}-heading`} className="playground-section-heading">
          <button type="button" className="playground-section-toggle" aria-expanded={open} aria-controls={`${id}-body`} onClick={onToggle}>
            {open ? <ChevronUp aria-hidden="true" /> : <ChevronDown aria-hidden="true" />}
            <span>{title}</span>
          </button>
        </h3>
        {status && <span className="playground-section-status">{status}</span>}
      </div>
      <div id={`${id}-body`} className="playground-section-body" hidden={!open}>{children}</div>
    </section>
  );
}

/** A catalog price per million tokens, in the price's currency. */
const perMillion = (minorPerToken: number, currency: string) => formatMoney(minorPerToken * 1_000_000, currency);

/** What the alias is: its model, where it runs, its output cap and price. */
function ModelFacts({ model, cap }: { model: PlaygroundModel | EmbeddingModel; cap?: number | null }) {
  const where = model.deployments > 1 ? `${model.deployments} deployments` : model.location ?? model.deploymentName;
  return (
    <dl className="playground-model-facts">
      <div><dt>Model</dt><dd>{model.model}</dd></div>
      <div><dt>Runs on</dt><dd>{where}</dd></div>
      {cap !== undefined && <div><dt>Output cap</dt><dd>{cap === null ? 'Not set' : `${cap} tokens`}</dd></div>}
      <div>
        <dt>Price per 1M tokens</dt>
        <dd>{model.price
          ? `${perMillion(model.price.input_minor_per_token, model.price.currency)} in${cap !== undefined ? ` · ${perMillion(model.price.output_minor_per_token, model.price.currency)} out` : ''}`
          : 'No price in the catalog'}
        </dd>
      </div>
    </dl>
  );
}

function LaneView({ lane, index, comparing, models, onAlias, onRemove, onRetry, onToolResults, onResume }: {
  lane: Lane;
  index: number;
  comparing: boolean;
  models: PlaygroundModel[];
  onAlias: (alias: string) => void;
  onRemove?: () => void;
  onRetry?: () => void;
  /** Answers the reply's tool calls, by call id, and continues. */
  onToolResults?: (results: Map<string, string>) => void;
  /** Sends the conversation on after tool results that did not go (it
   * failed, was stopped, or had no key). */
  onResume?: () => void;
}) {
  const end = useRef<HTMLDivElement>(null);
  useEffect(() => { end.current?.scrollIntoView?.({ block: 'nearest' }); }, [lane.entries.length, lane.partial, lane.pending, lane.partialCalls]);
  const waiting = lane.sending ? [] : unansweredCalls(lane.entries);
  const sendingOn = lane.sending && lane.pending === null;
  const empty = lane.entries.length === 0 && lane.pending === null && !lane.failure;
  return (
    <div className="playground-lane playground-reply" aria-label={comparing ? `Model ${index + 1}: ${lane.alias}` : undefined} role={comparing ? 'group' : undefined}>
      {comparing && (
        <div className="playground-lane-head">
          <Select aria-label={`Model ${index + 1}`} value={lane.alias} onChange={event => onAlias(event.target.value)}>
            {models.map(candidate => <option key={candidate.alias} value={candidate.alias}>{candidate.alias}</option>)}
          </Select>
          {onRemove && <IconButton label={`Remove ${lane.alias} from the comparison`} onClick={onRemove}><Close /></IconButton>}
        </div>
      )}
      {empty ? (
        <div className="playground-empty">
          <p className="playground-empty-title">What do you want to chat about?</p>
          <p className="playground-help">Each reply shows its tokens, its cost at the catalog price, its latency and a link to its trace.</p>
        </div>
      ) : (
        <div className="playground-turns">
          {lane.entries.map((entry, at) => <Turn key={at} entry={entry} />)}
          {lane.pending !== null && <Turn entry={{ role: 'user', content: lane.pending }} />}
          {(lane.pending !== null || sendingOn) && (
            <div className="playground-turn playground-turn-assistant">
              <span className="playground-turn-role">Assistant</span>
              {(lane.partial || lane.partialCalls.length === 0) && <div className="playground-output">{lane.partial || '…'}</div>}
              {lane.partialCalls.length > 0 && <ToolCalls calls={lane.partialCalls} />}
            </div>
          )}
          {waiting.length > 0 && <ToolResults key={waiting.map(call => call.id).join(' ')} calls={waiting} onSubmit={onToolResults} />}
          {onResume && (
            <div><Button type="button" variant="ghost" onClick={onResume}>{lane.failure ? 'Retry' : 'Continue'}</Button></div>
          )}
          {lane.failedPrompt !== null && <Turn entry={{ role: 'user', content: lane.failedPrompt }} />}
          {lane.failure && <PlaygroundFailure failure={lane.failure} />}
          {lane.failedPrompt !== null && onRetry && (
            <div><Button type="button" variant="ghost" onClick={onRetry}>Retry</Button></div>
          )}
          <div ref={end} />
        </div>
      )}
      <p className="sr-only" role="status">{lane.sending ? `Waiting for ${lane.alias}.` : ''}</p>
    </div>
  );
}

const roleLabels: Record<string, string> = { user: 'You', assistant: 'Assistant', developer: 'Developer', system: 'System', tool: 'Tool result' };

/** A JSON text indented, when it parses; else as it is. */
function prettyJson(text: string): string {
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return text;
  }
}

/** The calls of an assistant turn: each function's name and arguments. */
function ToolCalls({ calls }: { calls: ToolCall[] }) {
  return (
    <ul className="playground-tool-calls" aria-label="Tool calls">
      {calls.map((call, at) => (
        <li key={call.id || at} className="playground-tool-call">
          <div className="playground-tool-call-head">
            <span className="playground-meta-label">Calls</span> <code>{call.function.name || '…'}</code>
            {call.id && <span className="playground-help"> · {call.id}</span>}
          </div>
          <pre className="playground-tool-arguments"><code>{prettyJson(call.function.arguments) || '{}'}</code></pre>
        </li>
      ))}
    </ul>
  );
}

/** What the person's code would answer each call with, to continue. */
function ToolResults({ calls, onSubmit }: { calls: ToolCall[]; onSubmit?: (results: Map<string, string>) => void }) {
  const [results, setResults] = useState<Record<string, string>>({});
  const id = React.useId();
  return (
    <form
      className="playground-tool-results"
      noValidate
      onSubmit={event => {
        event.preventDefault();
        onSubmit?.(new Map(calls.map(call => [call.id, results[call.id] ?? ''])));
      }}
    >
      <p className="playground-help">The model is waiting for these results. Run each call yourself and enter what it returned, often JSON.</p>
      {calls.map((call, at) => (
        <FormField key={call.id} label={`Result of ${call.function.name}`} htmlFor={`${id}-${at}`}>
          <TextArea
            id={`${id}-${at}`}
            rows={3}
            className="playground-code-input"
            placeholder='{"temperature": 21}'
            value={results[call.id] ?? ''}
            maxLength={131072}
            onChange={event => setResults(current => ({ ...current, [call.id]: event.target.value }))}
          />
        </FormField>
      ))}
      <div><Button type="submit" disabled={!onSubmit}>{calls.length === 1 ? 'Send result' : 'Send results'}</Button></div>
    </form>
  );
}

/** The Tools section: the functions the model may call (D-032), whether
 * it must call one, and whether a reply may make several calls. */
function ToolsEditor({ tools, onTools, toolChoice, onToolChoice, parallel, onParallel, error, unsupported }: {
  tools: ToolDraft[];
  onTools: (tools: ToolDraft[]) => void;
  toolChoice: string;
  onToolChoice: (choice: string) => void;
  parallel: boolean;
  onParallel: (parallel: boolean) => void;
  error?: string;
  /** The aliases on screen whose deployments do not all take tools. */
  unsupported: string[];
}) {
  const { problems } = buildTools(tools);
  const set = (key: number, change: Partial<ToolDraft>) => onTools(tools.map(tool => (tool.key === key ? { ...tool, ...change } : tool)));
  const names = tools.map(tool => tool.name.trim()).filter(name => TOOL_NAME.test(name));
  return (
    <>
      <p className="playground-help">Functions the model may call. A reply that calls one waits for you to enter its result.</p>
      {tools.length > 0 && unsupported.length > 0 && (
        <Message kind="warning" title="Not every model takes tools">
          {`${unsupported.join(', ')} ${unsupported.length === 1 ? 'refuses' : 'refuse'} requests with tools; the catalog marks which deployments take them.`}
        </Message>
      )}
      {tools.map((tool, at) => (
        <fieldset key={tool.key} className="playground-tool">
          <legend className="cds--label">{`Tool ${at + 1}`}</legend>
          <FormField label="Name" htmlFor={`playground-tool-${tool.key}-name`} error={problems.get(tool.key)}>
            <TextInput id={`playground-tool-${tool.key}-name`} value={tool.name} maxLength={64} placeholder="get_weather" onChange={event => set(tool.key, { name: event.target.value })} />
          </FormField>
          <FormField label="Description" htmlFor={`playground-tool-${tool.key}-description`} hint="What it does and when to call it.">
            <TextArea id={`playground-tool-${tool.key}-description`} rows={2} value={tool.description} onChange={event => set(tool.key, { description: event.target.value })} />
          </FormField>
          <FormField label="Parameters (JSON Schema)" htmlFor={`playground-tool-${tool.key}-parameters`} hint="An object schema; blank for none.">
            <TextArea
              id={`playground-tool-${tool.key}-parameters`}
              className="playground-code-input"
              rows={7}
              spellCheck={false}
              value={tool.parameters}
              onChange={event => set(tool.key, { parameters: event.target.value })}
            />
          </FormField>
          <Button type="button" variant="ghost" onClick={() => onTools(tools.filter(other => other.key !== tool.key))}>{`Remove tool ${at + 1}`}</Button>
        </fieldset>
      ))}
      <div>
        <Button
          type="button"
          variant="ghost"
          disabled={tools.length >= MAX_TOOLS}
          onClick={() => onTools([...tools, tools.length === 0
            ? { key: ++toolKeys, name: 'get_weather', description: 'The current weather in a city.', parameters: EXAMPLE_PARAMETERS }
            : { key: ++toolKeys, name: '', description: '', parameters: '{\n  "type": "object",\n  "properties": {}\n}' }])}
        >
          Add tool
        </Button>
      </div>
      {tools.length > 0 && (
        <>
          <Select id="playground-tool-choice" field={{ label: 'Tool choice' }} value={toolChoice} onChange={event => onToolChoice(event.target.value)}>
            <option value="auto">The model decides</option>
            <option value="required">Call at least one tool</option>
            <option value="none">Call no tool</option>
            {names.map(name => <option key={name} value={`function:${name}`}>{`Call ${name}`}</option>)}
            {toolChoice.startsWith('function:') && !names.includes(toolChoice.slice('function:'.length)) && (
              <option value={toolChoice}>{`Call ${toolChoice.slice('function:'.length)} (no such tool)`}</option>
            )}
          </Select>
          <CheckOption id="playground-parallel" checked={parallel} onChange={event => onParallel(event.target.checked)}>
            Allow several calls in one reply
          </CheckOption>
        </>
      )}
      {error && <div className="field-error"><span role="alert">{error}</span></div>}
    </>
  );
}

function Turn({ entry }: { entry: Entry }) {
  const reply = entry.reply;
  const cost = reply ? estimateCost(reply.usage, reply.price) : null;
  const money = cost !== null && reply?.price ? formatMoney(Math.round(cost), reply.price.currency) : null;
  const tracesLink = reply ? `/admin/traces${writeView('', 'traces', { ...emptyView, search: reply.requestId ?? reply.alias })}` : '';
  return (
    <div className={`playground-turn playground-turn-${entry.role}`}>
      <span className="playground-turn-role">
        {roleLabels[entry.role] ?? entry.role}
        {entry.role === 'tool' && entry.tool_call_id && <span className="playground-help"> · {entry.tool_call_id}</span>}
      </span>
      {(entry.content || !entry.tool_calls?.length) && <div className="playground-output">{entry.content}</div>}
      {entry.tool_calls?.length ? <ToolCalls calls={entry.tool_calls} /> : null}
      {reply?.refusal && <Message kind="warning" title="The model refused">{reply.refusal}</Message>}
      {reply && (
        <div className="playground-meta">
          <ul className="playground-meta-list" aria-label="About this reply">
            <li><span className="playground-meta-label">Tokens</span> {reply.usage ? `${reply.usage.input_tokens} in · ${reply.usage.output_tokens} out` : 'No usage'}</li>
            <li><span className="playground-meta-label">Cost</span> {money ?? '—'}</li>
            <li><span className="playground-meta-label">Latency</span> {Math.round(reply.totalMs)} ms</li>
            <li><AppLink to={tracesLink} className="cds--link">View trace</AppLink></li>
          </ul>
          <details className="playground-facts-box">
            <summary>Details</summary>
            <dl className="playground-facts">
              <div><dt>Finish reason</dt><dd>{reply.finishReason ?? '—'}</dd></div>
              <div><dt>Tokens</dt><dd>{reply.usage ? `${reply.usage.input_tokens} in · ${reply.usage.output_tokens} out${reply.usage.cached_input_tokens ? ` · ${reply.usage.cached_input_tokens} cached` : ''}${reply.usage.reasoning_tokens ? ` · ${reply.usage.reasoning_tokens} reasoning` : ''}` : '—'}</dd></div>
              <div><dt>Cost at catalog price</dt><dd>{money ?? '—'}</dd></div>
              <div><dt>Latency</dt><dd>{reply.firstTokenMs !== null ? `${Math.round(reply.firstTokenMs)} ms to first token · ` : ''}{Math.round(reply.totalMs)} ms total</dd></div>
              <div>
                <dt>Request id</dt>
                <dd>{reply.requestId ? <><code>{reply.requestId}</code> <CopyButton text={reply.requestId} label="Copy request id" /></> : '—'}</dd>
              </div>
            </dl>
            <p className="playground-help">The ledger settles the actual charge.</p>
          </details>
        </div>
      )}
    </div>
  );
}


/** This page's replies at a glance, in the conversation's corner: how many,
 * their tokens and what they cost at the catalog price; with links to every
 * application's requests to the alias. */
function SessionStats({ alias, lanes }: { alias: string; lanes: Lane[] }) {
  const replies = lanes.flatMap(lane => lane.entries.flatMap(entry => (entry.reply ? [entry.reply] : [])));
  const totals = new Map<string, number>();
  let input = 0;
  let output = 0;
  for (const reply of replies) {
    input += reply.usage?.input_tokens ?? 0;
    output += reply.usage?.output_tokens ?? 0;
    const cost = estimateCost(reply.usage, reply.price);
    if (cost !== null && reply.price) totals.set(reply.price.currency, (totals.get(reply.price.currency) ?? 0) + cost);
  }
  return (
    <div className="playground-stats" role="group" aria-label="This session">
      <dl>
        <div><dt>Replies</dt><dd>{replies.length}</dd></div>
        <div><dt>Tokens</dt><dd>{input} in · {output} out</dd></div>
        <div><dt>Cost</dt><dd title="At the catalog price">{totals.size ? [...totals].map(([currency, amount]) => formatMoney(Math.round(amount), currency)).join(' · ') : '—'}</dd></div>
      </dl>
      <AliasLinks alias={alias} />
    </div>
  );
}

/** Every application's requests to `alias`: its usage and its traces. */
function AliasLinks({ alias }: { alias: string }) {
  const usageLink = `/admin/usage${writeDashboard('', { ...readDashboard(''), split: 'model', filters: { alias: [alias] } })}`;
  const tracesLink = `/admin/traces${writeView('', 'traces', { ...emptyView, search: alias })}`;
  return <p><AppLink to={usageLink} label={`Usage of ${alias}`}>Usage</AppLink> · <AppLink to={tracesLink} label={`Traces of ${alias}`}>Traces</AppLink></p>;
}

/** *Continue in code*: the gateway's endpoint, the key the page holds, and
 * the code that sends the conversation on screen. The code never holds the
 * key: it reads `GATEWAY_API_KEY` (D-025). */
function CodeDialog({ open, onClose, alias, request, stream, heldKey, embeddingInputs: inputs }: {
  open: boolean;
  onClose: () => void;
  alias: string;
  request: PlaygroundRequest;
  stream: boolean;
  heldKey: string | null;
  /** The texts to embed, in embeddings mode; null for chat. */
  embeddingInputs: string[] | null;
}) {
  const [language, setLanguage] = useState<Language>('python');
  const [style, setStyle] = useState<CodeStyle>('client');
  const [revealed, setRevealed] = useState(false);
  useEffect(() => { if (!open) setRevealed(false); }, [open]);
  // A language without a client says why its code is raw HTTP.
  const embedding = inputs !== null;
  const shownLanguages = embedding ? embeddingLanguages : languages;
  // Embeddings code exists in fewer languages: another one shows as Python.
  const shownLanguage: Language = shownLanguages.some(([value]) => value === language) ? language : 'python';
  const client = embedding ? shownLanguage === 'python' : hasClient(shownLanguage);
  const code = useMemo(
    () => (inputs !== null
      ? embeddingSnippet(shownLanguage, client ? style : 'client', alias, inputs, location.origin)
      : snippet(shownLanguage, client ? style : 'client', request, { baseUrl: location.origin, stream })),
    [shownLanguage, client, style, request, stream, inputs, alias],
  );
  const endpoint = `${location.origin}${embedding ? EMBEDDINGS_PATH : stream ? STREAM_PATH : CHAT_PATH}`;
  return (
    <Dialog open={open} onClose={onClose} title={`Call ${alias} in code`} size="lg">
      <div className="playground-code-dialog">
        <div className="playground-code-top">
          <p className="muted">{embedding ? 'Embed these texts with the model from your own code.' : 'Send this conversation to the model from your own code.'}</p>
          <IconButton label="Close" onClick={onClose}><Close /></IconButton>
        </div>
        <div className="playground-code-fields">
          <FormField label="Endpoint" htmlFor="playground-endpoint">
            <div className="playground-field-row">
              <TextInput id="playground-endpoint" readOnly value={endpoint} />
              <CopyButton text={endpoint} label="Copy endpoint" />
            </div>
          </FormField>
          <FormField label="API key" htmlFor="playground-code-key" hint={heldKey ? 'The key this page holds. The code reads it from GATEWAY_API_KEY.' : 'Choose a key on the page, or set GATEWAY_API_KEY to one of your own.'}>
            <div className="playground-field-row">
              {heldKey ? (
                <>
                  <TextInput id="playground-code-key" readOnly type={revealed ? 'text' : 'password'} autoComplete="off" spellCheck={false} value={heldKey} />
                  <IconButton label={revealed ? 'Hide key' : 'Show key'} pressed={revealed} onClick={() => setRevealed(current => !current)}>
                    {revealed ? <ViewOff /> : <View />}
                  </IconButton>
                  <CopyButton text={heldKey} label="Copy API key" />
                </>
              ) : (
                <TextInput id="playground-code-key" readOnly value="GATEWAY_API_KEY" />
              )}
            </div>
          </FormField>
        </div>
        <h3 className="playground-code-heading">Call this model</h3>
        <div className="playground-code-block">
          <div className="playground-code-toolbar">
            <label className="playground-inline-select">
              <span>Language</span>
              <Select aria-label="Language" value={shownLanguage} onChange={event => setLanguage(event.target.value as Language)}>
                {shownLanguages.map(([value, label]) => <option key={value} value={value}>{label}</option>)}
              </Select>
            </label>
            <label className="playground-inline-select">
              <span>Client</span>
              <Select aria-label="Client" value={client ? style : 'raw'} disabled={!client} onChange={event => setStyle(event.target.value as CodeStyle)}>
                <option value="client">Gateway client</option>
                <option value="raw">Raw HTTP</option>
              </Select>
            </label>
            <CopyButton text={code.code} label="Copy code" />
          </div>
          {code.note && <Message kind="info">{code.note}</Message>}
          <div className="playground-setup">
            <pre aria-label="Setup"><code>{code.setup}</code></pre>
            <CopyButton text={code.setup} label="Copy setup" />
          </div>
          <div className="playground-snippet">
            <pre aria-label={`${shownLanguages.find(([value]) => value === shownLanguage)?.[1]} ${code.style === 'client' ? 'client' : 'raw HTTP'} code`}>
              <HighlightedCode code={code.code} syntax={code.syntax} />
            </pre>
          </div>
        </div>
      </div>
    </Dialog>
  );
}

function PlaygroundFailure({ failure, scope = 'Inference' }: { failure: InferenceError; scope?: 'Inference' | 'Embeddings' }) {
  const what = failure.status === 401
    ? 'The key was refused. It may be revoked, expired, or for another gateway.'
    : failure.status === 403
      ? `The key cannot call this model. It needs the ${scope} scope and access to this alias.`
      : failure.status === 429
        ? 'A rate limit or budget stopped this request. Wait, or check Policies.'
        : failure.status === 0
          ? failure.detail
          : `The request failed: ${failure.detail}`;
  return (
    <Message kind="error" title={what}>
      {failure.code !== 'request_failed' && <span className="problem-ref">Error code: <code>{failure.code}</code></span>}
      {failure.requestId && <> Request id <code>{failure.requestId}</code></>}
    </Message>
  );
}

/** A model picker value: the operation the alias is used for, and the alias. */
const pickerValue = (operation: 'chat' | 'embeddings', alias: string) => `${operation}:${alias}`;
const pickedModel = (value: string): { operation: 'chat' | 'embeddings'; alias: string } => {
  const at = value.indexOf(':');
  return { operation: value.slice(0, at) === 'embeddings' ? 'embeddings' : 'chat', alias: value.slice(at + 1) };
};

/** Shown while the page holds no key: sending creates one. */
function NoKeyYet({ onNeedKey }: { onNeedKey: () => void }) {
  return (
    <p className="muted playground-need-key">
      Sending creates a temporary key for an hour on your own service account.{' '}
      <button type="button" className="inline-action link-button" onClick={onNeedKey}>Use another key</button>
    </p>
  );
}

/** Embeddings mode's right pane: texts in, one vector per line out, with how
 * alike the texts are (cosine similarity) when there are a few. */
function EmbeddingsPane({ model, text, onText, keyForSending, hasKey, keyBusy, onNeedKey }: {
  model: EmbeddingModel;
  text: string;
  onText: (text: string) => void;
  /** The secret to send with, created if the page holds none. */
  keyForSending: () => Promise<string | null>;
  hasKey: boolean;
  keyBusy: boolean;
  onNeedKey: () => void;
}) {
  const [run, setRun] = useState<EmbeddingRun | null>(null);
  const [failure, setFailure] = useState<InferenceError | null>(null);
  const [busy, setBusy] = useState(false);
  const [touched, setTouched] = useState(false);
  const abort = useRef<AbortController | null>(null);
  useEffect(() => () => abort.current?.abort(), []);
  const { inputs, error } = embeddingInputs(text);

  const embed = async () => {
    setTouched(true);
    // `abort.current` claims the pane at once: a second submit, even before
    // `busy` shows, is ignored, and leaving the pane aborts a send still
    // waiting for its key.
    if (error || busy || abort.current) return;
    const controller = new AbortController();
    abort.current = controller;
    setBusy(true);
    setFailure(null);
    try {
      const secret = await keyForSending();
      if (!secret || controller.signal.aborted) return;
      const result = await sendEmbeddings(secret, model.alias, inputs, { signal: controller.signal });
      setRun({ ...result, alias: model.alias, inputs, price: model.price });
      setTouched(false);
    } catch (caught) {
      if (caught instanceof DOMException && caught.name === 'AbortError') return;
      setFailure(caught instanceof InferenceError
        ? caught
        : new InferenceError(0, 'request_failed', caught instanceof Error ? caught.message : 'The request failed.', null));
    } finally {
      if (abort.current === controller) abort.current = null;
      setBusy(false);
    }
  };

  const cost = run ? estimateCost(run.usage, run.price) : null;
  const dimensions = run?.vectors[0]?.length ?? 0;
  const similar = run && run.vectors.length >= 2 && run.vectors.length <= 8;
  return (
    <section className="playground-stage" aria-label="Embeddings" aria-busy={busy}>
      <div className="playground-stage-bar">
        <div className="playground-stage-title"><h3>Embeddings</h3></div>
        <div className="playground-stats" role="group" aria-label={`Requests to ${model.alias}`}><AliasLinks alias={model.alias} /></div>
      </div>
      <div className="playground-lanes">
        <div className="playground-lane playground-reply">
          {!run && !failure ? (
            <div className="playground-empty">
              <p className="playground-empty-title">What do you want to embed?</p>
              <p className="playground-help">Write one text per line. A few texts also show how alike they are.</p>
            </div>
          ) : (
            <div className="playground-turns">
              {failure && <PlaygroundFailure failure={failure} scope="Embeddings" />}
              {run && (
                <div className="playground-embeddings">
                  <p className="playground-embeddings-summary">
                    <strong>{run.alias}</strong>: {run.vectors.length} {run.vectors.length === 1 ? 'vector' : 'vectors'} of {dimensions} dimensions
                    {run.usage ? ` · ${run.usage.input_tokens} tokens` : ' · no usage'}
                    {cost !== null && run.price ? ` · ${formatMoney(Math.round(cost), run.price.currency)}` : ''}
                    {` · ${Math.round(run.totalMs)} ms`}
                    {run.requestId && <> · <code>{run.requestId}</code> <CopyButton text={run.requestId} label="Copy request id" /></>}
                  </p>
                  <div className="table-wrap">
                    <table className="cds--data-table cds--data-table--sm playground-embeddings-table">
                      <thead><tr><th scope="col">#</th><th scope="col">Text</th><th scope="col">Vector</th><th scope="col"><span className="sr-only">Copy</span></th></tr></thead>
                      <tbody>
                        {run.inputs.map((input, index) => {
                          const vector = run.vectors[index] ?? [];
                          return (
                            <tr key={index}>
                              <td>{index + 1}</td>
                              <td className="playground-embeddings-text">{input}</td>
                              <td><code>[{vector.slice(0, 4).map(value => value.toFixed(4)).join(', ')}{vector.length > 4 ? ', …' : ''}]</code></td>
                              <td><CopyButton text={JSON.stringify(vector)} label={`Copy vector ${index + 1}`} /></td>
                            </tr>
                          );
                        })}
                      </tbody>
                    </table>
                  </div>
                  {similar && (
                    <>
                      <h3>Similarity</h3>
                      <p className="muted">Cosine similarity of each pair: 1 is the same direction, 0 unrelated.</p>
                      <div className="table-wrap">
                        <table className="cds--data-table cds--data-table--sm playground-similarity" aria-label="Cosine similarity">
                          <thead><tr><th scope="col"><span className="sr-only">Text</span></th>{run.inputs.map((_, index) => <th key={index} scope="col">{index + 1}</th>)}</tr></thead>
                          <tbody>
                            {run.vectors.map((row, at) => (
                              <tr key={at}>
                                <th scope="row">{at + 1}</th>
                                {run.vectors.map((column, index) => {
                                  const value = cosine(row, column);
                                  return <td key={index}>{value === null ? '—' : value.toFixed(3)}</td>;
                                })}
                              </tr>
                            ))}
                          </tbody>
                        </table>
                      </div>
                    </>
                  )}
                  <p className="muted">The ledger settles the actual charge.</p>
                </div>
              )}
            </div>
          )}
          <p className="sr-only" role="status">{busy ? `Waiting for ${model.alias}.` : ''}</p>
        </div>
      </div>
      <form className="playground-composer" noValidate onSubmit={event => { event.preventDefault(); void embed(); }}>
        <TextArea
          id="playground-embed-input"
          aria-label="Texts to embed, one per line"
          placeholder="One text per line…"
          rows={4}
          value={text}
          maxLength={1048576}
          onChange={event => onText(event.target.value)}
        />
        {touched && error && <div className="field-error"><span role="alert">{error}</span></div>}
        <div className="playground-composer-bar">
          <div className="playground-composer-notes">
            <p className="playground-help">Each line is one input, sent in a single request.</p>
            {!hasKey && <NoKeyYet onNeedKey={onNeedKey} />}
          </div>
          {busy
            ? <CarbonButton type="button" kind="secondary" size="md" renderIcon={StopFilled} onClick={() => abort.current?.abort()}>Stop</CarbonButton>
            : <CarbonButton type="submit" kind="primary" size="md" renderIcon={SendFilled} disabled={keyBusy}>Embed</CarbonButton>}
        </div>
      </form>
    </section>
  );
}
