// What a tenant may have to ask its cloud provider for before the gateway
// can call a model: the gateway can only offer what the account may call.

import type { ModelEvidence, ProviderFamily } from './api';
import { docsLink } from './header';

/** The section on requesting model access: the providers guide on the
 * documentation site, or the operations guide in the repository. */
export const modelAccessDocs = docsLink('guide/providers#model-access-on-your-cloud-account');

export type Cloud = 'aws' | 'azure' | 'google' | null;

export function cloudOf(family: ProviderFamily | string): Cloud {
  if (family === 'bedrock') return 'aws';
  if (family === 'azure-openai' || family === 'azure-ai-foundry') return 'azure';
  if (family === 'gemini') return 'google';
  return null;
}

/** A short per-cloud note that access may need a request to the provider;
 * null for a provider without one. */
export function accessNote(family: ProviderFamily | string): string | null {
  switch (cloudOf(family)) {
    case 'aws':
      return 'Some Bedrock models also need access AWS grants on request (your AWS account team or a support case), and new accounts may start with low quotas. Accepting an agreement does not replace that.';
    case 'azure':
      return 'Some Azure models need a registration (Limited Access) or an Azure Marketplace purchase on a paid subscription, and quota in the region, before a deployment can serve them.';
    case 'google':
      return 'Claude models are enabled on their Model Garden card, some models are released behind an allowlist, and partner models’ default quotas may need an increase request in the Google Cloud console.';
    default:
      return null;
  }
}

/** `requires_action` reasons a request to the provider (an agreement,
 * access, quota or enabling the model) may fix; not a missing permission
 * or an organization policy, which the tenant changes itself. */
const providerActions = new Set(['agreement_required', 'not_entitled', 'quota_zero', 'enable_in_model_garden']);

/** Whether evidence is a refusal that a request to the provider may fix. */
export function needsAccessRequest(evidence: Pick<ModelEvidence, 'state' | 'reason'> | null): boolean {
  if (!evidence) return false;
  if (evidence.state === 'requires_action') return evidence.reason !== null && providerActions.has(evidence.reason);
  return evidence.state === 'refused' && evidence.reason !== 'not_found';
}
