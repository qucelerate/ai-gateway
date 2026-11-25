import type { CatalogDeployment, DeployableModel, DeployableModelFilters, DiscoveredModel, DiscoveredModelFilters, ModelEvidence, ProviderFamily } from './api';

// What a provider account can call (Deployed) and what it could use once the
// tenant acts (Deployable), per the architecture docs (https://gateway.qucelerate.com/architecture/), "API
// contract for the tabs". The gateway never creates deployments, accepts
// terms or requests quota: these tabs say what to do, and where.

/** Azure lists the account's own deployments, so every listed entry is a
 * deployment. The others are serverless: a listed model is callable only
 * where the evidence does not say otherwise. */
export type AccountKind = 'deployments' | 'serverless';

export function accountKind(family: ProviderFamily): AccountKind {
  return family === 'azure-openai' || family === 'azure-ai-foundry' ? 'deployments' : 'serverless';
}

export type EvidenceFilter = ModelEvidence['state'] | 'none';

/** What the Deployed tab may show: every deployment on Azure, whatever its
 * evidence; on a serverless account, the models no check or call has
 * refused. */
export function deployedEvidence(kind: AccountKind): EvidenceFilter[] {
  return kind === 'deployments'
    ? ['verified', 'entitled', 'requires_action', 'refused', 'none']
    : ['verified', 'entitled', 'none'];
}

/** Blocked serverless models, which the Deployable tab lists. */
export const blockedEvidence: EvidenceFilter[] = ['requires_action', 'refused'];

/** The Deployed tab's query. `narrow` picks one or more of the evidence
 * states the tab allows; an empty or foreign choice means all of them. */
export function deployedQuery(key: string, kind: AccountKind, narrow: EvidenceFilter[] = []): DiscoveredModelFilters {
  const allowed = deployedEvidence(kind);
  const chosen = narrow.filter(state => allowed.includes(state));
  // Azure's "all" sends no evidence filter: every listed deployment.
  const evidence = chosen.length ? chosen : kind === 'deployments' ? undefined : allowed;
  return { adapter_instance_key: key, state: 'listed', ...(evidence ? { evidence } : {}) };
}

/** The Deployable tab's query for a serverless account. */
export function blockedQuery(key: string): DiscoveredModelFilters {
  return { adapter_instance_key: key, state: 'listed', evidence: blockedEvidence };
}

/** The Deployable tab's query for an Azure account: catalog models no
 * deployment serves. */
export function undeployedQuery(key: string, filters: Omit<DeployableModelFilters, 'adapter_instance_key' | 'deployed'> = {}): DeployableModelFilters {
  return { adapter_instance_key: key, deployed: false, ...filters };
}

export const evidenceLabel: Record<EvidenceFilter, string> = {
  verified: 'Verified',
  entitled: 'Entitled',
  requires_action: 'Needs action',
  refused: 'Refused',
  none: 'Not checked yet',
};

export const evidenceBadge: Record<EvidenceFilter, string> = {
  verified: 'badge-success',
  entitled: 'badge-neutral',
  requires_action: 'badge-danger',
  refused: 'badge-danger',
  none: 'badge-neutral',
};

export function evidenceState(model: Pick<DiscoveredModel, 'evidence'>): EvidenceFilter {
  return model.evidence?.state ?? 'none';
}

const sourceText: Record<ModelEvidence['source'], string> = {
  traffic: 'a request',
  probe: 'a probe',
  control_plane: 'a permission check',
};

/** Where the latest evidence came from, and the provider's code if any. */
export function evidenceDetail(evidence: ModelEvidence | null): string | null {
  if (!evidence) return null;
  const code = evidence.provider_code ?? (evidence.provider_status ? `HTTP ${evidence.provider_status}` : null);
  return `From ${sourceText[evidence.source]}${code ? ` (${code})` : ''}`;
}

/** A console page the tenant acts on. */
export type ConsoleLink = { href: string; label: string };

/** What the UI knows of where an account runs: Bedrock's region, from the
 * locations of its deployments in the catalog. The credential that names
 * an Azure resource or a Google project is write-only, so links to those
 * open the console's own picker. */
export type AccountPlace = { region: string | null };

/** The region a Bedrock account's catalog deployments run in, when they
 * agree on one. */
export function accountRegion(key: string, deployments: Pick<CatalogDeployment, 'adapter_instance_key' | 'location'>[]): string | null {
  const regions = new Set(deployments.filter(item => item.adapter_instance_key === key && item.location).map(item => item.location as string));
  return regions.size === 1 ? [...regions][0] : null;
}

