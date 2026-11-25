import React, { useCallback, useEffect, useRef, useState } from 'react';
import { api, newIdempotencyKey, type ApiProblem, type CardOperation, type MasterDeployment, type MasterList, type MasterRecord, type Modality, type OrganizationMaster, type ProposalOperation, type RouteChange, type RouteProposal } from './api';
import { ChangeFeedback } from './catalog-actions';
import { useCatalogChange } from './catalog-change';
import { problemOf } from './connect-cloud';
import { cardPath } from './model-card';
import {
  changesExisting,
  defaultOperations,
  deploymentAliases,
  exclusionLabels,
  groupMasters,
  linkChoices,
  linkIfMatch,
  linkInUse,
  masterOperations,
  proposalEntries,
  proposalMatches,
  proposalProblem,
  recordVersion,
  routeAcrossProviders,
  routeChanges,
  slugProblem,
  suggestionGroups,
  type ProposalEntry,
} from './model-masters-model';
import { formatMoney } from './money';
import { ActionError, ReportError, type SignIn } from './screen-feedback';
import { AppLink, Badge, Button, CheckOption, ConfirmDialog, DataCell, DataGrid, Dialog, FormField, Message, RadioOption, Select, TableBody, TableHead, TableHeader, TableRow, TextInput, formatDate } from './ui';

// The Models page's view of one record per model across providers (D-033):
// each master record with its linked deployments side by side, the
// deployments with none, and the controls to change links, keep the
// organization's own records and route a model across providers.

type Props = SignIn & {
  organizationId: string;
  /** The active release the page shows: a new one reloads the records. */
  release: string;
  familyLabels: Record<string, string>;
  /** Whether to offer changes (the server decides). */
  canChange: boolean;
  /** A route was published: the page's release changed. */
  onReleaseChanged: (summary: string) => void;
};

const operationLabels: Record<string, string> = { chat: 'Chat', chat_stream: 'Streaming', embeddings: 'Embeddings', chat_batch: 'Chat batch', embeddings_batch: 'Embeddings batch' };
const operationLabel = (operation: string) => operationLabels[operation] ?? operation;
const modalityLabels: Record<Modality, string> = { chat: 'Chat', embeddings: 'Embeddings' };
const embeds = (operation: string) => operation === 'embeddings' || operation === 'embeddings_batch';
const availabilityLabels: Record<string, [string, 'success' | 'danger' | 'warning' | 'neutral']> = {
  verified: ['Verified', 'success'],
  entitled: ['Entitled', 'neutral'],
  refused: ['Refused', 'danger'],
  requires_action: ['Needs action', 'warning'],
};
const perMillion = (perToken: number, currency: string) => formatMoney(perToken * 1_000_000, currency);
const bytes = (value: number) => (value >= 1024 * 1024 ? `${(value / 1024 / 1024).toFixed(1)} MiB` : value >= 1024 ? `${Math.round(value / 1024)} KiB` : `${value} B`);
/** `chat_stream` of an older release is chat. */
const shown = (operations: CardOperation[]) => {
  const chat = operations.some(op => op.operation === 'chat');
  return operations.flatMap(op => (op.operation !== 'chat_stream' ? [op] : chat ? [] : [{ ...op, operation: 'chat' }]));
};

/** Messages for the codes master writes refuse with; others as the server says. */
function masterProblem(problem: ApiProblem | null): ApiProblem | null {
  if (!problem) return null;
  const detail: Record<string, string> = {
    master_in_use: 'Links still name this record. Change or clear those links first.',
    modality_mismatch: 'That record is for another modality than this deployment serves: a chat deployment links only to a chat model’s record, an embeddings deployment to an embedding model’s.',
    unknown_master: 'No card or organization record has that id any more. Reload and choose another.',
    deployment_not_found: 'The active release no longer serves this model on this provider. Reload to see the catalog now.',
  };
  return detail[problem.code] ? { ...problem, detail: detail[problem.code] } : problem;
}

