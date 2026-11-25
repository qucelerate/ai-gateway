import { useRef, useState } from 'react';
import { allPages, api, newIdempotencyKey, type ApiProblem, type CatalogManifest, type CatalogRelease } from './api';
import { problemOf } from './connect-cloud';

// Every catalog change is a new release: the active one, read back as its
// manifest, with the change applied; created, then activated at the
// activation version read with it. A release activated in between makes
// activation conflict rather than be silently replaced.

/** A change to a manifest: the manifest changed (the base is a copy the
 * change may modify), or why it cannot be made. `base` is null before any
 * release is active. */
export type CatalogEdit = (base: CatalogManifest | null) => CatalogManifest | { problem: string };

/** The manifest with streaming folded into chat (D-020): a `chat_stream`
 * capability, price or route of a release stored before streaming became a
 * mode of chat is dropped where a `chat` one exists (chat wins, as it does
 * for unary requests), and becomes `chat` where none does. */
export function foldStreaming(manifest: CatalogManifest): CatalogManifest {
  const deployments = manifest.deployments.map(deployment => {
    const hasChat = deployment.capabilities.some(capability => capability.operation === 'chat');
    return {
      ...deployment,
      capabilities: deployment.capabilities.flatMap(capability => capability.operation !== 'chat_stream' ? [capability] : hasChat ? [] : [{ ...capability, operation: 'chat' as const }]),
    };
  });
  const chatPrices = new Set(manifest.prices.filter(price => price.operation === 'chat').map(price => price.deployment));
  const chatRoutes = new Set(manifest.routes.filter(route => route.operation === 'chat').map(route => route.alias));
  return {
    ...manifest,
    deployments,
    prices: manifest.prices.flatMap(price => price.operation !== 'chat_stream' ? [price] : chatPrices.has(price.deployment) ? [] : [{ ...price, operation: 'chat' as const }]),
    routes: manifest.routes.flatMap(route => route.operation !== 'chat_stream' ? [route] : chatRoutes.has(route.alias) ? [] : [{ ...route, operation: 'chat' as const }]),
  };
}

/** The operations to show: `chat_stream`, from a release stored before
 * streaming became a mode of chat, is chat. */
export function shownOperations<T extends { operation: string }>(items: T[]): T[] {
  const chat = items.some(item => item.operation === 'chat');
  return items.flatMap(item => item.operation !== 'chat_stream' ? [item] : chat ? [] : [{ ...item, operation: 'chat' } as T]);
}

/** A release version naming what changed: `2026-10-12T18:04Z <summary>`. */
export function releaseVersion(now: Date, summary: string): string {
  return `${now.toISOString().slice(0, 16)}Z ${summary}`.slice(0, 128);
}

type Attempt = {
  manifest: CatalogManifest;
  activationVersion: number;
  createKey: string;
  activateKey: string;
  release: CatalogRelease | null;
};

export type CatalogChange = {
  /** Applies `edit` to the active release and activates the result. */
  submit: (edit: CatalogEdit) => Promise<CatalogRelease | null>;
  submitting: boolean;
  problem: ApiProblem | null;
  /** Why the edit could not be applied, before anything was sent. */
  invalid: string | null;
  /** Forget a pending attempt: the next submit reads the catalog again. */
  reset: () => void;
};

export function useCatalogChange(organizationId: string): CatalogChange {
  const [submitting, setSubmitting] = useState(false);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [invalid, setInvalid] = useState<string | null>(null);
  const attempt = useRef<Attempt | null>(null);
  const submit = async (edit: CatalogEdit) => {
    setProblem(null);
    setInvalid(null);
    setSubmitting(true);
    try {
      if (!attempt.current) {
        const releases = await allPages<CatalogRelease, Awaited<ReturnType<typeof api.listReleases>>>(token => api.listReleases(organizationId, { page_size: 100, page_token: token }));
        const active = releases.items.find(release => release.active) ?? null;
        const base = active ? foldStreaming(await api.releaseManifest(organizationId, active.id)) : null;
        const result = edit(base ? structuredClone(base) : null);
        if ('problem' in result) {
          setInvalid(result.problem);
          return null;
        }
        attempt.current = {
          manifest: result,
          activationVersion: releases.first.activation_version,
          createKey: newIdempotencyKey(),
          activateKey: newIdempotencyKey(),
          release: null,
        };
      }
      const current = attempt.current;
      current.release ??= await api.createRelease(organizationId, current.manifest, current.createKey);
      await api.activateRelease(organizationId, current.release.id, current.activationVersion, current.activateKey);
      attempt.current = null;
      return current.release;
    } catch (caught) {
      const failure = problemOf(caught, 'The catalog could not be changed.');
      // The server answered: the next submit starts again from the catalog
      // then active (a release created here stays, inactive). Only an
      // unknown outcome keeps its keys, so a retry replays it.
      if (failure.status !== 0) attempt.current = null;
      setProblem(failure);
      return null;
    } finally {
      setSubmitting(false);
    }
  };
  return {
    submit,
    submitting,
    problem,
    invalid,
    reset: () => {
      attempt.current = null;
      setInvalid(null);
    },
  };
}

/** What a failed change shows: a release activated meanwhile is its own case. */
export function conflictMessage(problem: ApiProblem | null): string | null {
  return problem?.code === 'version_conflict' ? 'Another release was activated meanwhile. Try again to build on it.' : null;
}
