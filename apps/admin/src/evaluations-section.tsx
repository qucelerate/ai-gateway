import React, { useEffect, useRef, useState } from 'react';
import {
  api,
  newIdempotencyKey,
  type AdminBatch,
  type ApiProblem,
  type EvaluationItem,
  type EvaluationMatch,
  type EvaluationRun,
  type EvaluationSet,
  type EvaluationSetInput,
} from './api';
import { formatCount } from './money';
import { ActionError, ReportError, loadFailed, type SignIn } from './screen-feedback';
import { useCollection } from './use-collection';
import { DataTable, type Column } from './data-table';
import {
  Badge,
  Button,
  ConfirmDialog,
  DataCell,
  DataGrid,
  Dialog,
  EmptyState,
  FormField,
  Message,
  RadioOption,
  Select,
  TableBody,
  TableHead,
  TableHeader,
  TableRow,
  TextArea,
  TextInput,
  formatDate,
} from './ui';

// Models → Evaluations (Phase 8, the architecture docs (https://gateway.qucelerate.com/architecture/)): the
// organization's own evaluation sets. A set is a list of questions' ids with
// the expected answers; the organization asks them in an ordinary chat batch
// (custom_id ITEM[:anything]) and records a run from the completed batch,
// which keeps only each model's counts. Sets never change; deleting one is
// soft (its runs and scores are kept, and it leaves the model cards).

type Props = SignIn & { organizationId: string; canChange: boolean };

const problemOf = (caught: unknown, fallback: string): ApiProblem => (caught as { problem?: ApiProblem }).problem ?? loadFailed(fallback);

// The server's limits (src/domain/model_evaluation.rs).
export const MAX_ITEMS = 1000;
const MAX_NAME_CHARS = 120;
const MAX_DESCRIPTION_CHARS = 1000;
const MAX_EXPECTED_CHARS = 1000;
const ITEM_ID = /^[A-Za-z0-9._-]{1,64}$/;
/** The endpoint of a chat batch, the only kind a run scores. */
export const CHAT_BATCH_ENDPOINT = '/v1/chat/completions';
/** Completed batches listed per page in Record a run. */
const BATCH_PAGE_SIZE = 100;
/** A batch id as the inference API gives it: `batch_<uuid>`. */
const BATCH_ID = /^batch_[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** Unicode code points, as the server counts characters. */
const chars = (text: string) => Array.from(text).length;
/** Control characters other than a newline (Unicode Cc, as Rust's is_control). */
const hasControl = (text: string) => /[\u0000-\u0009\u000b-\u001f\u007f-\u009f]/.test(text);
const blank = (text: string) => text.split(/\s+/).join('') === '';

export type SetValues = { name: string; description: string; items: EvaluationItem[] };
export type SetErrors = Partial<Record<'name' | 'description' | 'items', string>>;

/** The first problem with the items, as the server words it, or null. */
export function itemsProblem(items: EvaluationItem[]): string | null {
  if (items.length === 0 || items.length > MAX_ITEMS) return `A set has 1 to ${formatCount(MAX_ITEMS)} items.`;
  const seen = new Set<string>();
  for (const [index, item] of items.entries()) {
    const at = `Item ${index + 1}`;
    if (!ITEM_ID.test(item.id)) return `${at}: an id is 1 to 64 letters, digits, '-', '_' or '.'.`;
    if (seen.has(item.id)) return `${at}: the id ${item.id} is used twice; ids must be unique.`;
    seen.add(item.id);
    if (blank(item.expected) || chars(item.expected) > MAX_EXPECTED_CHARS || hasControl(item.expected)) {
      return `${at}: an expected answer is 1 to ${formatCount(MAX_EXPECTED_CHARS)} characters.`;
    }
    if (item.match !== 'exact' && item.match !== 'contains') return `${at}: match is exact or contains.`;
  }
  return null;
}

/** The set to create, checked as the server checks it. */
export function setInput(values: SetValues): { input: EvaluationSetInput } | { errors: SetErrors } {
  const errors: SetErrors = {};
  const name = values.name.trim();
  const description = values.description.trim();
  if (!name || chars(name) > MAX_NAME_CHARS || name.includes('\n') || hasControl(name)) errors.name = 'Enter a name of 1 to 120 characters on one line.';
  if (chars(description) > MAX_DESCRIPTION_CHARS || hasControl(description)) errors.description = 'A description holds at most 1,000 characters.';
  const items = itemsProblem(values.items);
  if (items) errors.items = items;
  if (Object.keys(errors).length) return { errors };
  return { input: { name, ...(description ? { description } : {}), items: values.items } };
}

/** Items pasted as JSON: a list of {id, expected, match}, or an object
 * holding one as `items` (with an optional name and description). */
export function parseItems(text: string): { items: EvaluationItem[]; name?: string; description?: string } | { error: string } {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    return { error: 'This is not JSON. Paste a list such as [{"id": "q1", "expected": "Paris", "match": "exact"}].' };
  }
  let name: string | undefined;
  let description: string | undefined;
  if (value && typeof value === 'object' && !Array.isArray(value)) {
    const object = value as Record<string, unknown>;
    if (typeof object.name === 'string') name = object.name;
    if (typeof object.description === 'string') description = object.description;
    value = object.items;
  }
  if (!Array.isArray(value)) return { error: 'Paste a JSON list of items, or an object with an "items" list.' };
  const items: EvaluationItem[] = [];
  for (const [index, entry] of value.entries()) {
    const at = `Item ${index + 1}`;
    if (!entry || typeof entry !== 'object' || Array.isArray(entry)) return { error: `${at} is not an object.` };
    const { id, expected, match, ...rest } = entry as Record<string, unknown>;
    const unknown = Object.keys(rest);
    if (unknown.length) return { error: `${at} has a field the gateway does not take: ${unknown[0]}. Items hold id, expected and match.` };
    if (typeof id !== 'string' || typeof expected !== 'string') return { error: `${at} needs an id and an expected answer, as text.` };
    if (match !== 'exact' && match !== 'contains') return { error: `${at}: match is "exact" or "contains".` };
    items.push({ id, expected, match });
  }
  return { items, ...(name !== undefined ? { name } : {}), ...(description !== undefined ? { description } : {}) };
}

