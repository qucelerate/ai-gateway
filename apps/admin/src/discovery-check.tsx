import React, { useEffect, useRef, useState } from 'react';
import { api, newIdempotencyKey, type ApiProblem, type CatalogSyncJob } from './api';
import { isActive } from './discovery-model';
import { ActionError, loadFailed, type SignIn } from './screen-feedback';
import { Button, EmptyState, Message } from './ui';

const POLL_MS = 3_000;
/** How long to follow a check before saying it is taking a while. */
const FOLLOW_MS = 120_000;
const problemOf = (caught: unknown, fallback: string): ApiProblem => (caught as { problem?: ApiProblem }).problem ?? loadFailed(fallback);

/** What an account with nothing deployed shows: whether the gateway has
 * looked yet, is looking, or looked and found nothing. A check that finishes
 * calls `onChecked`, so the view reads its models again. */
export function DiscoveryCheck({ organizationId, accountKey, onChecked, ...signIn }: SignIn & {
  organizationId: string;
  accountKey: string;
  onChecked: () => void;
}) {
  // undefined while the latest job loads; null when there is none.
  const [job, setJob] = useState<CatalogSyncJob | null | undefined>(undefined);
  // Reading the latest job failed: not the same as never having checked.
  const [loadProblem, setLoadProblem] = useState<ApiProblem | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [starting, setStarting] = useState(false);
  // Following a running check: a refusal stops it; so does running long.
  const [followProblem, setFollowProblem] = useState<ApiProblem | null>(null);
  const [gaveUp, setGaveUp] = useState(false);
  const [reload, setReload] = useState(0);
  const onCheckedRef = useRef(onChecked);
  onCheckedRef.current = onChecked;

  useEffect(() => {
    let current = true;
    setJob(undefined);
    setLoadProblem(null);
    setFollowProblem(null);
    setGaveUp(false);
    api.listSyncJobs(organizationId, accountKey, { page_size: 1 }).then(
      page => { if (current) setJob(page.items[0] ?? null); },
      caught => { if (current) { setLoadProblem(problemOf(caught, 'Whether this account was checked could not be read.')); setJob(null); } },
    );
    return () => { current = false; };
  }, [organizationId, accountKey, reload]);

  // Follow a queued or running job until it ends.
  const activeId = job && isActive(job) ? job.id : null;
  useEffect(() => {
    if (!activeId) return;
    let current = true;
    const stop = () => { current = false; clearInterval(timer); clearTimeout(deadline); };
    const timer = setInterval(() => {
      api.getSyncJob(organizationId, activeId).then(next => {
        if (!current) return;
        setJob(next);
        if (!isActive(next)) { stop(); onCheckedRef.current(); }
      }, caught => {
        if (!current) return;
        const failure = problemOf(caught, 'The check’s progress could not be read.');
        // A refusal (an ended session, a removed permission) will not change
        // by asking again; anything else might, until the deadline.
        if (failure.status >= 400 && failure.status < 500) { stop(); setFollowProblem(failure); }
      });
    }, POLL_MS);
    const deadline = setTimeout(() => { if (current) { stop(); setGaveUp(true); } }, FOLLOW_MS);
    return stop;
  }, [organizationId, activeId]);

  const check = async () => {
    setProblem(null);
    setStarting(true);
    try {
      const { data } = await api.startSyncJob(organizationId, accountKey, newIdempotencyKey());
      setJob(data);
    } catch (caught) {
      const failure = problemOf(caught, 'The check could not be started. Check your connection and try again.');
      if (failure.code === 'catalog_sync_in_progress') {
        // One is already under way: follow it.
        const page = await api.listSyncJobs(organizationId, accountKey, { page_size: 5 }).catch(() => null);
        const running = page?.items.find(isActive);
        if (running) { setJob(running); return; }
      }
      setProblem(failure);
    } finally {
      setStarting(false);
    }
  };

  if (loadProblem) {
    return (
      <div className="discovery-check-error">
        <ActionError problem={loadProblem} action="read this account’s last check" {...signIn} />
        <Button variant="ghost" onClick={() => setReload(value => value + 1)}>Try again</Button>
      </div>
    );
  }
  if (job === undefined) return <Message kind="info">Loading deployed models…</Message>;
  const action = (label: string) => (
    <>
      <Button onClick={() => void check()} loading={starting}>{label}</Button>
      <ActionError problem={problem} action="check this account" {...signIn} />
    </>
  );
  if (job && isActive(job)) {
    if (followProblem) return <ActionError problem={followProblem} action="follow this check" {...signIn} />;
    if (gaveUp) {
      return <EmptyState title="Still checking" text={`Listing ${accountKey} is taking longer than usual. Discovery shows its progress.`} action={<Button variant="ghost" onClick={() => setReload(value => value + 1)}>Refresh</Button>} />;
    }
    return <EmptyState title="Checking…" text={`The gateway is listing what ${accountKey} offers. This usually takes a few seconds.`} />;
  }
  if (!job) {
    return <EmptyState title="Not checked yet" text={`The gateway has not listed what ${accountKey} offers yet. Models appear here once it has.`} action={action('Check now')} />;
  }
  if (job.status === 'failed') {
    return <EmptyState title="The last check failed" text={`Listing ${accountKey} failed${job.error_code ? ` (${job.error_code})` : ''}. Discovery has the details; check again once the connection is fixed.`} action={action('Check again')} />;
  }
  if (job.status === 'unsupported') {
    return <EmptyState title="Nothing deployed yet" text={`The gateway cannot list what ${accountKey} offers, so its models appear here once a request has called them.`} />;
  }
  return <EmptyState title="Nothing deployed yet" text={`The last check found no model ${accountKey} can serve. Deploy one in your cloud, then check again.`} action={action('Check again')} />;
}