const regionPattern = /^[a-z]{2}(-[a-z]+)+-\d$/;

export function bedrockCatalogLink(region: string | null): ConsoleLink {
  const safe = region && regionPattern.test(region) ? region : null;
  return {
    href: safe
      ? `https://${safe}.console.aws.amazon.com/bedrock/home?region=${safe}#/model-catalog`
      : 'https://console.aws.amazon.com/bedrock/home#/model-catalog',
    label: safe ? `Open the Bedrock model catalog (${safe})` : 'Open the Bedrock model catalog',
  };
}

const awsRoles: ConsoleLink = { href: 'https://console.aws.amazon.com/iam/home#/roles', label: 'Open IAM roles' };
const awsSupport: ConsoleLink = { href: 'https://support.console.aws.amazon.com/support/home', label: 'Open AWS Support' };
const googleIam: ConsoleLink = { href: 'https://console.cloud.google.com/iam-admin/iam', label: 'Open Google Cloud IAM' };
const googleQuotas: ConsoleLink = { href: 'https://console.cloud.google.com/iam-admin/quotas', label: 'Open Google Cloud quotas' };
const googleOrgPolicies: ConsoleLink = { href: 'https://console.cloud.google.com/iam-admin/orgpolicies/vertexai-allowedModels', label: 'Open the Vertex AI model access policy' };
const modelGarden: ConsoleLink = { href: 'https://console.cloud.google.com/vertex-ai/model-garden', label: 'Open Model Garden' };
export const foundryPortal: ConsoleLink = { href: 'https://ai.azure.com/', label: 'Open the Azure AI Foundry portal' };
const azurePortal: ConsoleLink = { href: 'https://portal.azure.com/', label: 'Open the Azure portal' };

/** What a blocked model's evidence means, and what the tenant does. */
export type Action = { means: string; action: string; link: ConsoleLink | null };

export function actionFor(family: ProviderFamily, evidence: ModelEvidence | null, place: AccountPlace): Action | null {
  if (!evidence) return null;
  const aws = family === 'bedrock';
  const google = family === 'gemini';
  const azure = accountKind(family) === 'deployments';
  if (evidence.state === 'requires_action') {
    switch (evidence.reason) {
      case 'permission_missing':
        return {
          means: 'The gateway’s identity lacks the role or permission to call it.',
          action: azure
            ? 'On the resource, under Access control (IAM), grant the gateway’s identity Cognitive Services OpenAI User (OpenAI-format models) or Cognitive Services User.'
            : aws
              ? 'Allow bedrock:InvokeModel and bedrock:InvokeModelWithResponseStream in the role the gateway assumes.'
              : google
                ? 'Grant the gateway’s service account Vertex AI User (roles/aiplatform.user) on the project.'
                : 'Give the gateway’s credential permission to call it.',
          link: azure ? azurePortal : aws ? awsRoles : google ? googleIam : null,
        };
      case 'agreement_required':
        return {
          means: 'The model’s offer terms are not accepted.',
          action: aws
            ? 'Request access to the model in the Bedrock console’s model catalog, which subscribes through AWS Marketplace.'
            : 'Accept the model’s terms with the provider.',
          link: aws ? bedrockCatalogLink(place.region) : google ? modelGarden : null,
        };
      case 'not_entitled':
        return {
          means: 'The provider restricts the model for this account.',
          action: aws ? 'Ask AWS Support or your AWS account team for access.' : 'Contact the provider.',
          link: aws ? awsSupport : null,
        };
      case 'quota_zero':
        return {
          means: 'The project’s request quota for the model is zero.',
          action: 'Request a quota increase for the model (online_prediction_requests_per_base_model, or global_online_prediction_requests_per_base_model on the global endpoint). Enable xAI and Mistral models in Model Garden first.',
          link: googleQuotas,
        };
      case 'enable_in_model_garden':
        return {
          means: 'The project has not enabled the model in Model Garden.',
          action: 'Open the model’s card in Model Garden and enable it, accepting its terms; a project owner may have to. The next call or probe shows whether it worked.',
          link: modelGarden,
        };
      case 'policy_denied':
        return {
          means: 'An organization policy forbids the project to call it.',
          action: 'Ask your organization policy administrator to allow the model in the vertexai.allowedModels policy (publishers/<publisher>/models/<model>:predict). Changes take up to 15 minutes to apply.',
          link: googleOrgPolicies,
        };
      default:
        return { means: 'A check found something to do with the provider.', action: 'Check the model’s access with the provider.', link: null };
    }
  }
  if (evidence.state === 'refused') {
    if (evidence.reason === 'not_found') {
      return {
        means: 'It is not served where the gateway calls it.',
        action: google
          ? 'Use another location or model. Some models are served only at global (Gemini 4 Argon, for one).'
          : 'Use another location or model.',
        link: google ? modelGarden : aws ? bedrockCatalogLink(place.region) : null,
      };
    }
    return {
      means: 'A call was refused for this account.',
      action: aws
        ? 'A vendor’s denial (“not available for this account”) needs your AWS account team; otherwise check the model’s access in the Bedrock console.'
        : 'Check the account’s access to the model with the provider.',
      link: aws ? bedrockCatalogLink(place.region) : null,
    };
  }
  return null;
}