export function ModelMasters({ organizationId, release, familyLabels, canChange, onReleaseChanged, ...signIn }: Props) {
  const [list, setList] = useState<MasterList | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [linking, setLinking] = useState<MasterDeployment | null>(null);
  const [creating, setCreating] = useState(false);
  const [deleting, setDeleting] = useState<MasterRecord | null>(null);
  const [routing, setRouting] = useState<{ master: MasterRecord; initial?: ProposalStart } | null>(null);
  const [changed, setChanged] = useState<string | null>(null);
  const latest = useRef(0);
  const load = useCallback(async () => {
    const sequence = ++latest.current;
    setProblem(null);
    try {
      const loaded = await api.modelMasters(organizationId);
      // A gateway without master records answers something else: say so, not crash.
      if (!Array.isArray(loaded?.items) || !Array.isArray(loaded?.unlinked) || !Array.isArray(loaded?.links)) throw new Error('not a master list');
      if (sequence === latest.current) setList({ ...loaded, deleted_records: Array.isArray(loaded.deleted_records) ? loaded.deleted_records : [] });
    } catch (caught) {
      if (sequence === latest.current) setProblem(problemOf(caught, 'The model records could not be loaded.'));
    }
  }, [organizationId]);
  useEffect(() => {
    setList(null);
    void load();
    return () => { latest.current += 1; };
  }, [load, release]);
  const family = (value: string) => familyLabels[value] ?? value;
  const onChanged = (summary: string) => {
    setLinking(null);
    setCreating(false);
    setDeleting(null);
    setChanged(summary);
    void load();
  };
  const groups = list ? groupMasters(list) : null;
  return (
    <section aria-labelledby="model-masters" className="model-masters">
      <div className="section-header">
        <div>
          <h2 id="model-masters">Models across providers</h2>
          <p className="muted">
            Each model once, with every deployment of it side by side. A deployment belongs to the card that lists its provider and model identifier exactly, unless the organization linked it elsewhere. Links are catalog metadata: they never change routing, which only a published release does.
          </p>
        </div>
        {canChange && <Button variant="ghost" type="button" onClick={() => setCreating(true)}>New organization record</Button>}
      </div>
      {changed && <Message kind="success">{changed}</Message>}
      {!list && !problem && <Message kind="info">Loading model records…</Message>}
      {problem && <ReportError problem={problem} what="model records" onRetry={() => void load()} {...signIn} />}
      {groups && (
        <>
          {groups.linked.length === 0 && <p className="muted">No deployment of the active release belongs to a model record yet.</p>}
          {groups.linked.map(master => (
            <div key={master.id} className="cds--tile detail-panel master-record" aria-label={master.name} role="group">
              <div className="detail-header">
                <div>
                  <h3>{master.name}</h3>
                  <p className="meta">
                    <code>{master.id}</code> · {master.kind === 'card' ? <>card{master.vendor ? ` from ${master.vendor}` : ''}</> : 'organization record'} · {modalityLabels[master.modality]} · {master.deployments.length} deployment{master.deployments.length === 1 ? '' : 's'}
                  </p>
                </div>
                <span className="row-actions">
                  {canChange && masterOperations(master).length > 0 && (
                    <Button variant="ghost" type="button" onClick={() => setRouting({ master })} aria-label={`Route ${master.name} across providers`}>Route across providers</Button>
                  )}
                  {canChange && master.kind === 'organization' && (
                    <button type="button" className="inline-action" onClick={() => setDeleting(master)} aria-label={`Delete ${master.name}`}>Delete</button>
                  )}
                </span>
              </div>
              <MasterDeployments label={`Deployments of ${master.name}`} deployments={master.deployments} family={family} canChange={canChange} onLink={setLinking} />
              <Suggestions master={master} canChange={canChange} onReview={initial => setRouting({ master, initial })} />
            </div>
          ))}
          <h3>Deployments with no model record</h3>
          {groups.unlinked.length === 0
            ? <p className="muted">Every deployment of the active release belongs to a model record.</p>
            : (
              <>
                <p className="muted">No card lists these, or the organization linked them to none. Link one to a record to see it beside the model’s other deployments and route it across providers.</p>
                <MasterDeployments label="Deployments with no model record" unlinked deployments={groups.unlinked} family={family} canChange={canChange} onLink={setLinking} />
              </>
            )}
          {groups.idleOrganization.length > 0 && (
            <>
              <h3>Organization records with no deployments</h3>
              <ul className="plain-list">
                {groups.idleOrganization.map(master => (
                  <li key={master.id}>
                    {master.name} <code>{master.id}</code> · {modalityLabels[master.modality]}
                    {canChange && <> <button type="button" className="inline-action" onClick={() => setDeleting(master)} aria-label={`Delete ${master.name}`}>Delete</button></>}
                  </li>
                ))}
              </ul>
            </>
          )}
        </>
      )}
      {list && linking && <LinkDialog {...signIn} organizationId={organizationId} deployment={linking} masters={list.items} onClose={() => setLinking(null)} onChanged={onChanged} />}
      {creating && <NewOrganizationMasterDialog {...signIn} organizationId={organizationId} deleted={list?.deleted_records ?? []} onClose={() => setCreating(false)} onChanged={onChanged} />}
      {list && deleting && <DeleteOrganizationMasterDialog organizationId={organizationId} master={deleting} links={list.links.filter(link => link.master === deleting.id).length} onClose={() => setDeleting(null)} onChanged={onChanged} />}
      {routing && (
        <RouteProposalDialog
          {...signIn}
          organizationId={organizationId}
          master={routing.master}
          initial={routing.initial}
          family={family}
          onClose={() => setRouting(null)}
          onChanged={summary => { setRouting(null); setChanged(summary); onReleaseChanged(summary); }}
        />
      )}
    </section>
  );
}

/** Where a route proposal starts: an alias and the operation to propose it for. */
type ProposalStart = { alias: string; operation: ProposalOperation };

/** Deployments of the record an alias routing others of it leaves out.
 * Only a suggestion: reviewing one opens the proposal for that alias, which
 * says why a deployment is left out, and nothing changes until it publishes. */
