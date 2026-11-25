import React, { useEffect, useRef, useState } from 'react';
import {
  allPages,
  api,
  newIdempotencyKey,
  type ApiProblem,
  type AwsInstructions,
  type AzureInstructions,
  type GoogleInstructions,
  type OnboardingSummary,
  type ProviderAccount,
  type ProviderCredential,
  type ProviderFamily,
  type ProviderSecretInput,
  type ReleasePage,
  type Verification,
} from './api';
import {
  accountKeyOk,
  apiKeyCredential,
  apiKeyProblem,
  awsAccountId,
  awsCredential,
  awsRegion,
  azureAccountName,
  azureCredential,
  azureEndpoint,
  azureEndpointOk,
  consentReturn,
  defaultAccountKey,
  endpointName,
  googleCredential,
  googleLocation,
  googleProject,
  needsOperatorAllowance,
  openAiCompatibleCredential,
  openAiCompatibleEndpoint,
  uniqueAccountKey,
  uuid,
  type Cloud,
  type DirectAccount,
} from './connect-cloud-form';
import { AddProvider } from './add-provider';
import { ActionError, ReportError, loadFailed, type SignIn } from './screen-feedback';
import { AppLink, Button, CheckOption, CopyButton, FormField, Select, TabPanel, Tabs, TextInput, buttonClass, Message } from './ui';
import { AccessNote } from './cloud-access-note';

type Props = SignIn & { organizationId: string };

const cloudLabels: Record<Cloud, string> = { anthropic: 'Anthropic', openai: 'OpenAI', aws: 'Amazon Web Services', azure: 'Microsoft Azure', google: 'Google Cloud', 'openai-compatible': 'OpenAI-compatible endpoint' };
const cloudModels: Record<Cloud, string> = {
  anthropic: 'Claude, with an API key from the Anthropic Console',
  openai: 'GPT models, with a project API key',
  aws: 'Amazon Bedrock',
  azure: 'Azure OpenAI or AI Foundry',
  google: 'Gemini on Agent Platform',
  'openai-compatible': 'vLLM, llama.cpp, Ollama or any other /v1 API',
};

/** The server's problem; a response without one keeps its HTTP status and
 * code; status 0 means no response, an unknown outcome. */
export function problemOf(caught: unknown, detail: string): ApiProblem {
  const failure = caught as { problem?: ApiProblem; status?: number; code?: string };
  if (failure.problem) return failure.problem;
  if (typeof failure.status === 'number' && failure.status > 0) {
    return { ...loadFailed(detail), status: failure.status, code: failure.code ?? 'request_failed' };
  }
  return loadFailed(detail);
}

/**
 * Connect a provider: the tenant creates, in its own cloud, what lets the
 * gateway reach its models without a key (instructions rendered for this
 * organization), or enters an Anthropic or OpenAI API key or an
 * OpenAI-compatible endpoint; then the gateway verifies the connection by
 * signing in as it will, and the credential is stored for a provider
 * account.
 */
export function ConnectCloud({ organizationId, ...signIn }: Props) {
  const returned = useRef(consentReturn(window.location.search)).current;
  // A return for another organization waits, unread and in the address,
  // until that organization is selected (switching remounts this screen).
  const elsewhere = Boolean(returned?.organization && returned.organization !== organizationId);
  const consent = elsewhere ? null : returned;
  const [summary, setSummary] = useState<OnboardingSummary | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [attempt, setAttempt] = useState(0);
  const [cloud, setCloud] = useState<Cloud | null>(returned ? 'azure' : null);
  useEffect(() => {
    // The consent return is read once; the address bar is left clean.
    if (consent) window.history.replaceState(window.history.state, '', window.location.pathname);
  }, [consent]);
  useEffect(() => {
    let current = true;
    setProblem(null);
    api.onboarding(organizationId).then(
      value => { if (current) setSummary(value); },
      caught => { if (current) setProblem(problemOf(caught, 'Cloud onboarding could not be loaded.')); },
    );
    return () => { current = false; };
  }, [organizationId, attempt]);
  // API keys need no gateway cloud identity: Anthropic and OpenAI are
  // always offered, even while the onboarding summary loads or if it
  // fails; each cloud only once its operator names one.
  const clouds = summary ? (['aws', 'azure', 'google'] as const).filter(item => summary[item]) : [];
  const offered: Cloud[] = ['anthropic', 'openai', ...clouds, ...(summary?.openai_compatible !== undefined ? ['openai-compatible' as const] : [])];
  return (
    <div className="connect-cloud">
      <div className="section-header">
        <div>
          <h2>Connect a provider</h2>
          <p className="muted">Let the gateway call models in your own accounts. Connect an Anthropic or OpenAI account with an API key, or a cloud account without a long-lived key: you create a role or grant in your cloud. Or connect any endpoint that speaks the OpenAI API, such as a model server you run. The gateway checks the connection before anything is saved.</p>
          {summary && clouds.length === 0 && <p className="muted">Cloud accounts (AWS, Azure, Google Cloud) are offered once this gateway’s operator configures a cloud identity for tenants to trust (GATEWAY_ONBOARDING_*; see the operations guide).</p>}
        </div>
      </div>
      <div className="cloud-choices" role="radiogroup" aria-label="Provider">
        {offered.map(item => (
          <button key={item} type="button" role="radio" aria-checked={cloud === item} className={cloud === item ? 'cloud-choice active' : 'cloud-choice'} onClick={() => setCloud(item)}>
            <strong>{cloudLabels[item]}</strong>
            <span className="muted">{cloudModels[item]}</span>
          </button>
        ))}
      </div>
      {/* Where the cloud choices would be. */}
      {problem && <ReportError problem={problem} what="cloud onboarding" onRetry={() => setAttempt(value => value + 1)} {...signIn} />}
      {!problem && !summary && <Message kind="info">Loading cloud onboarding…</Message>}
      {(cloud === 'anthropic' || cloud === 'openai') && <ConnectDirect key={cloud} account={cloud} organizationId={organizationId} {...signIn} />}
      {cloud === 'aws' && summary?.aws && <ConnectAws organizationId={organizationId} {...signIn} />}
      {elsewhere && (
        <Message kind="info">Microsoft returned consent for another of your organizations. Switch to it to continue connecting Azure there.</Message>
      )}
      {cloud === 'azure' && summary?.azure && (
        <ConnectAzure organizationId={organizationId} consent={consent} {...signIn} />
      )}
      {cloud === 'google' && summary?.google && <ConnectGoogle organizationId={organizationId} gateway={summary.google.service_account} {...signIn} />}
      {cloud === 'openai-compatible' && summary?.openai_compatible && (
        <ConnectOpenAiCompatible organizationId={organizationId} allowed={summary.openai_compatible.private_endpoints} {...signIn} />
      )}
    </div>
  );
}