export function percent(correct: number, items: number): string {
  return items > 0 ? `${Math.round((correct / items) * 100)}%` : '—';
}

const matchLabels: Record<EvaluationMatch, string> = { exact: 'Exact', contains: 'Contains' };

const unix = (seconds: number | null | undefined) => (seconds ? new Date(seconds * 1000).toISOString() : null);

/** How a completed batch is named when picking one to score. */
export function batchLabel(batch: AdminBatch): string {
  const name = batch.metadata?.name ?? batch.metadata?.description;
  return `${formatDate(unix(batch.completed_at ?? batch.created_at))} · ${formatCount(batch.request_counts.total)} lines${name ? ` · ${name}` : ''} · ${batch.id}`;
}

const setColumns: Column<EvaluationSet>[] = [
  {
    id: 'name',
    label: 'Name',
    primary: true,
    value: set => set.name,
    search: set => set.description ?? '',
    render: set => <>{set.name}{set.description && <><br /><small className="muted">{set.description}</small></>}</>,
  },
  { id: 'items', label: 'Items', filter: 'number', value: set => set.items.length, render: set => formatCount(set.items.length) },
  { id: 'created', label: 'Created', filter: 'date', value: set => set.created_at, render: set => formatDate(set.created_at) },
  { id: 'created_by', label: 'Created by', filter: 'category', value: set => set.created_by?.name ?? null, nullLabel: 'Unknown' },
];