function Suggestions({ master, canChange, onReview }: { master: MasterRecord; canChange: boolean; onReview: (start: ProposalStart) => void }) {
  const groups = suggestionGroups(master);
  // As *Route across providers*: only a record whose deployments serve something to propose.
  const routable = masterOperations(master).length > 0;
  if (groups.length === 0) return null;
  return (
    <div className="master-suggestions" aria-label={`Suggestions for ${master.name}`} role="group">
      <h4>Suggested</h4>
      <ul className="plain-list">
        {groups.map(group => (
          <li key={`${group.alias} ${group.operation}`}>
            <code>{group.alias}</code> routes {operationLabel(group.operation).toLowerCase()} to other deployments of this model, but not to{' '}
            {group.deployments.map((deployment, index) => <React.Fragment key={deployment}>{index > 0 && ', '}<code>{deployment}</code></React.Fragment>)}.
            {canChange && routable && <> <button type="button" className="inline-action" onClick={() => onReview({ alias: group.alias, operation: group.operation })} aria-label={`Review the route for ${group.alias} ${operationLabel(group.operation).toLowerCase()}`}>Review the route</button></>}
          </li>
        ))}
      </ul>
    </div>
  );
}

/** How a deployment's master was decided. */
function Resolution({ deployment, listed }: { deployment: MasterDeployment; listed: boolean }) {
  if (deployment.resolved_by === 'card') return <Badge kind="neutral" title={`The card ${deployment.card} lists this provider and model`}>Card</Badge>;
  if (deployment.resolved_by === 'link') {
    const overrides = deployment.card && deployment.card !== deployment.master ? `; the card ${deployment.card} would match it otherwise` : '';
    if (deployment.master === null) return <Badge kind="warning" title={`The organization linked it to none${overrides}`}>Linked to none</Badge>;
    if (!listed) return <Badge kind="warning" title={`The link names ${deployment.master}, which no longer exists`}>Link to a deleted record</Badge>;
    return <Badge kind="success" title={`The organization linked it${overrides}`}>Linked</Badge>;
  }
  return <span className="muted">No card lists it</span>;
}

function MasterDeployments({ label, deployments, unlinked = false, family, canChange, onLink }: {
  label: string;
  /** The deployments with no record: a link naming one names a deleted record. */
  unlinked?: boolean;
  deployments: MasterDeployment[];
  family: (value: string) => string;
  canChange: boolean;
  onLink: (deployment: MasterDeployment) => void;
}) {
  return (
    <DataGrid aria-label={label} size="sm">
      <TableHead>
        <TableRow>
          {['Deployment', 'Provider', 'Location', 'Price per 1M tokens', 'Limits', 'Tools', 'Availability', 'Aliases', 'Record from', ...(canChange ? ['Actions'] : [])].map(name => <TableHeader key={name}>{name}</TableHeader>)}
        </TableRow>
      </TableHead>
      <TableBody>
        {deployments.map(deployment => {
          const operations = shown(deployment.operations);
          const availability = deployment.availability;
          const [availabilityLabel, kind] = availability ? availabilityLabels[availability.state] ?? [availability.state, 'neutral' as const] : ['Not yet checked', 'neutral' as const];
          const chat = operations.filter(op => !embeds(op.operation));
          const aliases = deploymentAliases(deployment);
          return (
            <TableRow key={deployment.deployment}>
              <DataCell label="Deployment"><code>{deployment.deployment}</code>{deployment.model !== deployment.deployment && <><br /><AppLink to={cardPath(deployment.model)} label={`Model card for ${deployment.model}`}><small>{deployment.model}</small></AppLink></>}</DataCell>
              <DataCell label="Provider">{family(deployment.provider_family)}<br /><span className="muted">{deployment.adapter_instance_key}</span></DataCell>
              <DataCell label="Location">{deployment.location ?? '—'}</DataCell>
              <DataCell label="Price per 1M tokens">
                {operations.length === 0 && <span className="muted">—</span>}
                {operations.map(({ operation, price }) => (
                  <div key={operation}>
                    {operationLabel(operation)}:{' '}
                    {price ? (
                      <>
                        {perMillion(price.input_minor_per_token, price.currency)} in
                        {!embeds(operation) && <> / {perMillion(price.output_minor_per_token, price.currency)} out</>}
                        {price.billed_by === 'override' && <> <Badge kind="warning" title={price.source_version ?? undefined}>Override</Badge></>}
                      </>
                    ) : <span className="muted">not priced, so not routed</span>}
                  </div>
                ))}
              </DataCell>
              <DataCell label="Limits">
                {operations.map(op => (
                  <div key={op.operation} title={`${operationLabel(op.operation)}: largest request ${op.max_input_bytes.toLocaleString()} bytes${embeds(op.operation) ? '' : `, at most ${op.max_output_tokens.toLocaleString()} output tokens`}`}>
                    {bytes(op.max_input_bytes)} in{!embeds(op.operation) && <> · {op.max_output_tokens.toLocaleString()} out</>}
                  </div>
                ))}
              </DataCell>
              <DataCell label="Tools">{chat.length === 0 ? '—' : chat.some(op => op.tools) ? <Badge kind="success">Tools</Badge> : <span className="muted">No</span>}</DataCell>
              <DataCell label="Availability">
                <Badge kind={kind} title={availability ? `${availability.reason ?? availability.state}, from ${availability.source} on ${formatDate(availability.observed_at)}` : undefined}>{availabilityLabel}</Badge>
              </DataCell>
              <DataCell label="Aliases">
                {aliases.length ? aliases.map(item => <div key={item.alias}><code>{item.alias}</code> <small className="muted">{item.operations.map(operationLabel).join(', ')}</small></div>) : <span className="muted">None</span>}
              </DataCell>
              <DataCell label="Record from"><Resolution deployment={deployment} listed={!unlinked} /></DataCell>
              {canChange && (
                <DataCell label="Actions">
                  <button type="button" className="inline-action" onClick={() => onLink(deployment)} aria-label={`Change the record of ${deployment.deployment}`}>Change link</button>
                </DataCell>
              )}
            </TableRow>
          );
        })}
      </TableBody>
    </DataGrid>
  );
}

