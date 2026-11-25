import type { CatalogManifest, CatalogOperation, MasterDeployment, MasterLinkRef, MasterList, MasterRecord, Modality, OrganizationMaster, ProposalOperation, ProposedRoute, RouteChange, RouteProposal } from './api';
import { releaseVersion, type CatalogEdit } from './catalog-change';
import { MAX_ROUTE_DEPLOYMENTS } from './catalog-edit';

// Master model records (D-033): pure helpers for the Models page's view of
// one record per model, its links and its route across providers.

/** The `If-Match` for setting a deployment's link: `"v0"` while none was
 * ever set (so two first links cannot overwrite each other), else the link's
 * version: a cleared link is kept, and set again at its version. */
export function linkIfMatch(link: MasterLinkRef | null): string {
  return `"v${link ? link.version : 0}"`;
}

/** The link in use, if any: a cleared one is kept only for its version. */
export const linkInUse = (link: MasterLinkRef | null): MasterLinkRef | null => (link && !link.cleared ? link : null);

/** The version to create organization record `id` at: a deleted one's (it
 * is kept, and created again at its version), else none (`"v0"`). */
export function recordVersion(id: string, deleted: Pick<OrganizationMaster, 'id' | 'version'>[]): number | null {
  return deleted.find(record => record.id === id)?.version ?? null;
}

/** Whether a proposal was read for these operations and tool setting, so
 * what it shows is what would be published. */
export function proposalMatches(proposal: Pick<RouteProposal, 'operations' | 'tools'>, operations: string[], tools: boolean): boolean {
  const key = (items: string[]) => [...new Set(items)].sort().join(',');
  return proposal.tools === tools && key(proposal.operations) === key(operations);
}

/** What a record is for: chat (with streaming and batch) or embeddings. */
export function modalityOf(operation: string): Modality | null {
  if (operation === 'chat' || operation === 'chat_stream' || operation === 'chat_batch') return 'chat';
  if (operation === 'embeddings' || operation === 'embeddings_batch') return 'embeddings';
  return null;
}

/** The modalities a deployment serves: it links only to a master of one of them. */
export function deploymentModalities(deployment: Pick<MasterDeployment, 'operations'>): Modality[] {
  const found = new Set(deployment.operations.flatMap(op => modalityOf(op.operation) ?? []));
  return (['chat', 'embeddings'] as const).filter(modality => found.has(modality));
}

/** The masters a deployment may link to: those of a modality it serves, by name. */
export function linkChoices(items: MasterRecord[], deployment: Pick<MasterDeployment, 'operations'>): MasterRecord[] {
  const modalities = deploymentModalities(deployment);
  return items.filter(item => modalities.includes(item.modality)).sort((a, b) => a.name.localeCompare(b.name) || a.id.localeCompare(b.id));
}

export type MasterGroups = {
  /** Records with at least one linked deployment, by name. */
  linked: MasterRecord[];
  /** The organization's own records no deployment links to (yet). */
  idleOrganization: MasterRecord[];
  /** Deployments with no master: none matched, linked to none, or to a record since deleted. */
  unlinked: MasterDeployment[];
};

const byName = (a: MasterRecord, b: MasterRecord) => a.name.localeCompare(b.name) || a.id.localeCompare(b.id);
const byDeployment = (a: MasterDeployment, b: MasterDeployment) => a.deployment.localeCompare(b.deployment);

export function groupMasters(list: MasterList): MasterGroups {
  const sorted = (item: MasterRecord) => ({ ...item, deployments: [...item.deployments].sort(byDeployment) });
  return {
    linked: list.items.filter(item => item.deployments.length > 0).map(sorted).sort(byName),
    idleOrganization: list.items.filter(item => item.kind === 'organization' && item.deployments.length === 0).sort(byName),
    unlinked: [...list.unlinked].sort(byDeployment),
  };
}

/** The aliases routing to a deployment, each with the operations it routes
 * there (`chat_stream` of an older release is chat). */
export function deploymentAliases(deployment: Pick<MasterDeployment, 'routes'>): { alias: string; operations: string[] }[] {
  const aliases = new Map<string, string[]>();
  for (const route of deployment.routes) {
    const operation = route.operation === 'chat_stream' ? 'chat' : route.operation;
    const operations = aliases.get(route.alias) ?? [];
    if (!operations.includes(operation)) operations.push(operation);
    aliases.set(route.alias, operations);
  }
  return [...aliases].map(([alias, operations]) => ({ alias, operations })).sort((a, b) => a.alias.localeCompare(b.alias));
}