export function EvaluationsSection({ organizationId, canChange, ...signIn }: Props) {
  const [creating, setCreating] = useState(false);
  const [open, setOpen] = useState<EvaluationSet | null>(null);
  const [deleting, setDeleting] = useState<EvaluationSet | null>(null);
  const [changed, setChanged] = useState<string | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  // One key per set being deleted: a retry replays the first attempt.
  const deleteKey = useRef<{ id: string; key: string } | null>(null);
  const sets = useCollection<EvaluationSet>(
    async () => ({ items: await api.listEvaluationSets(organizationId), next_page_token: null }),
    [organizationId],
  );
  const remove = async (set: EvaluationSet) => {
    if (deleteKey.current?.id !== set.id) deleteKey.current = { id: set.id, key: newIdempotencyKey() };
    setProblem(null);
    setSubmitting(true);
    try {
      await api.deleteEvaluationSet(organizationId, set, deleteKey.current.key);
      deleteKey.current = null;
      setDeleting(null);
      setOpen(null);
      setChanged(`Deleted ${set.name}. Its runs and scores are kept, and no longer shown on model cards.`);
      void sets.load(true);
    } catch (caught) {
      setProblem(problemOf(caught, 'The evaluation set could not be deleted.'));
    } finally {
      setSubmitting(false);
    }
  };
  const askDelete = (set: EvaluationSet) => {
    setProblem(null);
    setDeleting(set);
  };
  return (
    <>
      {changed && <Message kind="success">{changed}</Message>}
      <div className="section-header">
        <div>
          <h2>Evaluation sets</h2>
          <p className="muted">
            Your own questions with their expected answers. Ask them in a chat batch, one line per question and model, with each line’s <code>custom_id</code> set
            to the question’s id (optionally followed by <code>:</code> and anything, as in <code>q1:claude</code>), then record a run from the completed batch.
            Each model’s score shows on its card as this organization’s own: not a benchmark.
          </p>
        </div>
        {canChange && <span className="row-actions"><Button onClick={() => { setChanged(null); setCreating(true); }}>New evaluation set</Button></span>}
      </div>
      {sets.loading && <Message kind="info">Loading evaluation sets…</Message>}
      {sets.error && <ReportError problem={sets.error} what="evaluation sets" onRetry={() => void sets.load(true)} {...signIn} />}
      {!sets.loading && !sets.error && sets.items.length === 0 && (
        <EmptyState title="No evaluation sets" text={canChange ? 'Create a set of questions to score models with your own answers.' : 'A catalog writer can create sets of questions to score models with.'} />
      )}
      {!sets.loading && !sets.error && sets.items.length > 0 && (
        <DataTable
          label="Evaluation sets"
          urlKey="evaluation-sets"
          columns={setColumns}
          rows={sets.items}
          rowKey={set => set.id}
          actions={set => (
            <>
              <button className="inline-action" onClick={() => setOpen(set)} aria-label={`Open ${set.name}`}>Open</button>
              {canChange && <button className="inline-action" onClick={() => askDelete(set)} aria-label={`Delete ${set.name}`}>Delete</button>}
            </>
          )}
        />
      )}
      {creating && (
        <CreateSetDialog
          {...signIn}
          organizationId={organizationId}
          onClose={() => setCreating(false)}
          onCreated={set => {
            setCreating(false);
            setChanged(`Created ${set.name}, with ${formatCount(set.items.length)} items.`);
            void sets.load(true);
            setOpen(set);
          }}
        />
      )}
      {open && !deleting && (
        <SetDialog {...signIn} organizationId={organizationId} canChange={canChange} set={open} onClose={() => setOpen(null)} onDelete={() => askDelete(open)} />
      )}
      <ConfirmDialog
        open={deleting !== null}
        onClose={() => setDeleting(null)}
        onConfirm={() => { if (deleting) void remove(deleting); }}
        title="Delete evaluation set"
        confirmText="Delete"
        problem={problem}
        submitting={submitting}
      >
        {deleting?.name} leaves this list and the model cards, and takes no more runs. Its questions, runs and scores are kept in the gateway’s records, and the
        batches it scored are not touched.
      </ConfirmDialog>
    </>
  );
}

type Row = EvaluationItem & { key: number };