/** The Deployable tab's one action for an Azure catalog model. */
export const createDeployment: Action = {
  means: 'No deployment serves it.',
  action: 'Create a deployment of it in the Azure AI Foundry portal; the next discovery lists it under Deployed.',
  link: foundryPortal,
};

/** When each Azure deployment's model retires, by deployment name, from
 * the account's catalog models that deployments serve. */
export function retirements(models: Pick<DeployableModel, 'deployments' | 'retires_at'>[]): Map<string, string> {
  const byDeployment = new Map<string, string>();
  for (const model of models) {
    if (!model.retires_at) continue;
    for (const name of model.deployments) {
      const known = byDeployment.get(name);
      if (!known || model.retires_at < known) byDeployment.set(name, model.retires_at);
    }
  }
  return byDeployment;
}

/** What an Azure deployment serves, in one line. */
export function servesText(model: Pick<DiscoveredModel, 'deployment'>): string | null {
  const deployment = model.deployment;
  if (!deployment || !deployment.model) return null;
  return [deployment.format, `${deployment.model}${deployment.version ? ` ${deployment.version}` : ''}`].filter(Boolean).join(' · ');
}

const wireApis: Record<string, string> = {
  gemini: 'Gemini’s API',
  anthropic_messages: 'Anthropic’s Messages API (rawPredict)',
  mistral_chat: 'Mistral’s chat API (rawPredict)',
  openai_chat: 'OpenAI chat completions',
};

/** How the gateway calls an Agent Platform model it lists, from its
 * deployment format (the wire API); null for another kind of format. */
export function wireApiText(model: Pick<DiscoveredModel, 'deployment'>): string | null {
  const format = model.deployment?.format;
  return format ? wireApis[format] ?? null : null;
}

/** The Deployed tab's Calls line for a model outside Azure. */
export function callsText(model: Pick<DiscoveredModel, 'availability' | 'deployment' | 'model'>): string {
  if (model.availability === 'profile_only') return 'Only through an inference profile';
  if (model.availability === 'endpoint_only') return 'Not yet: the gateway does not recognize what this endpoint serves';
  const wire = wireApiText(model);
  if (model.availability === 'unknown') return wire ? `Through ${wire}; location not confirmed` : 'Location not confirmed';
  if (wire && model.model.startsWith('endpoints/')) return `Through ${wire} on the endpoint`;
  return wire ? `Through ${wire}` : 'Directly';
}

/** What one of the account's own Google endpoints serves and on what
 * machines: `gemma-3-1b-it 1 on g2-standard-12 + 1 NVIDIA_L4, 1 replica`.
 * Its capacity is the endpoint's minimum replica count. */
export function endpointText(model: Pick<DiscoveredModel, 'deployment'>): string | null {
  const deployment = model.deployment;
  if (!deployment) return null;
  const serves = deployment.model ? `${deployment.model}${deployment.version ? ` ${deployment.version}` : ''}` : null;
  const replicas = deployment.capacity !== null ? `${deployment.capacity} ${deployment.capacity === 1 ? 'replica' : 'replicas'}` : null;
  const text = [serves, deployment.sku ? `on ${deployment.sku}` : null].filter(Boolean).join(' ');
  return [text || null, replicas].filter(Boolean).join(', ') || null;
}

export function skuText(model: Pick<DiscoveredModel, 'deployment'>): string | null {
  const deployment = model.deployment;
  if (!deployment || (!deployment.sku && deployment.capacity === null)) return null;
  return [deployment.sku, deployment.capacity !== null ? `capacity ${deployment.capacity}` : null].filter(Boolean).join(', ');
}
