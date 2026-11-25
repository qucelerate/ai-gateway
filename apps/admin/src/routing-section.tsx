import React, { useEffect, useState } from 'react';
import {
  api,
  newIdempotencyKey,
  type ApiProblem,
  type ConfigInstance,
  type ConfigPin,
  type ConfigVersion,
} from './api';
import type { CollectionState } from './use-collection';
import type { MutationState } from './mutation-state';
import {
  Badge,
  Button,
  ConfirmDialog,
  DataCell,
  EmptyState,
  InlineError,
  LoadMore,
  formatDate,
  uuidHint,
} from './ui';
import { DataGrid, TableBody, TableHead, TableHeader, TableRow } from './ui';

/** Instances that serve this organization something other than what it
 * serves now: still applying the latest change, or not reporting. */
export function instancesBehind(instances: ConfigInstance[], pin: ConfigPin | null): ConfigInstance[] {
  if (!pin) return [];
  return instances.filter(instance => instance.stale || instance.serving_version_id !== pin.served_version_id);
}

function problemOf(caught: unknown): ApiProblem {
  return (caught as { problem?: ApiProblem }).problem ?? {
    type: 'about:blank', code: 'network_error', title: 'network_error', status: 0,
    detail: 'The gateway could not be reached.', instance: '', request_id: '',
  };
}

/**
 * The organization's routing configuration versions, its pin, and what each
 * gateway instance serves it. Pinning an earlier version is the rollback
 * brake: every instance serves it until unpinned. Pinning needs catalog
 * write, which an admin role binding grants whatever the membership role,
 * so Pin and Unpin are always offered and the server decides.
 */