function CreateSetDialog({ organizationId, onClose, onCreated, ...signIn }: SignIn & {
  organizationId: string;
  onClose: () => void;
  onCreated: (set: EvaluationSet) => void;
}) {
  const [name, setName] = useState('');
  const [description, setDescription] = useState('');
  const [mode, setMode] = useState<'editor' | 'json'>('editor');
  const nextKey = useRef(1);
  const row = (item: EvaluationItem = { id: '', expected: '', match: 'exact' }): Row => ({ ...item, key: nextKey.current++ });
  const [rows, setRows] = useState<Row[]>(() => [row({ id: 'q1', expected: '', match: 'exact' })]);
  const [json, setJson] = useState('');
  const [errors, setErrors] = useState<SetErrors & { json?: string }>({});
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const attempt = useRef<{ body: string; key: string } | null>(null);
  const items = (): EvaluationItem[] => rows.map(({ id, expected, match }) => ({ id: id.trim(), expected, match }));
  const change = (key: number, update: Partial<EvaluationItem>) => setRows(current => current.map(item => (item.key === key ? { ...item, ...update } : item)));
  const toMode = (next: 'editor' | 'json') => {
    if (next === mode) return;
    if (next === 'json') {
      const filled = items().filter(item => item.id || item.expected);
      setJson(filled.length ? JSON.stringify(filled, null, 2) : '');
      setErrors({});
      setMode('json');
      return;
    }
    if (!json.trim()) {
      setMode('editor');
      return;
    }
    const parsed = parseItems(json);
    if ('error' in parsed) {
      setErrors({ json: `${parsed.error} Fix it to edit the items one by one.` });
      return;
    }
    setRows(parsed.items.length ? parsed.items.map(item => row(item)) : [row()]);
    if (parsed.name && !name) setName(parsed.name);
    if (parsed.description && !description) setDescription(parsed.description);
    setErrors({});
    setMode('editor');
  };
  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    let list: EvaluationItem[];
    let shownName = name;
    let shownDescription = description;
    if (mode === 'json') {
      const parsed = parseItems(json);
      if ('error' in parsed) {
        setErrors({ json: parsed.error });
        return;
      }
      list = parsed.items;
      if (parsed.name && !name.trim()) shownName = parsed.name;
      if (parsed.description && !description.trim()) shownDescription = parsed.description;
      setName(shownName);
      setDescription(shownDescription);
    } else {
      list = items();
    }
    const built = setInput({ name: shownName, description: shownDescription, items: list });
    if ('errors' in built) {
      setErrors(mode === 'json' && built.errors.items ? { ...built.errors, json: built.errors.items, items: undefined } : built.errors);
      return;
    }
    setErrors({});
    const body = JSON.stringify(built.input);
    if (attempt.current?.body !== body) attempt.current = { body, key: newIdempotencyKey() };
    setProblem(null);
    setSubmitting(true);
    try {
      onCreated(await api.createEvaluationSet(organizationId, built.input, attempt.current.key));
    } catch (caught) {
      setProblem(problemOf(caught, 'The evaluation set could not be created.'));
    } finally {
      setSubmitting(false);
    }
  };
  return (
    <Dialog open onClose={() => { if (!submitting) onClose(); }} title="New evaluation set" size="lg">
      <form onSubmit={submit}>
        <fieldset className="plain-fieldset dialog-body" disabled={submitting}>
          <FormField label="Name" htmlFor="evaluation-name" error={errors.name}>
            <TextInput id="evaluation-name" value={name} onChange={event => setName(event.target.value)} autoComplete="off" />
          </FormField>
          <FormField label="Description (optional)" htmlFor="evaluation-description" error={errors.description}>
            <TextInput id="evaluation-description" value={description} onChange={event => setDescription(event.target.value)} autoComplete="off" />
          </FormField>
          <fieldset className="plain-fieldset">
            <legend className="cds--label">Items</legend>
            <RadioOption id="evaluation-mode-editor" name="evaluation-mode" checked={mode === 'editor'} onChange={() => toMode('editor')}>One by one</RadioOption>
            <RadioOption id="evaluation-mode-json" name="evaluation-mode" checked={mode === 'json'} onChange={() => toMode('json')}>Paste JSON</RadioOption>
          </fieldset>
          <p className="meta">
            Each item has an id (letters, digits, <code>-</code>, <code>_</code> or <code>.</code>), the expected answer, and how an answer matches it:
            <em> exact</em> (the whole answer) or <em>contains</em>. Both compare after trimming, lowercasing and collapsing spaces. A set cannot change once
            created; at most {formatCount(MAX_ITEMS)} items.
          </p>
          {mode === 'editor' ? (
            <>
              {errors.items && <Message kind="error">{errors.items}</Message>}
              <DataGrid aria-label="Items" size="sm">
                <TableHead>
                  <TableRow className="data-row head">
                    <TableHeader>Id</TableHeader>
                    <TableHeader>Expected answer</TableHeader>
                    <TableHeader>Match</TableHeader>
                    <TableHeader className="row-actions">Action</TableHeader>
                  </TableRow>
                </TableHead>
                <TableBody>
                  {rows.map((item, index) => (
                    <TableRow className="data-row" key={item.key}>
                      <DataCell label="Id">
                        <TextInput aria-label={`Item ${index + 1} id`} value={item.id} onChange={event => change(item.key, { id: event.target.value })} autoComplete="off" />
                      </DataCell>
                      <DataCell label="Expected answer">
                        <TextInput aria-label={`Item ${index + 1} expected answer`} value={item.expected} onChange={event => change(item.key, { expected: event.target.value })} autoComplete="off" />
                      </DataCell>
                      <DataCell label="Match">
                        <Select aria-label={`Item ${index + 1} match`} value={item.match} onChange={event => change(item.key, { match: event.target.value as EvaluationMatch })}>
                          <option value="exact">Exact</option>
                          <option value="contains">Contains</option>
                        </Select>
                      </DataCell>
                      <DataCell label="Action">
                        <button type="button" className="inline-action" disabled={rows.length === 1} onClick={() => setRows(current => current.filter(other => other.key !== item.key))} aria-label={`Remove item ${index + 1}`}>Remove</button>
                      </DataCell>
                    </TableRow>
                  ))}
                </TableBody>
              </DataGrid>
              <Button
                variant="ghost"
                type="button"
                disabled={rows.length >= MAX_ITEMS}
                onClick={() => setRows(current => [...current, row({ id: `q${current.length + 1}`, expected: '', match: 'exact' })])}
              >
                Add item
              </Button>
            </>
          ) : (
            <FormField label="Items as JSON" htmlFor="evaluation-json" hint='A list such as [{"id": "q1", "expected": "Paris", "match": "exact"}], or an object with name, description and items.' error={errors.json}>
              <TextArea id="evaluation-json" rows={12} value={json} onChange={event => setJson(event.target.value)} spellCheck={false} />
            </FormField>
          )}
        </fieldset>
        <ActionError problem={problem} action="create the evaluation set" {...signIn} />
        <div className="dialog-actions">
          <Button variant="ghost" type="button" onClick={onClose} disabled={submitting}>Cancel</Button>
          <Button type="submit" loading={submitting}>Create set</Button>
        </div>
      </form>
    </Dialog>
  );
}