export const proposalOperationOrder: ProposalOperation[] = ['chat', 'embeddings', 'chat_batch', 'embeddings_batch'];

/** The operations a route across providers can be proposed for: those a
 * linked deployment of the record serves. */
export function masterOperations(master: Pick<MasterRecord, 'deployments'>): ProposalOperation[] {
  const served = new Set(master.deployments.flatMap(deployment => deployment.operations.map(op => (op.operation === 'chat_stream' ? 'chat' : op.operation))));
  return proposalOperationOrder.filter(operation => served.has(operation));
}

/** A record's suggestions by alias and operation, each with the deployments
 * the alias leaves out, in the order the server gives them. */
export function suggestionGroups(master: Pick<MasterRecord, 'suggestions'>): { alias: string; operation: ProposalOperation; deployments: string[] }[] {
  const groups: { alias: string; operation: ProposalOperation; deployments: string[] }[] = [];
  for (const suggestion of master.suggestions ?? []) {
    const group = groups.find(item => item.alias === suggestion.alias && item.operation === suggestion.operation);
    if (group) group.deployments.push(suggestion.deployment);
    else groups.push({ alias: suggestion.alias, operation: suggestion.operation, deployments: [suggestion.deployment] });
  }
  return groups;
}

/** The operations a proposal starts with: chat when served, else the first served. */
export function defaultOperations(master: Pick<MasterRecord, 'deployments'>): ProposalOperation[] {
  const served = masterOperations(master);
  return served.includes('chat') ? ['chat'] : served.slice(0, 1);
}

/** One route of a proposal as the dialog holds it: weight and priority as typed. */
export type ProposalEntry = { operation: string; deployment: string; weight: string; priority: string };

export const proposalEntries = (routes: ProposedRoute[]): ProposalEntry[] =>
  routes.map(route => ({ operation: route.operation, deployment: route.deployment, weight: String(route.weight), priority: String(route.priority) }));

const wholeIn = (value: string, max: number) => /^\d+$/.test(value.trim()) && Number(value) >= 1 && Number(value) <= max;

/** Why the edited routes cannot be published, or null. */
export function proposalProblem(entries: ProposalEntry[]): string | null {
  if (entries.length === 0) return 'Route the alias to at least one deployment.';
  const seen = new Set<string>();
  const perOperation = new Map<string, number>();
  for (const entry of entries) {
    const key = `${entry.operation}\u0000${entry.deployment}`;
    if (seen.has(key)) return `${entry.deployment} appears twice for ${entry.operation}.`;
    seen.add(key);
    perOperation.set(entry.operation, (perOperation.get(entry.operation) ?? 0) + 1);
    if (!wholeIn(entry.weight, 1000)) return `The weight of ${entry.deployment} is a whole number from 1 to 1000.`;
    if (!wholeIn(entry.priority, 10)) return `The priority of ${entry.deployment} is a whole number from 1 to 10.`;
  }
  for (const [operation, count] of perOperation) {
    if (count > MAX_ROUTE_DEPLOYMENTS) return `An alias routes an operation to at most ${MAX_ROUTE_DEPLOYMENTS} deployments; ${operation} has ${count}.`;
  }
  return null;
}

type Weighted = { operation: string; deployment: string; weight: number; priority: number };
const toWeighted = (entry: ProposalEntry): Weighted => ({ operation: entry.operation, deployment: entry.deployment, weight: Number(entry.weight), priority: Number(entry.priority) });
const routeKey = (route: { operation: string; deployment: string }) => `${route.operation}\u0000${route.deployment}`;

/** The edited routes against the alias's routes of those operations now
 * (`existing`), by operation and deployment, as the proposal's `changes`. */