export function RoutingSection({
  organizationId,
  versions,
  mutation,
  onSuccess,
}: {
  organizationId: string;
  versions: CollectionState<ConfigVersion>;
  mutation: MutationState<unknown>;
  onSuccess: () => void;
}) {
  const [pin, setPin] = useState<ConfigPin | null>(null);
  const [instances, setInstances] = useState<ConfigInstance[]>([]);
  const [statusError, setStatusError] = useState<ApiProblem | null>(null);
  const [statusLoading, setStatusLoading] = useState(true);
  const [pinning, setPinning] = useState<ConfigVersion | null>(null);
  const [unpinning, setUnpinning] = useState(false);
  // One key per confirmation: a retry after a lost response replays it.
  const [writeKey, setWriteKey] = useState('');

  const loadStatus = React.useCallback(async () => {
    setStatusLoading(true);
    setStatusError(null);
    try {
      const [nextPin, page] = await Promise.all([
        api.configPin(organizationId),
        api.listConfigInstances(organizationId),
      ]);
      setPin(nextPin);
      setInstances(page.items);
    } catch (caught) {
      setStatusError(problemOf(caught));
    } finally {
      setStatusLoading(false);
    }
  }, [organizationId]);

  useEffect(() => {
    void loadStatus();
  }, [loadStatus]);

  const refresh = () => {
    void loadStatus();
    onSuccess();
  };

  const confirmPin = async () => {
    if (!pinning || !pin) return;
    const target = pinning;
    const ok = await mutation.submit(async () => {
      const { data, meta } = await api.pinConfigVersion(organizationId, target.id, pin.version, writeKey);
      return { data, requestId: meta.requestId };
    });
    if (!ok) return;
    setPinning(null);
    refresh();
  };

  const confirmUnpin = async () => {
    if (!pin) return;
    const ok = await mutation.submit(async () => {
      const { data, meta } = await api.unpinConfigVersion(organizationId, pin.version, writeKey);
      return { data, requestId: meta.requestId };
    });
    if (!ok) return;
    setUnpinning(false);
    refresh();
  };

  const behind = instancesBehind(instances, pin);

  return (
    <>
      <div className="section-header">
        <h3>Routing versions</h3>
      </div>
      <p className="cds--form__helper-text">
        Each change to the active catalog release or a price override publishes a new version of this
        organization&apos;s routing. Every gateway instance serves the newest version, unless you pin an
        earlier one: then every instance serves the pinned version until you unpin it. Changes made while
        pinned are still published, but not served. Pinning never restores a deleted key, credential or
        budget.
      </p>

      {statusLoading && <p role="status">Loading pin…</p>}
      <InlineError problem={statusError} />
      {pin && (
        <div className="section-summary" role="status" aria-label="Pin">
          {pin.pinned_version_id ? (
            <>
              <Badge status="pinned" /> Pinned to version {uuidHint(pin.pinned_version_id)}.{' '}
              {pin.newest_version_id !== pin.pinned_version_id && 'Newer versions are not served.'}
              <Button variant="danger" onClick={() => { mutation.clearProblem(); setWriteKey(newIdempotencyKey()); setUnpinning(true); }}>
                Unpin
              </Button>
            </>
          ) : pin.served_version_id ? (
            <>Serving the newest version, {uuidHint(pin.served_version_id)}.</>
          ) : (
            <>No version yet: activate a catalog release to publish one.</>
          )}
        </div>
      )}

      {versions.loading && <p role="status">Loading versions…</p>}
      <InlineError problem={versions.error} />
      {!versions.loading && !versions.error && versions.items.length === 0 && (
        <EmptyState title="No versions" text="A version is published when a catalog release is activated." />
      )}
      {/* Also with no rows, or after a failed read, to try again. */}
      {!versions.loading && (
        <div className="table-toolbar table-toolbar-end" role="toolbar" aria-label="Routing version actions"><Button variant="ghost" onClick={refresh}>Refresh</Button></div>
      )}
      {versions.items.length > 0 && (
        <DataGrid aria-label="Routing versions">
          <TableHead>
            <TableRow className="data-row head">
              <TableHeader>Version</TableHeader>
              <TableHeader>Published</TableHeader>
              <TableHeader>Release</TableHeader>
              <TableHeader>Routes · overrides</TableHeader>
              <TableHeader>Status</TableHeader>
              <TableHeader className="row-actions">Actions</TableHeader>
            </TableRow>
          </TableHead>
          <TableBody>
            {versions.items.map(version => (
              <TableRow className="data-row" key={version.id}>
                <DataCell label="Version" className="cell-primary">
                  <span title={`${version.id}\nsha256 ${version.digest}`}>{uuidHint(version.id)}</span>
                </DataCell>
                <DataCell label="Published">{formatDate(version.created_at)}</DataCell>
                <DataCell label="Release">{version.release_id ? uuidHint(version.release_id) : 'None'}</DataCell>
                <DataCell label="Routes · overrides">{version.routes} · {version.overrides}</DataCell>
                <DataCell label="Status">
                  {version.served && <Badge status="active" />}
                  {version.pinned && <Badge status="pinned" />}
                  {version.newest && <Badge status="newest" />}
                </DataCell>
                <DataCell label="Actions" className="row-actions">
                  {pin && !version.pinned && (
                    <Button variant="ghost" onClick={() => { mutation.clearProblem(); setWriteKey(newIdempotencyKey()); setPinning(version); }}>
                      Pin
                    </Button>
                  )}
                </DataCell>
              </TableRow>
            ))}
          </TableBody>
        </DataGrid>
      )}
      <LoadMore hasMore={versions.hasMore} loading={versions.loadingMore} onClick={versions.loadMore} />

      <div className="section-header">
        <h3>Gateway instances</h3>
      </div>
      <p className="cds--form__helper-text">
        What each gateway instance serves this organization. An instance unseen for five minutes is stale.
        {pin && instances.length > 0 && (behind.length === 0
          ? ' Every instance serves the current version.'
          : ` ${behind.length} of ${instances.length} not serving the current version yet.`)}
      </p>
      {!statusLoading && !statusError && instances.length === 0 && (
        <EmptyState title="No instances" text="No gateway instance has reported in the last seven days." />
      )}
      {instances.length > 0 && (
        <DataGrid aria-label="Gateway instances">
          <TableHead>
            <TableRow className="data-row head">
              <TableHeader>Instance</TableHeader>
              <TableHeader>Profile · region</TableHeader>
              <TableHeader>Serving</TableHeader>
              <TableHeader>Build</TableHeader>
              <TableHeader>Last seen</TableHeader>
            </TableRow>
          </TableHead>
          <TableBody>
            {instances.map(instance => (
              <TableRow className="data-row" key={instance.instance_id}>
                <DataCell label="Instance" className="cell-primary">{instance.instance_id}</DataCell>
                <DataCell label="Profile · region">
                  {instance.profile}{instance.region ? ` · ${instance.region}` : ''}
                </DataCell>
                <DataCell label="Serving">
                  {instance.serving_version_id ? uuidHint(instance.serving_version_id) : '—'}{' '}
                  {pin && instance.serving_version_id !== pin.served_version_id && <Badge status="behind" />}
                </DataCell>
                <DataCell label="Build">{instance.build}</DataCell>
                <DataCell label="Last seen">
                  {formatDate(instance.last_seen_at)} {instance.stale && <Badge status="stale" />}
                </DataCell>
              </TableRow>
            ))}
          </TableBody>
        </DataGrid>
      )}

      <ConfirmDialog
        open={!!pinning}
        onClose={() => setPinning(null)}
        onConfirm={() => { void confirmPin(); }}
        title="Pin routing version"
        confirmText="Pin"
        problem={mutation.problem}
        submitting={mutation.submitting}
      >
        Serve version {pinning ? uuidHint(pinning.id) : ''} on every gateway instance until you unpin it?
        Routing changes made while pinned are published but not served.
      </ConfirmDialog>
      <ConfirmDialog
        open={unpinning}
        onClose={() => setUnpinning(false)}
        onConfirm={() => { void confirmUnpin(); }}
        title="Unpin routing version"
        confirmText="Unpin"
        problem={mutation.problem}
        submitting={mutation.submitting}
      >
        Serve the newest version again on every gateway instance?
      </ConfirmDialog>
    </>
  );
}