const CLEAR = '\u0000clear';
const NONE = '\u0000none';

/** Link a deployment's provider family and model to a record, to none, or
 * clear the link so the card decides again. */
function LinkDialog({ organizationId, deployment, masters, onClose, onChanged, ...signIn }: SignIn & {
  organizationId: string;
  deployment: MasterDeployment;
  masters: MasterRecord[];
  onClose: () => void;
  onChanged: (summary: string) => void;
}) {
  // A cleared link is kept for its version only: the card decides, as with none set.
  const inUse = linkInUse(deployment.link);
  const current = !inUse ? CLEAR : inUse.master === null ? NONE : inUse.master;
  const [choice, setChoice] = useState(current);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  // The same choice retried after an unknown outcome reuses its key, so it replays.
  const attempt = useRef<{ choice: string; key: string } | null>(null);
  const choices = linkChoices(masters, deployment);
  const cards = choices.filter(item => item.kind === 'card');
  const own = choices.filter(item => item.kind === 'organization');
  const name = (id: string) => masters.find(item => item.id === id)?.name ?? id;
  // A link naming a record that no longer exists (or of another modality)
  // is still the current value: shown, so the select says what is set.
  const orphan = inUse?.master && !choices.some(item => item.id === inUse.master) ? inUse.master : null;
  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    if (choice === current) return;
    setProblem(null);
    setSubmitting(true);
    if (attempt.current?.choice !== choice) attempt.current = { choice, key: newIdempotencyKey() };
    const { key } = attempt.current;
    try {
      if (choice === CLEAR) {
        await api.clearMasterLink(organizationId, deployment.provider_family, deployment.model, inUse!.version, key);
      } else {
        await api.setMasterLink(organizationId, deployment.provider_family, deployment.model, choice === NONE ? null : choice, linkIfMatch(deployment.link), key);
      }
      attempt.current = null;
      onChanged(choice === CLEAR
        ? `Cleared the link of ${deployment.model}: ${deployment.card ? `the card ${deployment.card} decides` : 'no card lists it, so it has no record'}.`
        : choice === NONE ? `Linked ${deployment.model} to no record.` : `Linked ${deployment.model} to ${name(choice)}.`);
    } catch (caught) {
      const failure = problemOf(caught, 'The link could not be changed.');
      // The server answered: a new attempt is a new request.
      if (failure.status !== 0) attempt.current = null;
      setProblem(failure);
    } finally {
      setSubmitting(false);
    }
  };
  return (
    <Dialog open onClose={() => { if (!submitting) onClose(); }} title={`Record of ${deployment.model}`}>
      <form onSubmit={submit}>
        <div className="dialog-body">
          <p className="muted">
            The link is for <code>{deployment.model}</code> on {deployment.provider_family}, in every release, so every deployment of it follows. It changes no route or price.
            {deployment.card ? <> The card <code>{deployment.card}</code> lists it.</> : ' No card lists it.'}
          </p>
          <FormField label="Model record" htmlFor="master-link" hint="Records of the kind of model this deployment serves">
            <Select id="master-link" value={choice} onChange={event => { setChoice(event.target.value); setProblem(null); }}>
              <option value={CLEAR}>{deployment.card ? `The card’s match (${name(deployment.card)})` : 'No link (no card lists it)'}{!inUse ? ' · current' : ''}</option>
              <option value={NONE}>None: not part of any record{inUse && inUse.master === null ? ' · current' : ''}</option>
              {orphan && <option value={orphan}>{orphan} ({masters.some(item => item.id === orphan) ? 'another modality' : 'deleted'}) · current</option>}
              {cards.length > 0 && (
                <optgroup label="Cards">
                  {cards.map(item => <option key={item.id} value={item.id}>{item.name} ({item.id}){item.id === inUse?.master ? ' · current' : ''}</option>)}
                </optgroup>
              )}
              {own.length > 0 && (
                <optgroup label="Organization records">
                  {own.map(item => <option key={item.id} value={item.id}>{item.name} ({item.id}){item.id === inUse?.master ? ' · current' : ''}</option>)}
                </optgroup>
              )}
            </Select>
          </FormField>
        </div>
        <ActionError problem={masterProblem(problem)} action="change model links" {...signIn} />
        <div className="dialog-actions">
          <Button variant="ghost" type="button" onClick={onClose} disabled={submitting}>Cancel</Button>
          <Button type="submit" loading={submitting} disabled={choice === current}>{choice === CLEAR ? 'Clear link' : 'Save link'}</Button>
        </div>
      </form>
    </Dialog>
  );
}

