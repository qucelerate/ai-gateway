// How each deployment has been answering lately, for the route editor
// (D-023): the outcome of its requests over the last 15 minutes, from the
// usage traces every gateway instance writes. A deployment's circuit is per
// instance and is not shown; a run of failures here is what opens it.

import React, { useEffect, useState } from 'react';
import { api, listFilter, type UsageSeriesReport } from './api';
import { Badge } from './ui';

export const HEALTH_WINDOW_MS = 15 * 60 * 1000;
/** The usage series splits into at most this many deployments a query. */
const PER_QUERY = 20;

export type HealthState = 'idle' | 'healthy' | 'degraded' | 'failing';

export type DeploymentHealth = {
  state: HealthState;
  /** Requests in the window with a known provider result: not pending,
   * not expired before they were sent, not closed with an unknown result. */
  requests: number;
  /** Of those, failed, or refused and failed over to another deployment. */
  failed: number;
};

/** A deployment's health from its finished requests and their failures: at
 * least half of three or more failing is failing, any failure degraded. */
export function healthOf(requests: number, failed: number): DeploymentHealth {
  const state: HealthState = requests === 0
    ? 'idle'
    : failed === 0
      ? 'healthy'
      : requests >= 3 && failed * 2 >= requests ? 'failing' : 'degraded';
  return { state, requests, failed };
}

const totalsBy = (report: UsageSeriesReport) => {
  const byName = new Map<string, UsageSeriesReport['series'][number]['totals']>();
  for (const series of report.series) if (series.value !== null && !series.other) byName.set(series.value, series.totals);
  return byName;
};

/** The health of each named deployment over the window ending `now`. */
export async function loadDeploymentHealth(organizationId: string, deployments: string[], now = Date.now()): Promise<Map<string, DeploymentHealth>> {
  const names = Array.from(new Set(deployments));
  const window = { from: new Date(now - HEALTH_WINDOW_MS).toISOString(), to: new Date(now).toISOString(), granularity: 'hour' as const, split_by: 'deployment' as const, limit: PER_QUERY };
  const health = new Map<string, DeploymentHealth>();
  for (let start = 0; start < names.length; start += PER_QUERY) {
    const chunk = names.slice(start, start + PER_QUERY);
    const deployment = listFilter(chunk);
    const [all, failedOver] = await Promise.all([
      api.usageSeries(organizationId, { ...window, deployment }),
      api.usageSeries(organizationId, { ...window, deployment, status: 'failed-over' }),
    ]);
    const totals = totalsBy(all);
    const overs = totalsBy(failedOver);
    for (const name of chunk) {
      const total = totals.get(name);
      const finished = total ? total.requests - total.pending_requests - total.expired_requests - total.unknown_requests : 0;
      const failed = total ? total.failed_requests + (overs.get(name)?.requests ?? 0) : 0;
      health.set(name, healthOf(finished, Math.min(failed, finished)));
    }
  }
  return health;
}

export type HealthLoad = Map<string, DeploymentHealth> | 'loading' | 'unavailable';

/** Health is read again this often while it is shown. */
export const HEALTH_REFRESH_MS = 60 * 1000;

/** The named deployments' health, read when the names change and a
 * minute after each read, with when it was last read; `unavailable` to someone who
 * may not read usage. */
export function useDeploymentHealth(organizationId: string, deployments: string[]): { health: HealthLoad; readAt: Date | null } {
  const [state, setState] = useState<{ health: HealthLoad; readAt: Date | null }>({ health: 'loading', readAt: null });
  const key = Array.from(new Set(deployments)).sort().join('\u0000');
  useEffect(() => {
    let live = true;
    setState({ health: key ? 'loading' : new Map(), readAt: null });
    if (!key) return;
    let timer: ReturnType<typeof setTimeout> | undefined;
    // The next read is scheduled once this one is done, so two never
    // overlap and an older answer never replaces a newer one.
    const read = () => {
      const now = Date.now();
      loadDeploymentHealth(organizationId, key.split('\u0000'), now)
        .then(result => { if (live) setState({ health: result, readAt: new Date(now) }); })
        // A refresh that fails keeps what was read; only a first read
        // that fails says health is unavailable.
        .catch(() => { if (live) setState(current => (current.health instanceof Map ? current : { health: 'unavailable', readAt: null })); })
        .finally(() => { if (live) timer = setTimeout(read, HEALTH_REFRESH_MS); });
    };
    read();
    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [organizationId, key]);
  return state;
}

/** When health was read, for beside the table. */
export function healthReadAt(readAt: Date | null): string {
  return readAt ? `Health as of ${readAt.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}, read again every minute.` : '';
}

const labels: Record<HealthState, string> = { idle: 'No recent requests', healthy: 'Healthy', degraded: 'Degraded', failing: 'Failing' };

/** One deployment's entry of a health load. */
export function healthFor(load: HealthLoad, deployment: string): DeploymentHealth | 'loading' | 'unavailable' | undefined {
  return load instanceof Map ? load.get(deployment) : load;
}

/** A deployment's health as a tag, with its counts. */
export function HealthBadge({ health }: { health: DeploymentHealth | 'loading' | 'unavailable' | undefined }) {
  if (health === 'loading') return <span className="muted">Checking…</span>;
  if (health === 'unavailable') return <span className="muted" title="Health comes from usage, which you may not read.">—</span>;
  if (!health) return <span className="muted">—</span>;
  const kind = health.state === 'healthy' ? 'success' : health.state === 'failing' ? 'danger' : health.state === 'degraded' ? 'warning' : 'neutral';
  const detail = health.state === 'idle'
    ? 'No requests in the last 15 minutes.'
    : `${health.failed} of ${health.requests} ${health.requests === 1 ? 'request' : 'requests'} failed in the last 15 minutes.`;
  const counts = health.state === 'degraded' || health.state === 'failing' ? ` · ${health.failed}/${health.requests}` : '';
  return <Badge kind={kind} title={detail}>{labels[health.state]}{counts}</Badge>;
}
