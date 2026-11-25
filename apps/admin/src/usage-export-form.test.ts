import { describe, expect, it } from 'vitest';
import type { ProviderCredential, UsageExport } from './api';
import { bucketValid, connectionsFor, destinationLabel, exportHealth, formValues, prefixValid, regionValid, usageExportInput, wrongConnection } from './usage-export-form';

const credential = (adapter_instance_key: string, family: ProviderCredential['family'], cloud_principal?: 'approved' | 'pending'): ProviderCredential => ({
  adapter_instance_key, family, key_id: 'k', version: 1, created_at: '2026-10-01T00:00:00Z', rotated_at: '2026-10-01T00:00:00Z',
  ...(cloud_principal ? { cloud_principal } : {}),
});
const exported: UsageExport = {
  id: 'e-1', kind: 's3', connection: 'bedrock-123456789012', bucket: 'usage-lake', account: null, region: 'eu-west-1', prefix: 'gateway',
  enabled_at: '2026-10-27T00:00:00Z', verified_at: '2026-10-27T00:00:00Z', last_success_at: null, failing: false, failing_since: null,
  last_error: null, queued_records: 0, oldest_queued_at: null, version: 1, created_at: '2026-10-27T00:00:00Z', updated_at: '2026-10-27T00:00:00Z',
};

describe('usage export form', () => {
  it('offers only connections that can write the kind of storage', () => {
    const all = [
      credential('bedrock-1', 'bedrock'), credential('openai', 'openai'),
      credential('gemini-sa', 'gemini', 'approved'), credential('gemini-key', 'gemini'),
      credential('azure', 'azure-openai', 'pending'), credential('azure-key', 'azure-openai'), credential('foundry', 'azure-ai-foundry', 'approved'),
    ];
    expect(connectionsFor('s3', all).map(item => item.adapter_instance_key)).toEqual(['bedrock-1']);
    // API keys for Gemini and Azure carry no cloud principal and cannot write storage.
    expect(connectionsFor('gcs', all).map(item => item.adapter_instance_key)).toEqual(['gemini-sa']);
    expect(connectionsFor('azure_blob', all).map(item => item.adapter_instance_key)).toEqual(['azure', 'foundry']);
  });

  it('checks names the way the server does', () => {
    expect(bucketValid('s3', 'usage-lake.eu')).toBe(true);
    expect(bucketValid('s3', 'Usage')).toBe(false);
    expect(bucketValid('s3', '192.168.1.1')).toBe(false);
    expect(bucketValid('s3', 'a..b')).toBe(false);
    expect(bucketValid('gcs', 'usage_lake')).toBe(true);
    expect(bucketValid('gcs', 'my-google-bucket')).toBe(false);
    expect(bucketValid('azure_blob', 'usage-lake')).toBe(true);
    expect(bucketValid('azure_blob', 'usage--lake')).toBe(false);
    expect(regionValid('us-gov-west-1')).toBe(true);
    expect(regionValid('eu-west')).toBe(false);
    expect(prefixValid('')).toBe(true);
    expect(prefixValid('warehouse/date=x')).toBe(true);
    expect(prefixValid('/leading')).toBe(false);
    expect(prefixValid('a/../b')).toBe(false);
  });

  it('sends region only for S3 and account only for Azure', () => {
    const base = { connection: ' c ', bucket: 'usage-lake', account: 'store1', region: 'eu-west-1', prefix: 'p' };
    expect(usageExportInput({ ...base, kind: 's3' })).toEqual({ input: { kind: 's3', connection: 'c', bucket: 'usage-lake', account: null, region: 'eu-west-1', prefix: 'p' } });
    expect(usageExportInput({ ...base, kind: 'azure_blob' })).toEqual({ input: { kind: 'azure_blob', connection: 'c', bucket: 'usage-lake', account: 'store1', region: null, prefix: 'p' } });
    expect(usageExportInput({ ...base, kind: 'gcs' })).toEqual({ input: { kind: 'gcs', connection: 'c', bucket: 'usage-lake', account: null, region: null, prefix: 'p' } });
    const missing = usageExportInput({ ...base, kind: 's3', connection: '', region: '' });
    expect('errors' in missing && Object.keys(missing.errors).sort()).toEqual(['connection', 'region']);
  });

  it('fills the form from the saved export', () => {
    expect(formValues(exported)).toEqual({ kind: 's3', connection: 'bedrock-123456789012', bucket: 'usage-lake', account: '', region: 'eu-west-1', prefix: 'gateway' });
  });

  it('names the destination as the storage service does', () => {
    expect(destinationLabel(exported)).toBe('s3://usage-lake/gateway/ (eu-west-1)');
    expect(destinationLabel({ ...exported, kind: 'gcs', region: null, prefix: '' })).toBe('gs://usage-lake/');
    expect(destinationLabel({ ...exported, kind: 'azure_blob', region: null, account: 'store1' })).toBe('https://store1.blob.core.windows.net/usage-lake/gateway/');
  });

  it('reports failing only after an hour, retrying before', () => {
    expect(exportHealth(exported)).toBe('waiting');
    expect(exportHealth({ ...exported, last_success_at: '2026-10-27T01:00:00Z' })).toBe('writing');
    expect(exportHealth({ ...exported, failing_since: '2026-10-27T02:00:00Z' })).toBe('retrying');
    expect(exportHealth({ ...exported, failing_since: '2026-10-27T02:00:00Z', failing: true })).toBe('failing');
  });

  it('tells a wrong connection from missing storage access', () => {
    expect(wrongConnection('the verification object could not be written: connection cannot write to this kind of storage')).toBe(true);
    expect(wrongConnection('the verification object could not be written: storage denied the write')).toBe(false);
  });
});