function NewOrganizationMasterDialog({ organizationId, deleted, onClose, onChanged, ...signIn }: SignIn & { organizationId: string; deleted: OrganizationMaster[]; onClose: () => void; onChanged: (summary: string) => void }) {
  const [slug, setSlug] = useState('');
  const [name, setName] = useState('');
  const [modality, setModality] = useState<Modality>('chat');
  const [shownErrors, setShownErrors] = useState(false);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const attempt = useRef<{ body: string; key: string } | null>(null);
  const slugError = slugProblem(slug.trim());
  const nameError = name.trim() ? (name.trim().length > 120 ? 'At most 120 characters.' : null) : 'Enter a name.';
  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    setShownErrors(true);
    if (slugError || nameError) return;
    const id = `org/${slug.trim()}`;
    const input = { name: name.trim(), modality };
    // A deleted record is kept, and created again at its version.
    const version = recordVersion(id, deleted);
    const body = JSON.stringify([id, input, version]);
    if (attempt.current?.body !== body) attempt.current = { body, key: newIdempotencyKey() };
    setProblem(null);
    setSubmitting(true);
    try {
      await api.putOrganizationMaster(organizationId, id, input, version, attempt.current.key);
      attempt.current = null;
      onChanged(`${version === null ? 'Created' : 'Created again'} the record ${input.name} (${id}). Link deployments to it from their Change link.`);
    } catch (caught) {
      const failure = problemOf(caught, 'The record could not be created.');
      if (failure.status !== 0) attempt.current = null;
      setProblem(failure.code === 'version_conflict' ? { ...failure, code: 'master_exists', detail: `A record ${id} exists already. Choose another short name.` } : failure);
    } finally {
      setSubmitting(false);
    }
  };
  return (
    <Dialog open onClose={() => { if (!submitting) onClose(); }} title="New organization record">
      <form onSubmit={submit} autoComplete="off">
        <div className="dialog-body">
          <p className="muted">A record for a model no card covers, such as one of your own servers. Deployments link to it one by one.</p>
          <FormField label="Short name" htmlFor="master-slug" hint={recordVersion(`org/${slug.trim()}`, deleted) !== null ? `org/${slug.trim()} was deleted: this creates it again, with its history` : `Its id is org/${slug.trim() || '<short name>'}`} error={shownErrors ? slugError ?? undefined : undefined}>
            <TextInput id="master-slug" spellCheck={false} placeholder="qwen-local" value={slug} onChange={event => setSlug(event.target.value)} />
          </FormField>
          <FormField label="Name" htmlFor="master-name" error={shownErrors ? nameError ?? undefined : undefined}>
            <TextInput id="master-name" placeholder="Qwen 3 on our GPUs" value={name} onChange={event => setName(event.target.value)} />
          </FormField>
          <fieldset className="plain-fieldset">
            <legend className="cds--label">What the model does</legend>
            {(['chat', 'embeddings'] as const).map(value => (
              <RadioOption key={value} id={`master-modality-${value}`} name="master-modality" checked={modality === value} onChange={() => setModality(value)}>{modalityLabels[value]}</RadioOption>
            ))}
          </fieldset>
        </div>
        <ActionError problem={masterProblem(problem)} action="create model records" {...signIn} />
        <div className="dialog-actions">
          <Button variant="ghost" type="button" onClick={onClose} disabled={submitting}>Cancel</Button>
          <Button type="submit" loading={submitting}>Create record</Button>
        </div>
      </form>
    </Dialog>
  );
}

function DeleteOrganizationMasterDialog({ organizationId, master, links, onClose, onChanged }: { organizationId: string; master: MasterRecord; links: number; onClose: () => void; onChanged: (summary: string) => void }) {
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const attempt = useRef<string | null>(null);
  const remove = async () => {
    setProblem(null);
    setSubmitting(true);
    attempt.current ??= newIdempotencyKey();
    try {
      await api.deleteOrganizationMaster(organizationId, master.id, master.version ?? 0, attempt.current);
      attempt.current = null;
      onChanged(`Deleted the record ${master.name}.`);
    } catch (caught) {
      const failure = problemOf(caught, 'The record could not be deleted.');
      if (failure.status !== 0) attempt.current = null;
      setProblem(failure);
    } finally {
      setSubmitting(false);
    }
  };
  return (
    <ConfirmDialog open title={`Delete ${master.name}?`} confirmText="Delete" onClose={onClose} onConfirm={() => void remove()} problem={masterProblem(problem)} submitting={submitting}>
      The record <code>{master.id}</code> is hidden and kept, with its history. {links > 0
        ? <>{links === 1 ? 'One link names' : `${links} links name`} it: change or clear {links === 1 ? 'it' : 'them'} first, or the delete is refused.</>
        : 'No link names it.'}
    </ConfirmDialog>
  );
}