function SetDialog({ organizationId, canChange, set, onClose, onDelete, ...signIn }: Props & {
  set: EvaluationSet;
  onClose: () => void;
  onDelete: () => void;
}) {
  const [runs, setRuns] = useState<EvaluationRun[] | null>(null);
  const [runsProblem, setRunsProblem] = useState<ApiProblem | null>(null);
  const [attemptRuns, setAttemptRuns] = useState(0);
  useEffect(() => {
    let live = true;
    setRunsProblem(null);
    api.listEvaluationRuns(organizationId, set.id).then(
      value => { if (live) setRuns(value); },
      caught => { if (live) setRunsProblem(problemOf(caught, 'The runs could not be loaded.')); },
    );
    return () => { live = false; };
  }, [organizationId, set.id, attemptRuns]);
  return (
    <Dialog open onClose={onClose} title={set.name} size="lg">
      <div className="dialog-body">
        <dl className="detail-list">
          {set.description && <><dt>Description</dt><dd>{set.description}</dd></>}
          <dt>Items</dt><dd>{formatCount(set.items.length)}</dd>
          <dt>Created</dt><dd>{formatDate(set.created_at)}{set.created_by?.name && <span className="muted"> by {set.created_by.name}</span>}</dd>
          <dt>Id</dt><dd><code>{set.id}</code></dd>
        </dl>
        <h3>Runs</h3>
        {runsProblem && <ReportError problem={runsProblem} what="runs" onRetry={() => setAttemptRuns(value => value + 1)} {...signIn} />}
        {!runs && !runsProblem && <Message kind="info">Loading runs…</Message>}
        {runs && runs.length === 0 && <p className="muted">Not scored yet. Record a run from a completed chat batch that asked these questions.</p>}
        {runs && runs.length > 0 && <RunsTable runs={runs} />}
        {canChange && <RecordRun {...signIn} organizationId={organizationId} set={set} onRecorded={() => setAttemptRuns(value => value + 1)} />}
        <h3>Items</h3>
        <DataGrid aria-label="Items of the set" size="sm">
          <TableHead>
            <TableRow className="data-row head">
              <TableHeader>Id</TableHeader>
              <TableHeader>Expected answer</TableHeader>
              <TableHeader>Match</TableHeader>
            </TableRow>
          </TableHead>
          <TableBody>
            {set.items.map(item => (
              <TableRow className="data-row" key={item.id}>
                <DataCell label="Id" className="cell-primary"><code>{item.id}</code></DataCell>
                <DataCell label="Expected answer">{item.expected}</DataCell>
                <DataCell label="Match">{matchLabels[item.match] ?? item.match}</DataCell>
              </TableRow>
            ))}
          </TableBody>
        </DataGrid>
      </div>
      <div className="dialog-actions">
        {canChange && <Button variant="danger" onClick={onDelete}>Delete set</Button>}
        <Button variant="ghost" onClick={onClose}>Close</Button>
      </div>
    </Dialog>
  );
}