function ConnectAws({ organizationId, ...signIn }: Props) {
  const [account, setAccount] = useState('');
  const [region, setRegion] = useState('us-east-1');
  const [acceptAgreements, setAcceptAgreements] = useState(false);
  const [batchBucket, setBatchBucket] = useState('');
  const [batchRole, setBatchRole] = useState('');
  const [errors, setErrors] = useState<Record<string, string>>({});
  // Instructions are kept with the inputs they were rendered for, and shown
  // only while those are still the form's.
  const [instructions, setInstructions] = useState<{ for: string; value: AwsInstructions } | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [loading, setLoading] = useState(false);
  const batch = batchBucket.trim() && batchRole.trim() ? { bucket: batchBucket.trim(), serviceRoleArn: batchRole.trim() } : undefined;
  const current = `${awsAccountId(account)}|${awsRegion(region)}|${acceptAgreements}|${batch?.bucket ?? ''}|${batch?.serviceRoleArn ?? ''}`;
  const show = async (event: React.FormEvent) => {
    event.preventDefault();
    const accountId = awsAccountId(account);
    const regionId = awsRegion(region);
    const next: Record<string, string> = {};
    if (!accountId) next.account = 'Enter the 12-digit account ID.';
    if (!regionId) next.region = 'Enter a region such as us-east-1.';
    if (!batch && batchBucket.trim()) next.batchRole = 'Enter the batch service role’s ARN, or clear the bucket.';
    if (!batch && batchRole.trim()) next.batchBucket = 'Enter the staging bucket, or clear the role.';
    setErrors(next);
    if (!accountId || !regionId || Object.keys(next).length) return;
    setProblem(null);
    setLoading(true);
    const requested = `${accountId}|${regionId}|${acceptAgreements}|${batch?.bucket ?? ''}|${batch?.serviceRoleArn ?? ''}`;
    try {
      setInstructions({ for: requested, value: await api.awsOnboarding(organizationId, accountId, regionId, acceptAgreements, batch) });
    } catch (caught) {
      setProblem(problemOf(caught, 'The instructions could not be loaded.'));
    } finally {
      setLoading(false);
    }
  };
  const accountId = awsAccountId(account);
  const shown = instructions?.for === current ? instructions.value : null;
  return (
    <>
      <Step number={1} title="Your AWS account">
        <form className="connect-form" onSubmit={show}>
          <FormField label="Account ID" htmlFor="aws-account" error={errors.account}>
            <TextInput id="aws-account" inputMode="numeric" placeholder="123456789012" value={account} onChange={event => setAccount(event.target.value)} />
          </FormField>
          <FormField label="Region" htmlFor="aws-region" hint="Where you use Bedrock." error={errors.region}>
            <TextInput id="aws-region" value={region} onChange={event => setRegion(event.target.value)} />
          </FormField>
          <CheckOption id="aws-accept-agreements" checked={acceptAgreements} onChange={event => setAcceptAgreements(event.target.checked)}>
            Also let the gateway accept model agreements{' '}
            <span className="muted">
              (an administrator’s choice under Availability; it creates AWS Marketplace subscriptions billed per use). With it on, a model’s first ordinary call can also
              create its subscription, as Bedrock subscribes on first use when the role may; the gateway never offers such a model by itself before its agreement is available.
            </span>
          </CheckOption>
          <details className="connect-more" open={Boolean(batchBucket || batchRole || errors.batchBucket || errors.batchRole)}>
            <summary>Batches (optional)</summary>
            <p className="muted">To run Bedrock batch inference jobs, name the S3 bucket batches are staged in and the service role Bedrock assumes to read and write it. The role can then create, read and stop batch jobs, pass that role to Bedrock, and use that bucket.</p>
            <FormField label="Staging bucket" htmlFor="aws-batch-bucket" error={errors.batchBucket}>
              <TextInput id="aws-batch-bucket" value={batchBucket} spellCheck={false} placeholder="my-gateway-batches" onChange={event => setBatchBucket(event.target.value)} />
            </FormField>
            <FormField label="Batch service role ARN" htmlFor="aws-batch-role" hint="Trusts bedrock.amazonaws.com and may read and write the bucket." error={errors.batchRole}>
              <TextInput id="aws-batch-role" value={batchRole} spellCheck={false} placeholder="arn:aws:iam::123456789012:role/BedrockBatch" onChange={event => setBatchRole(event.target.value)} />
            </FormField>
          </details>
          <div className="dialog-actions"><Button type="submit" loading={loading}>Show instructions</Button></div>
        </form>
        <ActionError problem={problem} action="see the instructions" {...signIn} />
      </Step>
      {shown && accountId && (
        <>
          <Step number={2} title="Create the role in your account">
            <p className="muted">
              The role <code>QucelerateGateway</code> lets only the gateway call Bedrock in your account, and only for this organization (external ID <code>{shown.external_id}</code>).{' '}
              {shown.accept_model_agreements
                ? 'It can invoke and list models, and accept a model’s AWS Marketplace agreement when an administrator asks for it. A model’s first ordinary call can then also create its subscription.'
                : 'It can invoke and list models, nothing else.'}
              {shown.batch_bucket && <> It can also run batch jobs staged in <code>{shown.batch_bucket}</code>.</>}
            </p>
            <p><AccessNote family="bedrock" /></p>
            {shown.launch_url
              ? <p><a className={buttonClass()} href={shown.launch_url} target="_blank" rel="noopener noreferrer">Launch in AWS CloudFormation</a> <span className="muted">Review and create the stack while signed in to account {accountId}.</span></p>
              : <p className="muted">One-click setup is not available in this region's partition: use the AWS CLI or Terraform.</p>}
            <Scripts scripts={[['AWS CLI', shown.cli, 'qucelerate-aws.sh'], ['Terraform', shown.terraform, 'qucelerate-aws.tf']]} />
          </Step>
          <Finish
            {...signIn}
            organizationId={organizationId}
            number={3}
            family="bedrock"
            defaultKey={defaultAccountKey('aws', accountId)}
            credential={awsCredential(accountId, awsRegion(region) ?? region)}
            subject={`role ${shown.role_arn}`}
          />
        </>
      )}
    </>
  );
}

/** What the Azure form held when the person left for Microsoft's consent
 * page, so the flow resumes where it was when Microsoft sends them back. */
const azureDraftKey = 'gateway-connect-azure';
type AzureDraft = { organization: string; family: ProviderFamily; resource: string; endpoint: string | null; batch?: boolean };

function saveAzureDraft(draft: AzureDraft) {
  try { sessionStorage.setItem(azureDraftKey, JSON.stringify(draft)); } catch { /* storage unavailable: the form starts empty on return */ }
}

function takeAzureDraft(organization: string): AzureDraft | null {
  try {
    const raw = sessionStorage.getItem(azureDraftKey);
    if (!raw) return null;
    const draft = JSON.parse(raw) as AzureDraft;
    if (draft.organization !== organization) return null;
    sessionStorage.removeItem(azureDraftKey);
    return (draft.family === 'azure-openai' || draft.family === 'azure-ai-foundry') && typeof draft.resource === 'string' ? draft : null;
  } catch {
    return null;
  }
}

function ConnectAzure({ organizationId, consent, ...signIn }: Props & { consent: ReturnType<typeof consentReturn> }) {
  // Back from consent: the form as it was left.
  const draft = useRef(consent ? takeAzureDraft(organizationId) : null).current;
  const [family, setFamily] = useState<ProviderFamily>(draft?.family ?? 'azure-openai');
  const [resource, setResource] = useState(draft?.resource ?? '');
  const [endpoint, setEndpoint] = useState(draft?.endpoint ?? '');
  const [endpointEdited, setEndpointEdited] = useState(Boolean(draft?.endpoint));
  const [tenant, setTenant] = useState(consent?.tenant ?? '');
  const [batchOn, setBatchOn] = useState(draft?.batch === true);
  // Azure AI Foundry runs no batch jobs.
  const batch = batchOn && family === 'azure-openai';
  const [errors, setErrors] = useState<Record<string, string>>({});
  // Instructions are kept with the service and resource they were rendered
  // for, and shown only while those are still the form's.
  const [instructions, setInstructions] = useState<{ for: string; value: AzureInstructions } | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [loading, setLoading] = useState(false);
  const accountName = azureAccountName(resource);
  useEffect(() => {
    if (accountName && !endpointEdited) setEndpoint(azureEndpoint(accountName, family));
  }, [accountName, family, endpointEdited]);
  const load = async (forFamily: ProviderFamily, forResource: string, forBatch: boolean) => {
    setProblem(null);
    setLoading(true);
    try {
      setInstructions({ for: `${forFamily}|${forResource}|${forBatch}`, value: await api.azureOnboarding(organizationId, forFamily, forResource, forBatch) });
    } catch (caught) {
      setProblem(problemOf(caught, 'The instructions could not be loaded.'));
    } finally {
      setLoading(false);
    }
  };
  useEffect(() => {
    // Resumed after consent: show where the person was.
    if (draft && azureAccountName(draft.resource)) void load(draft.family, draft.resource.trim(), draft.batch === true && draft.family === 'azure-openai');
    // Once, on return.
  }, []);
  const show = (event: React.FormEvent) => {
    event.preventDefault();
    if (!accountName) {
      setErrors({ resource: 'Paste the resource ID from the resource’s JSON view or Properties page.' });
      return;
    }
    setErrors({});
    void load(family, resource.trim(), batch);
  };
  const tenantId = uuid(tenant);
  const endpointOk = azureEndpointOk(endpoint, family);
  const endpointError = endpoint && !endpointOk ? `Use the resource’s ${family === 'azure-ai-foundry' ? 'services.ai.azure.com' : 'openai.azure.com'} or cognitiveservices.azure.com address.` : undefined;
  const shown = instructions?.for === `${family}|${resource.trim()}|${batch}` ? instructions.value : null;
  return (
    <>
      {consent?.error && <Message kind="error">Consent was not granted ({consent.error}). An administrator of your Entra directory must approve the gateway.</Message>}
      {consent?.tenant && <Message kind="success">Consent granted for directory {consent.tenant}.</Message>}
      <Step number={1} title="Your Azure resource">
        <form className="connect-form" onSubmit={show}>
          <FormField label="Service" htmlFor="azure-family">
            <Select id="azure-family" value={family} onChange={event => setFamily(event.target.value as ProviderFamily)}>
              <option value="azure-openai">Azure OpenAI</option>
              <option value="azure-ai-foundry">Azure AI Foundry</option>
            </Select>
          </FormField>
          <FormField label="Resource ID" htmlFor="azure-resource" hint="From the resource’s JSON view or Properties page." error={errors.resource}>
            <TextInput id="azure-resource" value={resource} spellCheck={false} placeholder="/subscriptions/…/providers/Microsoft.CognitiveServices/accounts/…" onChange={event => setResource(event.target.value)} />
          </FormField>
          {/* The endpoint follows from the resource; changing it is the exception. */}
          <details className="connect-more" open={Boolean(endpointError) || endpointEdited}>
            <summary>Endpoint{endpoint && !endpointEdited ? `: ${endpoint}` : ''}</summary>
            <FormField label="Endpoint" htmlFor="azure-endpoint" hint="The resource’s endpoint, as requests will call it. Change it only for a custom domain." error={endpointError}>
              <TextInput id="azure-endpoint" value={endpoint} spellCheck={false} placeholder="https://<resource>.openai.azure.com" onChange={event => { setEndpointEdited(true); setEndpoint(event.target.value); }} />
            </FormField>
          </details>
          {family === 'azure-openai' && (
            <CheckOption id="azure-batch" checked={batchOn} onChange={event => setBatchOn(event.target.checked)}>
              Also let the gateway run batches{' '}
              <span className="muted">(creates a custom role that may upload batch files and create and cancel batch jobs; batches also need a deployment of a batch type).</span>
            </CheckOption>
          )}
          <div className="dialog-actions"><Button type="submit" loading={loading}>Show instructions</Button></div>
        </form>
        <ActionError problem={problem} action="see the instructions" {...signIn} />
      </Step>
      {shown && accountName && (
        <>
          <Step number={2} title="Approve the gateway and give it access">
            <p className="muted">
              First, an administrator of your Microsoft Entra directory approves the gateway’s app (client ID <code>{shown.client_id}</code>). Microsoft sends you back here.
            </p>
            <p>
              <a className={buttonClass()} href={shown.consent_url} onClick={() => saveAzureDraft({ organization: organizationId, family, resource: resource.trim(), endpoint: endpointEdited ? endpoint : null, batch })}>Grant consent in Microsoft Entra</a>
            </p>
            <FormField label="Directory (tenant) ID" htmlFor="azure-tenant" hint="Filled in after consent, or from Microsoft Entra ID → Overview." error={tenant && !tenantId ? 'Enter the directory ID, a UUID.' : undefined}>
              <TextInput id="azure-tenant" value={tenant} onChange={event => setTenant(event.target.value)} placeholder="00000000-0000-0000-0000-000000000000" />
            </FormField>
            <p className="muted">Then grant the gateway’s app <strong>{shown.role}</strong> on the resource{shown.batch ? ', and the batch role the scripts create' : ''}.</p>
            <Scripts scripts={[['Azure CLI', shown.cli, 'qucelerate-azure.sh'], ['Terraform', shown.terraform, 'qucelerate-azure.tf']]} />
            <p><AccessNote family="azure-ai-foundry" /></p>
          </Step>
          {tenantId && endpointOk && (
            <Finish
              {...signIn}
              organizationId={organizationId}
              number={3}
              family={family}
              defaultKey={defaultAccountKey('azure', accountName)}
              credential={azureCredential(tenantId, endpoint, resource)}
              subject={`directory ${tenantId}`}
            />
          )}
        </>
      )}
    </>
  );
}

function ConnectGoogle({ organizationId, gateway, ...signIn }: Props & { gateway: string }) {
  const [project, setProject] = useState('');
  const [location, setLocation] = useState('global');
  const [batchBucket, setBatchBucket] = useState('');
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [instructions, setInstructions] = useState<{ for: string; value: GoogleInstructions } | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [loading, setLoading] = useState(false);
  const show = async (event: React.FormEvent) => {
    event.preventDefault();
    const projectId = googleProject(project);
    const next: Record<string, string> = {};
    if (!projectId) next.project = 'Enter the project ID (lowercase, as in the console’s project picker).';
    if (!googleLocation(location)) next.location = 'Enter global, or a region such as us-central1.';
    setErrors(next);
    if (!projectId || Object.keys(next).length) return;
    setProblem(null);
    setLoading(true);
    try {
      setInstructions({ for: `${projectId}|${batchBucket.trim()}`, value: await api.googleOnboarding(organizationId, projectId, batchBucket.trim() || undefined) });
    } catch (caught) {
      setProblem(problemOf(caught, 'The instructions could not be loaded.'));
    } finally {
      setLoading(false);
    }
  };
  const projectId = googleProject(project);
  const locationId = googleLocation(location);
  const shown = projectId && instructions?.for === `${projectId}|${batchBucket.trim()}` ? instructions.value : null;
  return (
    <>
      <Step number={1} title="Your Google Cloud project">
        <form className="connect-form" onSubmit={show}>
          <FormField label="Project ID" htmlFor="google-project" error={errors.project}>
            <TextInput id="google-project" value={project} placeholder="my-project-123" onChange={event => setProject(event.target.value)} />
          </FormField>
          <FormField label="Location" htmlFor="google-location" hint="global, or a region such as us-central1." error={errors.location}>
            <TextInput id="google-location" value={location} onChange={event => setLocation(event.target.value)} />
          </FormField>
          <details className="connect-more" open={Boolean(batchBucket)}>
            <summary>Batches (optional)</summary>
            <FormField label="Staging bucket" htmlFor="google-batch-bucket" hint="The Cloud Storage bucket batch prediction jobs are staged in. The service account and Agent Platform’s service agent can then read and write it.">
              <TextInput id="google-batch-bucket" value={batchBucket} spellCheck={false} placeholder="my-gateway-batches" onChange={event => setBatchBucket(event.target.value)} />
            </FormField>
          </details>
          <div className="dialog-actions"><Button type="submit" loading={loading}>Show instructions</Button></div>
        </form>
        <ActionError problem={problem} action="see the instructions" {...signIn} />
      </Step>
      {shown && projectId && locationId && (
        <>
          <Step number={2} title="Create the service account and let the gateway use it">
            <p className="muted">
              Creates <code>{shown.service_account}</code> with access to Agent Platform, and lets the gateway (<code>{gateway}</code>) act as it. No key is created.
              {shown.batch_bucket && <> It and Agent Platform’s service agent can also read and write <code>{shown.batch_bucket}</code> for batches.</>}
            </p>
            <p><a className={buttonClass()} href={shown.cloud_shell_url} target="_blank" rel="noopener noreferrer">Open Cloud Shell</a> <span className="muted">Then paste the gcloud script.</span></p>
            <Scripts scripts={[['gcloud', shown.gcloud, 'qucelerate-google.sh'], ['Terraform', shown.terraform, 'qucelerate-google.tf']]} />
            <p><AccessNote family="gemini" /></p>
          </Step>
          <Finish
            {...signIn}
            organizationId={organizationId}
            number={3}
            family="gemini"
            defaultKey={defaultAccountKey('google', projectId)}
            credential={googleCredential(projectId, locationId)}
            subject={`service account ${shown.service_account}`}
          />
        </>
      )}
    </>
  );
}

function ConnectOpenAiCompatible({ organizationId, allowed, ...signIn }: Props & { allowed: string[] }) {
  const [url, setUrl] = useState('');
  const [apiKey, setApiKey] = useState('');
  const endpoint = openAiCompatibleEndpoint(url);
  return (
    <>
      <Step number={1} title="Your endpoint">
        <p className="muted">The base URL that request paths such as <code>chat/completions</code> and <code>models</code> follow, usually ending in <code>/v1</code>.</p>
        <div className="connect-form">
          <FormField label="Base URL" htmlFor="compatible-endpoint" error={url.trim() && !endpoint ? 'Enter an http or https URL with no user name, query or fragment, such as https://models.example.com/v1.' : undefined}>
            <TextInput id="compatible-endpoint" value={url} placeholder="https://models.example.com/v1" list={allowed.length ? 'compatible-allowed' : undefined} onChange={event => setUrl(event.target.value)} />
          </FormField>
          {allowed.length > 0 && (
            <datalist id="compatible-allowed">
              {allowed.map(origin => <option key={origin} value={`${origin}/v1`} />)}
            </datalist>
          )}
          <FormField label="API key" htmlFor="compatible-key" hint="Leave empty if the endpoint takes none. Sent as a bearer token, and stored encrypted.">
            <TextInput id="compatible-key" type="password" autoComplete="off" value={apiKey} onChange={event => setApiKey(event.target.value)} />
          </FormField>
        </div>
        {endpoint && needsOperatorAllowance(endpoint, allowed) && (
          <Message kind="info">
            The gateway reaches endpoints over HTTPS at public addresses. One on a private network or over plain HTTP works only if this gateway’s operator allows its origin
            {allowed.length ? <>; allowed now: {allowed.map((origin, index) => <React.Fragment key={origin}>{index ? ', ' : ' '}<code>{origin}</code></React.Fragment>)}.</> : '.'}
          </Message>
        )}
      </Step>
      {endpoint && (
        <Finish
          {...signIn}
          organizationId={organizationId}
          number={2}
          family="openai-compatible"
          defaultKey={defaultAccountKey('openai-compatible', endpointName(endpoint))}
          credential={openAiCompatibleCredential(endpoint, apiKey)}
          subject={`endpoint ${new URL(endpoint).host}`}
        />
      )}
    </>
  );
}

const directAccounts: Record<DirectAccount, { name: string; console: string; where: string; placeholder: string }> = {
  anthropic: { name: 'Anthropic', console: 'https://console.anthropic.com/settings/keys', where: 'the Anthropic Console, under API keys', placeholder: 'sk-ant-api03-…' },
  openai: { name: 'OpenAI', console: 'https://platform.openai.com/api-keys', where: 'the OpenAI platform, under API keys, for the project to bill', placeholder: 'sk-proj-…' },
};

/** An Anthropic or OpenAI account, reached with an API key. */
function ConnectDirect({ account, organizationId, ...signIn }: Props & { account: DirectAccount }) {
  const [apiKey, setApiKey] = useState('');
  const [touched, setTouched] = useState(false);
  const about = directAccounts[account];
  const problem = apiKeyProblem(account, apiKey);
  return (
    <>
      <Step number={1} title={`Your ${about.name} API key`}>
        <p className="muted">
          Create a key in <a href={about.console} target="_blank" rel="noopener noreferrer">{about.where}</a>. The gateway calls {about.name} with it for every request routed to this account, and {about.name} bills your account.
        </p>
        <div className="connect-form">
          <FormField
            label="API key"
            htmlFor={`${account}-key`}
            hint="Stored encrypted, and never shown again. A key used only by the gateway, with a spend limit set in your account, is recommended."
            error={touched || apiKey ? (problem ?? undefined) : undefined}
          >
            <TextInput id={`${account}-key`} type="password" autoComplete="off" spellCheck={false} required aria-required="true" placeholder={about.placeholder} value={apiKey} onBlur={() => setTouched(true)} onChange={event => setApiKey(event.target.value)} />
          </FormField>
        </div>
      </Step>
      {!problem && (
        <Finish
          {...signIn}
          organizationId={organizationId}
          number={2}
          family={account}
          defaultKey={defaultAccountKey(account, 'main')}
          uniqueDefault
          credential={apiKeyCredential(apiKey)}
          subject={`${about.name} account with this key`}
        />
      )}
    </>
  );
}

function Step({ number, title, children }: { number: number; title: string; children: React.ReactNode }) {
  return (
    <section className="connect-step" aria-labelledby={`connect-step-${number}`}>
      <h3 id={`connect-step-${number}`}><span className="step-number" aria-hidden="true">{number}</span>{title}</h3>
      {children}
    </section>
  );
}

/** Copyable, downloadable scripts, one shown at a time. */
export function Scripts({ scripts }: { scripts: [string, string, string][] }) {
  const [shown, setShown] = useState(0);
  const [label, text, filename] = scripts[shown];
  const download = () => {
    const url = URL.createObjectURL(new Blob([text], { type: 'text/plain' }));
    const link = document.createElement('a');
    link.href = url;
    link.download = filename;
    link.click();
    URL.revokeObjectURL(url);
  };
  return (
    <div className="scripts">
      <Tabs id={`setup-${filename.replace(/[^a-z0-9]+/gi, '-')}`} label="Setup method" tabs={scripts.map(([name], index) => [String(index), name] as const)} value={String(shown)} onChange={next => setShown(Number(next))} />
      <TabPanel id={`setup-${filename.replace(/[^a-z0-9]+/gi, '-')}`} value={String(shown)}>
        <pre className="script" aria-label={label}><code>{text}</code></pre>
      </TabPanel>
      <div className="row-actions">
        <CopyButton text={text} label={`Copy the ${label} script`} />
        <Button variant="ghost" type="button" onClick={download}>Download {filename}</Button>
      </div>
    </div>
  );
}

/** Verify the connection, then store it for a provider account the catalog declares. */
function Finish({ organizationId, number, family, defaultKey: named, uniqueDefault = false, credential, subject, ...signIn }: SignIn & {
  organizationId: string;
  number: number;
  family: ProviderFamily;
  defaultKey: string;
  /** The default name says nothing about the account (an API key's
   * `anthropic-main`), so a name already taken is not this account: suggest
   * a free one. A cloud's default names its account, which the existing
   * account of that name is. */
  uniqueDefault?: boolean;
  credential: ProviderSecretInput;
  subject: string;
}) {
  // Every account name in the catalog when the form opened: an account
  // added here later does not move the suggestion off it.
  const [taken, setTaken] = useState<ReadonlySet<string> | null>(null);
  const defaultKey = uniqueDefault && taken ? uniqueAccountKey(named, taken) : named;
  const [key, setKey] = useState(defaultKey);
  const [keyEdited, setKeyEdited] = useState(false);
  const [verification, setVerification] = useState<Verification | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [verifying, setVerifying] = useState(false);
  const [accounts, setAccounts] = useState<ProviderAccount[] | null>(null);
  const [saved, setSaved] = useState(false);
  // Bumped when Add provider activates a release declaring the account.
  const [reload, setReload] = useState(0);
  const [added, setAdded] = useState<{ summary: string; account: ProviderAccount } | null>(null);
  const body = JSON.stringify([key, family, credential]);
  // What the form holds now, read when a check answers: a result is shown
  // only for the values it checked, never for values edited meanwhile.
  const latest = useRef(body);
  latest.current = body;
  // A changed credential or account is a new check.
  const checked = useRef<string | null>(null);
  useEffect(() => {
    if (!keyEdited) setKey(defaultKey);
  }, [defaultKey, keyEdited]);
  useEffect(() => {
    if (checked.current !== body) {
      setVerification(null);
      setSaved(false);
      setAdded(null);
    }
  }, [body]);
  useEffect(() => {
    let current = true;
    allPages<ProviderAccount, ReleasePage<ProviderAccount>>(token => api.listProviderAccounts(organizationId, { page_size: 100, page_token: token })).then(
      ({ items }) => {
        if (!current) return;
        setTaken(before => before ?? new Set(items.map(account => account.adapter_instance_key)));
        setAccounts(items.filter(account => account.family === family));
      },
      () => { if (current) { setTaken(before => before ?? new Set()); setAccounts([]); } },
    );
    return () => { current = false; };
  }, [organizationId, family, reload]);
  const verify = async () => {
    const requested = body;
    setProblem(null);
    setVerifying(true);
    try {
      checked.current = requested;
      const result = await api.verifyCredential(organizationId, key, family, credential);
      if (latest.current === requested) setVerification(result);
    } catch (caught) {
      checked.current = null;
      if (latest.current === requested) setProblem(problemOf(caught, 'The connection could not be checked.'));
    } finally {
      setVerifying(false);
    }
  };
  // The account just added counts as declared even if reading the catalog
  // again fails, so its connection can still be saved.
  const declared = accounts?.find(account => account.adapter_instance_key === key)
    ?? (added?.account.adapter_instance_key === key ? added.account : null);
  return (
    <Step number={number} title="Check the connection">
      <p className="muted">The gateway signs in to your {subject} exactly as it will for requests, and lists what it can call. Nothing is saved or billed.</p>
      <FormField label="Provider account name" htmlFor="connect-key" hint="How the catalog refers to this connection." error={key && !accountKeyOk(key) ? 'Lowercase letters, digits, dots, dashes and underscores.' : undefined}>
        {accounts && accounts.length > 0
          ? (
            <Select id="connect-key" value={declared ? key : ''} onChange={event => { setKeyEdited(true); setKey(event.target.value || defaultKey); }}>
              <option value="">New: {defaultKey}</option>
              {accounts.map(account => <option key={account.id} value={account.adapter_instance_key}>{account.adapter_instance_key}</option>)}
            </Select>
          )
          : <TextInput id="connect-key" value={key} onChange={event => { setKeyEdited(true); setKey(event.target.value.trim()); }} />}
      </FormField>
      <div className="dialog-actions"><Button onClick={() => void verify()} loading={verifying} disabled={!accountKeyOk(key) || (uniqueDefault && !taken)}>Verify connection</Button></div>
      <ActionError problem={problem} action="verify cloud connections" {...signIn} />
      {verification && <VerificationResult verification={verification} />}
      {added && <Message kind="success">{added.summary}</Message>}
      {verification?.status === 'connected' && (
        declared
          ? <SaveCredential {...signIn} organizationId={organizationId} account={declared} credential={credential} saved={saved} onSaved={() => setSaved(true)} />
          : <AddProvider {...signIn} organizationId={organizationId} accountKey={key} family={family} verification={verification} onAdded={(summary, account) => { setAdded({ summary, account }); setReload(count => count + 1); }} />
      )}
    </Step>
  );
}

export function VerificationResult({ verification }: { verification: Verification }) {
  const kind = verification.status === 'connected' ? 'success' : verification.status === 'failed' ? 'error' : '';
  const models = verification.models ?? [];
  return (
    <Message kind={kind || 'info'} role="status" aria-live="polite">
      <p><strong>{verification.status === 'connected' ? 'Connected.' : verification.status === 'awaiting_approval' ? 'Awaiting the operator’s approval.' : verification.status === 'not_configured' ? 'Not available on this gateway.' : 'Not connected.'}</strong> {verification.message}</p>
      {verification.region && <p className="muted">Region: {verification.region}</p>}
      {models.length > 0 && (
        <details>
          <summary>{verification.model_count ?? models.length} available</summary>
          <ul className="model-list">
            {models.slice(0, 50).map(model => <li key={model.model}><code>{model.model}</code>{model.display_name && model.display_name !== model.model ? ` · ${model.display_name}` : ''}</li>)}
          </ul>
          {models.length > 50 && <p className="muted">and {models.length - 50} more</p>}
        </details>
      )}
    </Message>
  );
}

function SaveCredential({ organizationId, account, credential, saved, onSaved, ...signIn }: SignIn & {
  organizationId: string;
  account: ProviderAccount;
  credential: ProviderSecretInput;
  saved: boolean;
  onSaved: () => void;
}) {
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const attempt = useRef<{ key: string; version: number | null } | null>(null);
  const [checking, setChecking] = useState(false);
  const save = async () => {
    setProblem(null);
    setSubmitting(true);
    try {
      if (!attempt.current) {
        // Replacing a stored credential is guarded by its version.
        const stored = await allPages<ProviderCredential, { items: ProviderCredential[]; next_page_token?: string | null }>(token => api.listCredentials(organizationId, { page_size: 100, page_token: token }));
        const current = stored.items.find(item => item.adapter_instance_key === account.adapter_instance_key) ?? null;
        attempt.current = { key: newIdempotencyKey(), version: current?.version ?? null };
      }
      await api.setCredential(organizationId, account.adapter_instance_key, credential, attempt.current.version, attempt.current.key);
      // Check what the account offers now that it can be reached; Discovery
      // and Availability show the result. Best effort: Check now there
      // starts one too.
      api.startSyncJob(organizationId, account.adapter_instance_key, newIdempotencyKey()).then(() => setChecking(true), () => undefined);
      onSaved();
    } catch (caught) {
      const failure = problemOf(caught, 'The connection could not be saved.');
      // The server answered: the next save is a new request, against the
      // version then stored. Only an unknown outcome keeps its key, so a
      // retry replays a save whose response was lost.
      if (failure.status !== 0) attempt.current = null;
      setProblem(failure);
    } finally {
      setSubmitting(false);
    }
  };
  if (saved) {
    return (
      <>
        <Message kind="success">Saved for <strong>{account.adapter_instance_key}</strong>. Requests to its models now use this connection.</Message>
        <div className="next-steps">
          <h4>Next</h4>
          <ol>
            <li><AppLink to="/admin/policies/budgets">Add a budget</AppLink>: every request is charged against one, and with none every request is refused.</li>
            <li><AppLink to="/admin/identities">Create an API key</AppLink> for the application that calls the model.</li>
            <li>{checking ? 'The gateway is checking what this account offers; ' : ''}<AppLink to="/admin/models/availability">Availability</AppLink> shows which models it can call.</li>
          </ol>
        </div>
      </>
    );
  }
  return (
    <>
      <div className="dialog-actions"><Button onClick={() => void save()} loading={submitting}>Save for {account.adapter_instance_key}</Button></div>
      {problem?.code === 'version_conflict'
        ? <Message kind="error">Someone changed this account’s credential meanwhile. Save again to replace the current one with this connection.</Message>
        : <ActionError problem={problem} action="save provider credentials" {...signIn} />}
    </>
  );
}