export function routeChanges(existing: ProposedRoute[], entries: ProposalEntry[]): RouteChange[] {
  const before = new Map(existing.map(route => [routeKey(route), route]));
  const after = new Map(entries.map(entry => [routeKey(entry), toWeighted(entry)]));
  const keys = [...new Set([...before.keys(), ...after.keys()])];
  const order = (operation: string) => {
    const index = proposalOperationOrder.indexOf(operation as ProposalOperation);
    return index < 0 ? proposalOperationOrder.length : index;
  };
  return keys
    .map((key): RouteChange => {
      const was = before.get(key);
      const will = after.get(key);
      const route = (was ?? will)!;
      const weight = (value: Weighted | ProposedRoute | undefined) => (value ? { weight: value.weight, priority: value.priority } : null);
      const change = !was ? 'added' : !will ? 'removed' : was.weight === will.weight && was.priority === will.priority ? 'unchanged' : 'changed';
      return { operation: route.operation, deployment: route.deployment, change, before: weight(was), after: weight(will) };
    })
    .sort((a, b) => order(a.operation) - order(b.operation) || a.deployment.localeCompare(b.deployment));
}

/** Whether publishing changes or removes a route the alias has now. */
export const changesExisting = (changes: RouteChange[]) => changes.some(change => change.change === 'changed' || change.change === 'removed');

const sameRoutes = (a: Weighted[], b: Weighted[]) => {
  const key = (routes: Weighted[]) => JSON.stringify(routes.map(route => [route.operation, route.deployment, route.weight, route.priority]).sort());
  return key(a) === key(b);
};

/** Route `alias`'s `operations` to exactly `entries`: the base's routes of
 * that alias and those operations are removed, the entries added, and
 * nothing else changes. Refused when the active release routes the alias
 * otherwise than the proposal's `existing` said, so a publish never undoes a
 * change it did not show. */
export function routeAcrossProviders(proposal: Pick<RouteProposal, 'alias' | 'operations' | 'existing'>, entries: ProposalEntry[], now: Date): CatalogEdit {
  const { alias } = proposal;
  const operations = new Set(proposal.operations);
  return base => {
    if (!base) return { problem: 'No catalog release is active.' };
    const problem = proposalProblem(entries);
    if (problem) return { problem };
    const touched = (route: CatalogManifest['routes'][number]) => route.alias === alias && operations.has(route.operation);
    const current = base.routes.filter(touched).map(route => ({ operation: route.operation as string, deployment: route.deployment, weight: route.weight ?? 1, priority: route.priority ?? 1 }));
    if (!sameRoutes(current, proposal.existing)) {
      return { problem: `The routing of ${alias} changed since the proposal was read. Get the proposal again to see the change.` };
    }
    for (const entry of entries) {
      if (!operations.has(entry.operation)) return { problem: `${entry.operation} is not one of the proposal's operations.` };
      if (!base.deployments.some(deployment => deployment.name === entry.deployment)) return { problem: `${entry.deployment} is no longer in the catalog.` };
    }
    const routes = base.routes.filter(route => !touched(route));
    for (const entry of entries) {
      const route: CatalogManifest['routes'][number] = { alias, operation: entry.operation as CatalogOperation, deployment: entry.deployment };
      const weight = Number(entry.weight);
      const priority = Number(entry.priority);
      // Left out when 1, so an unchanged route keeps the release's digest.
      if (weight !== 1) route.weight = weight;
      if (priority !== 1) route.priority = priority;
      routes.push(route);
    }
    return { ...base, routes, release: { ...base.release, version: releaseVersion(now, `routes ${alias} across providers`) } };
  };
}

/** What an exclusion reason means, briefly; the server's `detail` says more. */
export const exclusionLabels: Record<string, string> = {
  no_price: 'Not priced',
  provider_not_permitted: 'Provider not permitted',
  location_not_permitted: 'Location not permitted',
  availability_refused: 'Refused',
  availability_requires_action: 'Needs action',
  no_tools: 'No tool calling',
  mixed_tool_call_ids: 'Other tool call ids',
  currency: 'Other currency',
  too_many_deployments: 'Route full',
};

/** An organization record's slug: lowercase letters, digits, `-`, `.` and `_`,
 * starting with a letter or digit, at most 64 characters. */
export function slugProblem(slug: string): string | null {
  if (!slug) return 'Enter a short name.';
  if (slug.length > 64) return 'At most 64 characters.';
  if (!/^[a-z0-9][a-z0-9._-]*$/.test(slug)) return 'Lowercase letters, digits, “-”, “.” and “_”, starting with a letter or digit.';
  return null;
}