function RunsTable({ runs }: { runs: EvaluationRun[] }) {
  return (
    <DataGrid aria-label="Runs of the set" size="sm">
      <TableHead>
        <TableRow className="data-row head">
          <TableHeader>Scored</TableHeader>
          <TableHeader>Model</TableHeader>
          <TableHeader>Correct</TableHeader>
          <TableHeader>Answered</TableHeader>
          <TableHeader>Batch</TableHeader>
        </TableRow>
      </TableHead>
      <TableBody>
        {runs.flatMap(run => {
          const results = run.results.length ? run.results : [null];
          return results.map((result, index) => (
            <TableRow className="data-row" key={`${run.id}-${result?.model ?? 'none'}`}>
              <DataCell label="Scored">{index === 0 ? <>{formatDate(run.created_at)}{run.created_by?.name && <span className="muted"> by {run.created_by.name}</span>}</> : ''}</DataCell>
              <DataCell label="Model" className="cell-primary">{result ? result.model : <span className="muted">No line matched an item</span>}</DataCell>
              <DataCell label="Correct">{result ? <>{formatCount(result.correct)} of {formatCount(result.items)} <span className="muted">({percent(result.correct, result.items)})</span></> : '—'}</DataCell>
              <DataCell label="Answered">{result ? `${formatCount(result.answered)} of ${formatCount(result.items)}` : '—'}</DataCell>
              <DataCell label="Batch" title={run.batch_id}>
                {index === 0 ? <><code>batch_{run.batch_id}</code>{run.unmatched_lines > 0 && <> <Badge kind="warning" title="Output lines whose custom_id named no item of the set">{formatCount(run.unmatched_lines)} unmatched</Badge></>}</> : ''}
              </DataCell>
            </TableRow>
          ));
        })}
      </TableBody>
    </DataGrid>
  );
}

/** Picks one of the organization's completed chat batches and scores the set
 * from it. Reading batches needs `usage_read`: without it, a batch id is
 * typed instead. */