const changeLabels: Record<RouteChange['change'], [string, 'success' | 'danger' | 'warning' | 'neutral']> = {
  added: ['Added', 'success'],
  removed: ['Removed', 'danger'],
  changed: ['Changed', 'warning'],
  unchanged: ['Unchanged', 'neutral'],
};
const share = (value: RouteChange['before']) => (value ? `weight ${value.weight} · priority ${value.priority}` : '—');
const sameEntries = (a: ProposalEntry[], b: ProposalEntry[]) => JSON.stringify(a) === JSON.stringify(b);

/** A route for one record's model across its deployments: proposed by the
 * server against the active release, edited here, published as a release. */
export function RouteProposalDialog({ organizationId, master, initial, family, onClose, onChanged, ...signIn }: SignIn & {
  organizationId: string;
  master: MasterRecord;
  /** Start from this alias and operation (a suggestion) rather than the defaults. */
  initial?: ProposalStart;
  family: (value: string) => string;
  onClose: () => void;
  onChanged: (summary: string) => void;
}) {
  const served = masterOperations(master);
  const startOperations = (): ProposalOperation[] => (initial && served.includes(initial.operation) ? [initial.operation] : defaultOperations(master));
  const [operations, setOperations] = useState<ProposalOperation[]>(startOperations);
  const [tools, setTools] = useState(false);
  const [aliasInput, setAliasInput] = useState(initial?.alias ?? '');
  const [proposal, setProposal] = useState<RouteProposal | null>(null);
  const [entries, setEntries] = useState<ProposalEntry[]>([]);
  const [loading, setLoading] = useState(false);
  const [loadProblem, setLoadProblem] = useState<ApiProblem | null>(null);
  const [confirmed, setConfirmed] = useState(false);
  const [adding, setAdding] = useState('');
  const change = useCatalogChange(organizationId);
  const latest = useRef(0);
  const chatOperation = operations.some(operation => !operation.startsWith('embeddings'));
  const fetchProposal = useCallback(async (ops: ProposalOperation[], withTools: boolean, alias: string | undefined) => {
    const sequence = ++latest.current;
    change.reset();
    setLoadProblem(null);
    if (ops.length === 0) {
      setProposal(null);
      setEntries([]);
      setLoading(false);
      return;
    }
    setLoading(true);
    try {
      const loaded = await api.routeProposal(organizationId, master.id, ops, withTools, alias);
      if (sequence !== latest.current) return;
      setProposal(loaded);
      setEntries(proposalEntries(loaded.routes));
      setAliasInput(loaded.alias);
      setConfirmed(false);
      setAdding('');
    } catch (caught) {
      if (sequence === latest.current) setLoadProblem(problemOf(caught, 'The proposal could not be read.'));
    } finally {
      if (sequence === latest.current) setLoading(false);
    }
    // `change.reset` is stable in effect: it only clears the pending attempt.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [organizationId, master.id]);
  useEffect(() => {
    void fetchProposal(startOperations(), false, initial?.alias);
    return () => { latest.current += 1; };
    // `initial` is fixed while the dialog is open; `startOperations` reads it and `master`.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [fetchProposal, master]);
  const toggle = (operation: ProposalOperation, on: boolean) => {
    const next = served.filter(item => (item === operation ? on : operations.includes(item)));
    const keepTools = tools && next.some(item => !item.startsWith('embeddings'));
    setOperations(next);
    setTools(keepTools);
    void fetchProposal(next, keepTools, proposal?.alias ?? (aliasInput.trim() || undefined));
  };
  const update = (next: ProposalEntry[]) => {
    change.reset();
    setConfirmed(false);
    setEntries(next);
  };
  const byName = new Map(master.deployments.map(deployment => [deployment.deployment, deployment]));
  const changes = proposal ? routeChanges(proposal.existing, entries) : [];
  const existingChanges = changesExisting(changes);
  const localProblem = proposal ? proposalProblem(entries) : null;
  const edited = proposal ? !sameEntries(entries, proposalEntries(proposal.routes)) : false;
  const aliasStale = !!proposal && aliasInput.trim() !== proposal.alias;
  // The proposal shown was read for other operations or tool setting (a
  // read after a change failed): it is not what the controls ask for.
  const settingsStale = !!proposal && !proposalMatches(proposal, operations, tools);
  // Deployments that can be put (back) on the route: those the proposal had
  // and those it left out, for its operations, not on the route now.
  const candidates = proposal
    ? [
        ...proposal.routes.map(route => ({ operation: route.operation, deployment: route.deployment, reason: null as string | null })),
        ...proposal.excluded.map(item => ({ operation: item.operation, deployment: item.deployment, reason: `left out: ${exclusionLabels[item.reason] ?? item.reason}` })),
        // The alias's routes now, which may be to deployments of other records.
        ...proposal.existing.map(route => ({ operation: route.operation, deployment: route.deployment, reason: byName.has(route.deployment) ? null : 'on the alias now, of another record' })),
      ].filter((item, i, all) => all.findIndex(other => other.operation === item.operation && other.deployment === item.deployment) === i
        && !entries.some(entry => entry.operation === item.operation && entry.deployment === item.deployment))
    : [];
  const canPublish = !!proposal && !loading && !aliasStale && !settingsStale && entries.length > 0 && !localProblem && (!existingChanges || confirmed);
  const publish = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!proposal || !canPublish) return;
    const release = await change.submit(routeAcrossProviders(proposal, entries, new Date()));
    if (release) onChanged(`Routed ${proposal.alias} across providers; release ${release.version} is active.`);
  };
  return (
    <Dialog open size="lg" onClose={() => { if (!change.submitting) onClose(); }} title={`Route ${master.name} across providers`}>
      <form onSubmit={publish}>
        <div className="dialog-body">
          <p className="muted">
            One alias over every deployment of <code>{master.id}</code> that can serve it, proposed against the active release at weight 1 and priority 1. Requests go to the lowest priority with a healthy deployment, split by weight. Nothing changes until you publish: that creates a release and activates it.
          </p>
          <fieldset className="plain-fieldset" disabled={change.submitting}>
            <legend className="cds--label">Operations</legend>
            <div className="checkbox-group">
              {served.map(operation => (
                <CheckOption key={operation} checked={operations.includes(operation)} onChange={event => toggle(operation, event.target.checked)}>{operationLabel(operation)}</CheckOption>
              ))}
              <CheckOption checked={tools} disabled={!chatOperation} onChange={event => { setTools(event.target.checked); void fetchProposal(operations, event.target.checked, proposal?.alias); }}>With tools (chat only)</CheckOption>
            </div>
            <div className="form-grid">
              <FormField label="Alias" htmlFor="proposal-alias" hint="The model name clients send">
                <TextInput id="proposal-alias" spellCheck={false} value={aliasInput} onChange={event => setAliasInput(event.target.value)} />
              </FormField>
              <Button variant="ghost" type="button" disabled={!aliasInput.trim() || loading || operations.length === 0} onClick={() => void fetchProposal(operations, tools, aliasInput.trim())}>{aliasStale ? 'Propose for this alias' : 'Get the proposal again'}</Button>
            </div>
          </fieldset>
          {aliasStale && <Message kind="info">Get the proposal for <code>{aliasInput.trim() || '…'}</code> before publishing, so the changes below are against that alias.</Message>}
          {settingsStale && !loading && <Message kind="warning">The proposal below was read for {proposal!.operations.map(operationLabel).join(', ').toLowerCase()}{proposal!.tools ? ' with tools' : ''}, not the operations chosen above. Get the proposal again before publishing.</Message>}
          {operations.length === 0 && <Message kind="warning">Choose at least one operation.</Message>}
          {loading && <Message kind="info">Reading the proposal…</Message>}
          {loadProblem && <ActionError problem={loadProblem} action="propose routes" {...signIn} />}
          {proposal && !loading && (
            <>
              {proposal.alias_exists
                ? <Message kind="warning" title={`${proposal.alias} exists already`}>Publishing replaces its {proposal.operations.map(operationLabel).join(', ').toLowerCase()} routes with the ones below; its other operations stay as they are. Check the changes before you publish.</Message>
                : <Message kind="info">The alias <code>{proposal.alias}</code> is new.</Message>}
              {/* Locked while publishing: the publish carries the routes as they were. */}
              <fieldset className="plain-fieldset" disabled={change.submitting}>
              <h4>Routes</h4>
              {entries.length === 0
                ? <p className="muted">No deployment is on the route. Add one below.</p>
                : (
                  <DataGrid size="sm" aria-label="Proposed routes">
                    <TableHead>
                      <TableRow>{['Operation', 'Deployment', 'Provider', 'Weight', 'Priority', ''].map(name => <TableHeader key={name}>{name}</TableHeader>)}</TableRow>
                    </TableHead>
                    <TableBody>
                      {entries.map((entry, index) => {
                        const deployment = byName.get(entry.deployment);
                        const set = (field: 'weight' | 'priority', value: string) => update(entries.map((item, i) => (i === index ? { ...item, [field]: value } : item)));
                        const label = `${entry.deployment} ${operationLabel(entry.operation).toLowerCase()}`;
                        return (
                          <TableRow key={`${entry.operation}-${entry.deployment}`}>
                            <DataCell label="Operation">{operationLabel(entry.operation)}</DataCell>
                            <DataCell label="Deployment"><code>{entry.deployment}</code></DataCell>
                            <DataCell label="Provider">{deployment ? <>{family(deployment.provider_family)}<br /><span className="muted">{deployment.adapter_instance_key}{deployment.location ? ` · ${deployment.location}` : ''}</span></> : '—'}</DataCell>
                            <DataCell label="Weight"><TextInput className="proposal-number" aria-label={`Weight of ${label}`} inputMode="numeric" value={entry.weight} onChange={event => set('weight', event.target.value)} /></DataCell>
                            <DataCell label="Priority"><TextInput className="proposal-number" aria-label={`Priority of ${label}`} inputMode="numeric" value={entry.priority} onChange={event => set('priority', event.target.value)} /></DataCell>
                            <DataCell label="Actions"><button type="button" className="inline-action" onClick={() => update(entries.filter((_, i) => i !== index))} aria-label={`Remove ${label}`}>Remove</button></DataCell>
                          </TableRow>
                        );
                      })}
                    </TableBody>
                  </DataGrid>
                )}
              <p className="muted">Weight 1 to 1000, priority 1 (first) to 10.</p>
              {candidates.length > 0 && (
                <div className="form-grid">
                  <FormField label="Add a deployment" htmlFor="proposal-add">
                    <Select id="proposal-add" value={adding} onChange={event => setAdding(event.target.value)}>
                      <option value="">Choose a deployment</option>
                      {candidates.map(item => {
                        const value = `${item.operation}\u0000${item.deployment}`;
                        return <option key={value} value={value}>{item.deployment} · {operationLabel(item.operation)}{item.reason ? ` (${item.reason})` : ''}</option>;
                      })}
                    </Select>
                  </FormField>
                  <Button variant="ghost" type="button" disabled={!adding} onClick={() => {
                    const [operation, deployment] = adding.split('\u0000');
                    // A route the alias has now comes back as it is.
                    const now = proposal.existing.find(route => route.operation === operation && route.deployment === deployment);
                    update([...entries, { operation, deployment, weight: String(now?.weight ?? 1), priority: String(now?.priority ?? 1) }]);
                    setAdding('');
                  }}>Add</Button>
                </div>
              )}
              </fieldset>
              {proposal.excluded.length > 0 && (
                <>
                  <h4>Left out</h4>
                  <p className="muted">Deployments of this record the proposal leaves out, and why. Adding one back may make the release refused.</p>
                  <DataGrid size="sm" aria-label="Deployments left out">
                    <TableHead>
                      <TableRow>{['Operation', 'Deployment', 'Reason', 'Detail'].map(name => <TableHeader key={name}>{name}</TableHeader>)}</TableRow>
                    </TableHead>
                    <TableBody>
                      {proposal.excluded.map(item => (
                        <TableRow key={`${item.operation}-${item.deployment}`}>
                          <DataCell label="Operation">{operationLabel(item.operation)}</DataCell>
                          <DataCell label="Deployment"><code>{item.deployment}</code></DataCell>
                          <DataCell label="Reason"><Badge kind="warning">{exclusionLabels[item.reason] ?? item.reason}</Badge></DataCell>
                          <DataCell label="Detail">{item.detail}</DataCell>
                        </TableRow>
                      ))}
                    </TableBody>
                  </DataGrid>
                </>
              )}
              <h4>Changes to {proposal.alias}</h4>
              {proposal.existing.length === 0
                ? <p className="muted">The alias routes none of these operations now: every route below is added.</p>
                : (
                  <DataGrid size="sm" aria-label={`Changes to ${proposal.alias}`}>
                    <TableHead>
                      <TableRow>{['Operation', 'Deployment', 'Change', 'Now', 'After'].map(name => <TableHeader key={name}>{name}</TableHeader>)}</TableRow>
                    </TableHead>
                    <TableBody>
                      {changes.map(item => {
                        const [label, kind] = changeLabels[item.change];
                        return (
                          <TableRow key={`${item.operation}-${item.deployment}`}>
                            <DataCell label="Operation">{operationLabel(item.operation)}</DataCell>
                            <DataCell label="Deployment"><code>{item.deployment}</code></DataCell>
                            <DataCell label="Change"><Badge kind={kind}>{label}</Badge></DataCell>
                            <DataCell label="Now">{share(item.before)}</DataCell>
                            <DataCell label="After">{share(item.after)}</DataCell>
                          </TableRow>
                        );
                      })}
                    </TableBody>
                  </DataGrid>
                )}
              {proposal.problem && (
                <Message kind={edited ? 'warning' : 'error'} title={edited ? 'The proposal as first read would be refused' : 'This would be refused'}>
                  {proposal.problem}{edited ? ' The release is checked again when you publish.' : ''}
                </Message>
              )}
              {localProblem && entries.length > 0 && <Message kind="error">{localProblem}</Message>}
              {existingChanges && (
                <CheckOption checked={confirmed} onChange={event => setConfirmed(event.target.checked)}>Change the existing alias {proposal.alias} as shown</CheckOption>
              )}
            </>
          )}
        </div>
        <ChangeFeedback change={change} {...signIn} />
        <div className="dialog-actions">
          <Button variant="ghost" type="button" onClick={onClose} disabled={change.submitting}>Cancel</Button>
          <Button type="submit" loading={change.submitting} disabled={!canPublish}>Publish and activate</Button>
        </div>
      </form>
    </Dialog>
  );
}
