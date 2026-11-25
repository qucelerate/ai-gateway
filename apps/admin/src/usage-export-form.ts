import type { ProviderCredential, ProviderFamily, StorageKind, UsageExport, UsageExportInput } from './api';

// Client-side checks matching domain::usage_export, so the form explains a
// mistake before the server refuses it. The server still verifies that the
// connection can write to the destination, which the browser cannot.

export const storageKinds: [StorageKind, string][] = [
  ['s3', 'Amazon S3'],
  ['gcs', 'Google Cloud Storage'],
  ['azure_blob', 'Azure Blob Storage'],
];

/** Which connections can write each kind: an AWS role for S3, an Entra
 * directory for Azure Blob, a Google service account for Cloud Storage. */
const kindFamilies: Record<StorageKind, ProviderFamily[]> = {
  s3: ['bedrock'],
  gcs: ['gemini'],
  azure_blob: ['azure-openai', 'azure-ai-foundry'],
};

/**
 * The connections that may write `kind`. Entra directories and Google
 * service accounts carry a cloud principal, which API keys for the same
 * families never do, so those are left out. A Bedrock role and Bedrock
 * access keys look alike here; the server refuses keys when verifying.
 */
export function connectionsFor(kind: StorageKind, credentials: ProviderCredential[]): ProviderCredential[] {
  return credentials.filter(credential => kindFamilies[kind].includes(credential.family) && (kind === 's3' || credential.cloud_principal !== undefined));
}

/** Whether a refused verification was about the connection, not storage access. */
export function wrongConnection(detail: string): boolean {
  return detail.includes('cannot write to this kind of storage');
}

export type UsageExportFormValues = { kind: StorageKind; connection: string; bucket: string; account: string; region: string; prefix: string };
export type UsageExportFormErrors = Partial<Record<keyof UsageExportFormValues, string>>;

export function formValues(current: UsageExport | null): UsageExportFormValues {
  return current
    ? { kind: current.kind, connection: current.connection, bucket: current.bucket, account: current.account ?? '', region: current.region ?? '', prefix: current.prefix }
    : { kind: 's3', connection: '', bucket: '', account: '', region: '', prefix: '' };
}

const lowerAlnum = (value: string) => /^[a-z0-9]$/.test(value);
const edgesAlnum = (value: string) => lowerAlnum(value[0] ?? '') && lowerAlnum(value[value.length - 1] ?? '');

export function bucketValid(kind: StorageKind, value: string): boolean {
  switch (kind) {
    case 's3':
      return value.length >= 3 && value.length <= 63 && /^[a-z0-9.-]+$/.test(value) && edgesAlnum(value)
        && !/\.\.|\.-|-\./.test(value) && !/^[0-9.]+$/.test(value) && !value.startsWith('xn--');
    case 'gcs':
      return value.length >= 3 && value.length <= 222 && /^[a-z0-9._-]+$/.test(value) && edgesAlnum(value)
        && (value.length <= 63 || value.split('.').every(part => part.length <= 63))
        && !value.startsWith('goog') && !value.includes('google');
    case 'azure_blob':
      return value.length >= 3 && value.length <= 63 && /^[a-z0-9-]+$/.test(value) && edgesAlnum(value) && !value.includes('--');
  }
}

export const regionValid = (value: string) => /^[a-z]+(-[a-z]+){1,2}-[0-9]+$/.test(value);
export const accountValid = (value: string) => /^[a-z0-9]{3,24}$/.test(value);
export const prefixValid = (value: string) =>
  value === '' || (value.length <= 512 && /^[A-Za-z0-9_.=/-]+$/.test(value) && value.split('/').every(segment => segment !== '' && segment !== '.' && segment !== '..'));

const bucketRules: Record<StorageKind, string> = {
  s3: 'Enter an S3 bucket name: 3 to 63 lowercase letters, digits, dots and hyphens.',
  gcs: 'Enter a Cloud Storage bucket name: lowercase letters, digits, dots, hyphens and underscores, without "google".',
  azure_blob: 'Enter a container name: 3 to 63 lowercase letters, digits and single hyphens.',
};

export function usageExportInput(values: UsageExportFormValues): { input: UsageExportInput } | { errors: UsageExportFormErrors } {
  const connection = values.connection.trim();
  const bucket = values.bucket.trim();
  const account = values.account.trim();
  const region = values.region.trim();
  const prefix = values.prefix.trim();
  const errors: UsageExportFormErrors = {};
  if (!connection) errors.connection = 'Choose the connection that writes the objects.';
  if (!bucketValid(values.kind, bucket)) errors.bucket = bucketRules[values.kind];
  if (values.kind === 's3' && !regionValid(region)) errors.region = 'Enter the bucket’s region, such as eu-west-1.';
  if (values.kind === 'azure_blob' && !accountValid(account)) errors.account = 'Enter the storage account: 3 to 24 lowercase letters and digits.';
  if (!prefixValid(prefix)) errors.prefix = 'Use letters, digits and - _ . = /, without empty, "." or ".." segments or a leading or trailing "/".';
  if (Object.keys(errors).length) return { errors };
  return {
    input: {
      kind: values.kind,
      connection,
      bucket,
      account: values.kind === 'azure_blob' ? account : null,
      region: values.kind === 's3' ? region : null,
      prefix,
    },
  };
}

/** Where the objects go, as the storage service writes it. */
export function destinationLabel(destination: Pick<UsageExportInput, 'kind' | 'bucket' | 'account' | 'region' | 'prefix'>): string {
  const path = destination.prefix ? `/${destination.prefix}/` : '/';
  switch (destination.kind) {
    case 's3':
      return `s3://${destination.bucket}${path} (${destination.region})`;
    case 'gcs':
      return `gs://${destination.bucket}${path}`;
    case 'azure_blob':
      return `https://${destination.account}.blob.core.windows.net/${destination.bucket}${path}`;
  }
}

export type ExportHealth = 'failing' | 'retrying' | 'writing' | 'waiting';

/** How the export is doing, from its last writes: failing after an hour of
 * failures, retrying before that, otherwise writing or not yet written. */
export function exportHealth(current: UsageExport): ExportHealth {
  if (current.failing) return 'failing';
  if (current.failing_since) return 'retrying';
  return current.last_success_at ? 'writing' : 'waiting';
}