function RecordRun({ organizationId, set, onRecorded, ...signIn }: SignIn & { organizationId: string; set: EvaluationSet; onRecorded: () => void }) {
  const [batches, setBatches] = useState<AdminBatch[] | null>(null);
  // The next page of completed batches, or null once every page is listed.
  const [next, setNext] = useState<string | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [listProblem, setListProblem] = useState<ApiProblem | null>(null);
  const [moreProblem, setMoreProblem] = useState<ApiProblem | null>(null);
  const [picked, setPicked] = useState('');
  const [typed, setTyped] = useState('');
  const [error, setError] = useState<{ field: 'picked' | 'typed'; text: string } | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [recorded, setRecorded] = useState<string | null>(null);
  const attempt = useRef<{ batch: string; key: string } | null>(null);
  const live = useRef(true);
  useEffect(() => {
    live.current = true;
    // This request's own flag: a later run of the effect (another
    // organization, or StrictMode's second setup) sets `live` again, so a
    // late first page must not replace the pages loaded since.
    let current = true;
    api.listBatches(organizationId, 'completed', { page_size: BATCH_PAGE_SIZE }).then(
      page => {
        if (!current) return;
        const chat = page.items.filter(batch => batch.endpoint === CHAT_BATCH_ENDPOINT);
        setBatches(chat);
        setNext(page.next_page_token ?? null);
        setPicked(picked => picked || chat[0]?.id || '');
      },
      caught => { if (current) setListProblem(problemOf(caught, 'The batches could not be listed.')); },
    );
    return () => { current = false; live.current = false; };
  }, [organizationId]);
  const loadMore = async () => {
    if (!next) return;
    setLoadingMore(true);
    setMoreProblem(null);
    try {
      const page = await api.listBatches(organizationId, 'completed', { page_size: BATCH_PAGE_SIZE, page_token: next });
      if (!live.current) return;
      const chat = page.items.filter(batch => batch.endpoint === CHAT_BATCH_ENDPOINT);
      setBatches(current => {
        const seen = new Set((current ?? []).map(batch => batch.id));
        return [...(current ?? []), ...chat.filter(batch => !seen.has(batch.id))];
      });
      setNext(page.next_page_token ?? null);
      setPicked(current => current || chat[0]?.id || '');
    } catch (caught) {
      if (live.current) setMoreProblem(problemOf(caught, 'More batches could not be listed.'));
    } finally {
      if (live.current) setLoadingMore(false);
    }
  };
  // A typed id wins over the list, so a batch not listed can still be scored.
  const batchId = typed.trim() || picked;
  const record = async (event: React.FormEvent) => {
    event.preventDefault();
    if (typed.trim() && !BATCH_ID.test(typed.trim())) {
      setError({ field: 'typed', text: 'Enter the id of a completed chat batch, as in batch_…' });
      return;
    }
    if (!batchId) {
      setError(batches && batches.length > 0
        ? { field: 'picked', text: 'Choose a completed chat batch.' }
        : { field: 'typed', text: 'Enter the id of a completed chat batch, as in batch_…' });
      return;
    }
    setError(null);
    if (attempt.current?.batch !== batchId) attempt.current = { batch: batchId, key: newIdempotencyKey() };
    setProblem(null);
    setRecorded(null);
    setSubmitting(true);
    try {
      const run = await api.createEvaluationRun(organizationId, set.id, batchId, attempt.current.key);
      attempt.current = null;
      const models = run.results.length;
      setRecorded(models ? `Scored ${models === 1 ? 'one model' : `${formatCount(models)} models`} from the batch.` : 'Recorded, but no line of the batch named an item of this set.');
      onRecorded();
    } catch (caught) {
      setProblem(problemOf(caught, 'The run could not be recorded.'));
    } finally {
      setSubmitting(false);
    }
  };
  const listed = batches !== null && batches.length > 0;
  return (
    <form className="detail-panel" onSubmit={record}>
      <h4>Record a run</h4>
      <p className="meta">Scores each model the batch’s lines ran on. Only the counts are kept, never the answers. Score a batch before retention deletes its output.</p>
      <fieldset className="plain-fieldset" disabled={submitting}>
        {listProblem ? (
          <Message kind="info">The batches could not be listed{listProblem.code === 'forbidden' ? ': listing them needs permission to read usage' : ''}. Enter a batch id instead.</Message>
        ) : batches === null ? (
          <Message kind="info">Loading completed batches…</Message>
        ) : listed ? (
          <FormField label="Completed chat batch" htmlFor="evaluation-batch" hint="Most recent first." error={error?.field === 'picked' ? error.text : undefined}>
            <Select id="evaluation-batch" value={picked} onChange={event => setPicked(event.target.value)}>
              {batches.map(batch => <option key={batch.id} value={batch.id}>{batchLabel(batch)}</option>)}
            </Select>
          </FormField>
        ) : (
          <p className="muted">No completed chat batches{next ? ' on the first page' : ''}. Run the questions as a chat batch first; it shows here once it has completed.</p>
        )}
        {next && !listProblem && (
          <div className="dialog-actions">
            <Button variant="ghost" type="button" loading={loadingMore} onClick={() => void loadMore()}>Load more</Button>
          </div>
        )}
        {moreProblem && <ActionError problem={moreProblem} action="list more batches" {...signIn} />}
        <FormField label={listed ? 'Or enter a batch id' : 'Batch id'} htmlFor="evaluation-batch-id" hint={listed ? 'Used instead of the list when filled in.' : undefined} error={error?.field === 'typed' ? error.text : undefined}>
          <TextInput id="evaluation-batch-id" value={typed} onChange={event => setTyped(event.target.value)} placeholder="batch_…" autoComplete="off" />
        </FormField>
      </fieldset>
      {recorded && <Message kind="success">{recorded}</Message>}
      <ActionError problem={problem} action="record the run" {...signIn} />
      <div className="dialog-actions">
        <Button type="submit" loading={submitting}>Record run</Button>
      </div>
    </form>
  );
}
