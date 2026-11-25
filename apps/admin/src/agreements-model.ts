// Plain words for a Bedrock account's model agreements (D-035).

import type { AgreementModel, AgreementOffer, AgreementRate, AgreementReason, AgreementResult, AgreementState, AgreementStatus } from './api';

export const agreementStatusLabel: Record<AgreementStatus, string> = {
  pending: 'Pending with AWS',
  available: 'Available, not yet called',
  verified: 'Verified',
  refused: 'Refused',
  failed: 'Failed',
};

export const agreementStatusTone: Record<AgreementStatus, string> = {
  pending: 'badge-warning',
  available: 'badge-neutral',
  verified: 'badge-success',
  refused: 'badge-danger',
  failed: 'badge-danger',
};

export const outcomeLabel: Record<AgreementResult['outcome'], string> = {
  accepted: 'Accepted',
  already_accepted: 'Already accepted',
  unconfirmed: 'Sent; AWS has not confirmed',
  failed: 'Failed',
};

/** The status an agreement shows: `available` unverified says so. */
export function statusText(state: Pick<AgreementState, 'status' | 'reason'>): string {
  if (state.status === 'available' && state.reason !== 'not_verified') return 'Available';
  return agreementStatusLabel[state.status];
}

/** Reasons that mean the account lacks access only the provider grants. */
export function needsProviderAccess(reason: AgreementReason | null): boolean {
  return reason === 'access_request_required' || reason === 'agreement_not_available';
}

/** Whether a model can be picked to accept now: only a failed attempt is
 * tried again (a refused one has its agreement; AWS must grant access). */
export function acceptable(model: AgreementModel): boolean {
  if (!model.agreement_required || model.offers_error !== null || model.offers.length === 0) return false;
  return !model.agreement || model.agreement.status === 'failed';
}

/** The offer that would be sent for a model: its only one, else the one the
 * administrator chose (none until they choose). */
export function offerFor(model: AgreementModel, offers: Record<string, string>): AgreementOffer | null {
  if (model.offers.length === 1) return model.offers[0];
  return model.offers.find(offer => offer.offer_id === offers[model.model]) ?? null;
}

/** Drops chosen offers that a model no longer lists (after a reload). */
export function pruneOffers(models: AgreementModel[], offers: Record<string, string>): Record<string, string> {
  const kept: Record<string, string> = {};
  for (const model of models) {
    const chosen = offers[model.model];
    if (chosen && model.offers.some(offer => offer.offer_id === chosen)) kept[model.model] = chosen;
  }
  return kept;
}

/** Whether every rate an offer bills is listed: its region's rates, or the
 * whole card when it was not cut. */
export function allRatesListed(offer: AgreementOffer): boolean {
  return offer.rate_card_region !== null || offer.rate_card.length >= offer.rate_card_total;
}

/** One rate as AWS gives it: its price and unit, untranslated. */
export function rateText(rate: AgreementRate): string {
  return `${rate.dimension}: ${rate.price ?? '?'}${rate.unit ? ` ${rate.unit}` : ''}`;
}

/** A rate card in one line: the first few rates, how many more, and whose
 * they are. */
export function rateSummary(offer: AgreementOffer, shown = 3): string {
  if (offer.rate_card.length === 0) return 'No rates listed';
  const rates = offer.rate_card.slice(0, shown).map(rateText);
  const more = offer.rate_card.length - rates.length;
  const where = offer.rate_card_region
    ? ` (${offer.rate_card_region}’s rates)`
    : offer.rate_card.length < offer.rate_card_total ? ` (the first ${offer.rate_card.length} of ${offer.rate_card_total} rates)` : '';
  return `${rates.join('; ')}${more > 0 ? `; and ${more} more` : ''}${where}`;
}

type Pair = { model: string; offer_id: string };

/** The pairs to accept: each picked model with the offer that would be sent. */
export function acceptPairs(models: AgreementModel[], picked: Set<string>, offers: Record<string, string>): Pair[] {
  return pairsOf(models.filter(model => picked.has(model.model)), offers);
}

/** What *Accept all* sends: every model that may be accepted with an offer
 * to send (its only readable one, or the one chosen). */
export function acceptAllPairs(models: AgreementModel[], offers: Record<string, string>): Pair[] {
  return pairsOf(models, offers);
}

function pairsOf(models: AgreementModel[], offers: Record<string, string>): Pair[] {
  return models.flatMap(model => {
    const offer = acceptable(model) ? offerFor(model, offers) : null;
    return offer ? [{ model: model.model, offer_id: offer.offer_id }] : [];
  });
}
