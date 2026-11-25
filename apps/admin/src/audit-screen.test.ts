import { describe, expect, it } from 'vitest';
import { actionLabel, actorLabel, auditQuery, byColumn, drillInto, isView, targetLabel, toAuditUtc } from './audit-screen';
import type { AuditEvent } from './api';
import { emptyView } from './table-view';

const ACTOR = '123e4567-e89b-12d3-a456-426614174000';

describe('audit filter time conversion', () => {
  it('converts local datetime input and RFC 3339 times to UTC', () => {
    expect(toAuditUtc('2026-01-01T12:30')).toBe(new Date('2026-01-01T12:30').toISOString());
    expect(toAuditUtc('2026-01-01T00:00:00Z')).toBe('2026-01-01T00:00:00.000Z');
    expect(toAuditUtc('not a time')).toBeUndefined();
    expect(toAuditUtc(undefined)).toBeUndefined();
  });
});

describe('audit query', () => {
  const view = {
    ...emptyView,
    search: ' api_key ',
    categories: { action: ['api_key.created', 'api_key.revoked'], outcome: ['denied'], actor: [ACTOR], target: ['team-name'] },
    ranges: { occurred: { min: '2026-10-01T00:00:00Z' } },
  };

  it('sends lists comma-separated, one actor, a search and UTC times', () => {
    expect(auditQuery(view)).toEqual({
      action: 'api_key.created,api_key.revoked',
      outcome: 'denied',
      target_type: undefined,
      actor_type: undefined,
      actor: ACTOR,
      // Not an ID, so not sent.
      target_id: undefined,
      q: 'api_key',
      occurred_after: '2026-10-01T00:00:00.000Z',
      occurred_before: undefined,
    });
  });

  it('leaves out one column, for counting that column’s own choices', () => {
    expect(auditQuery(view, 'action').action).toBeUndefined();
    expect(auditQuery(view, 'action').outcome).toBe('denied');
    expect(auditQuery(view, 'occurred').occurred_after).toBeUndefined();
  });
});

describe('drilling into a summary group', () => {
  it('filters to the group’s values and its UTC day, and stops grouping', () => {
    const grouped = { ...emptyView, group: ['outcome', 'occurred'], page: 2 };
    const next = drillInto(grouped, { values: byColumn({ outcome: 'failed', day: '2026-10-29' }), count: 3, first_occurred_at: '', last_occurred_at: '' });
    expect(next.categories).toEqual({ outcome: ['failed'] });
    expect(next.ranges).toEqual({ occurred: { min: '2026-10-29T00:00:00Z', max: '2026-10-29T23:59:59.999Z' } });
    expect(next.group).toEqual([]);
    expect(next.page).toBe(0);
    // A system event has no actor: nothing to filter by.
    expect(drillInto(emptyView, { values: { actor: null }, count: 1, first_occurred_at: '', last_occurred_at: '' }).categories).toEqual({});
  });

  it('keeps a narrower time range already set within the day', () => {
    const within = { ...emptyView, ranges: { occurred: { min: '2026-10-29T09:00:00Z', max: '2026-11-02T00:00:00Z' } }, group: ['occurred'] };
    const next = drillInto(within, { values: { occurred: '2026-10-29' }, count: 1, first_occurred_at: '', last_occurred_at: '' });
    expect(next.ranges.occurred).toEqual({ min: '2026-10-29T09:00:00Z', max: '2026-10-29T23:59:59.999Z' });
  });

  it('labels a day group by the occurred column', () => {
    expect(byColumn({ day: '2026-10-29', actor: null })).toEqual({ occurred: '2026-10-29', actor: null });
  });
});

describe('audit readability', () => {
  it('names actions by resource and what happened', () => {
    expect(actionLabel('provider_credential.set')).toBe('Cloud credential set');
    expect(actionLabel('api_key.revoked')).toBe('API key revoked');
    expect(actionLabel('usage.read')).toBe('Usage viewed');
    expect(actionLabel('webhook_endpoint.rotate_secret')).toBe('Webhook endpoint secret rotated');
    expect(actionLabel('catalog_sync.fetch_requested')).toBe('Catalog sync fetch requested');
    expect(isView('trace.list')).toBe(true);
    expect(isView('trace_content.read')).toBe(true);
    expect(isView('team.created')).toBe(false);
  });

  it('names who acted and what, falling back to a kind and a short ID', () => {
    const event = (actor: Partial<AuditEvent['actor']>, target: Partial<AuditEvent> = {}) => ({
      id: 'e', occurred_at: '', organization_id: 'o', team_id: null, action: 'team.created', outcome: 'succeeded', request_id: null,
      actor: { actor_type: 'user', user_id: '123e4567-e89b-12d3-a456-426614174000', service_account_id: null, ...actor },
      target_type: 'team', target_id: '223e4567-e89b-12d3-a456-426614174000', ...target,
    }) as AuditEvent;
    expect(actorLabel(event({ name: 'Ada Lovelace' }))).toBe('Ada Lovelace');
    expect(actorLabel(event({}))).toBe('Person or agent 123e4567…4000');
    expect(actorLabel(event({ actor_type: 'system', user_id: null }))).toBe('System');
    expect(targetLabel(event({}, { target_name: 'Platform' }))).toBe('Platform');
    expect(targetLabel(event({}, { target_type: 'api_key' }))).toBe('API key 223e4567…4000');
  });
});

