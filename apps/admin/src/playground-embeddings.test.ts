import { describe, expect, it } from 'vitest';
import type { CatalogDeployment, CatalogRoute } from './api';
import { tokenize } from './code-highlight';
import { cosine, embeddingInputs, embeddingLanguages, embeddingModels, embeddingSnippet, MAX_EMBEDDING_INPUTS } from './playground-embeddings';

const price = {
  id: 'p', currency: 'USD', input_minor_per_token: 5, output_minor_per_token: 0, cached_input_minor_per_token: 0,
  reasoning_minor_per_token: 0, other_minor_per_token: 0, source: 'list', source_version: '1', confidence: 'known' as const,
};
const deployments: CatalogDeployment[] = [
  { id: 'd1', provider_account_id: 'a', adapter_instance_key: 'bedrock', family: 'bedrock', name: 'titan', model: 'amazon.titan-embed-text-v2:0', location: 'us-east-1',
    operations: [{ operation: 'embeddings', capability: { id: 'c1', max_output_tokens: 1 }, price }] },
  { id: 'd2', provider_account_id: 'a', adapter_instance_key: 'bedrock', family: 'bedrock', name: 'cohere', model: 'cohere.embed-v4:0', location: null,
    operations: [{ operation: 'embeddings', capability: { id: 'c2', max_output_tokens: 1 }, price: { ...price, input_minor_per_token: 9 } }] },
];
const route = (id: string, alias: string, deployment: string, priority = 1, operation: CatalogRoute['operation'] = 'embeddings'): CatalogRoute => ({
  id, alias, operation, deployment_id: deployment, deployment_name: deployment === 'd1' ? 'titan' : 'cohere',
  model: deployment === 'd1' ? 'amazon.titan-embed-text-v2:0' : 'cohere.embed-v4:0', family: 'bedrock', weight: 1, priority,
});

describe('playground embeddings', () => {
  it('lists embedding aliases once, named and priced by the preferred deployment', () => {
    const models = embeddingModels([route('r1', 'embed', 'd2', 2), route('r2', 'embed', 'd1', 1), route('r3', 'chat', 'd1', 1, 'chat')], deployments);
    expect(models).toEqual([{ alias: 'embed', model: 'amazon.titan-embed-text-v2:0', deploymentName: 'titan', location: 'us-east-1', deployments: 2, price }]);
  });

  it('embeds one text per non-empty line, up to the request limit', () => {
    expect(embeddingInputs('a\r\n\n b \n')).toEqual({ inputs: ['a', ' b '] });
    expect(embeddingInputs(' \n').error).toBe('Write at least one line to embed.');
    expect(embeddingInputs(Array.from({ length: MAX_EMBEDDING_INPUTS + 1 }, (_, i) => `t${i}`).join('\n')).error).toContain('at most 128');
  });

  it('measures cosine similarity, and none for vectors it cannot compare', () => {
    expect(cosine([1, 0], [1, 0])).toBe(1);
    expect(cosine([1, 0], [0, 2])).toBe(0);
    expect(cosine([1, 0], [1])).toBeNull();
    expect(cosine([0, 0], [1, 1])).toBeNull();
  });

  it('writes highlightable code that keeps hostile text inside its literals', () => {
    const hostile = ['Quote " and \' and ` and \\ and $HOME ${x}', 'line sep'];
    for (const [language] of embeddingLanguages) {
      for (const style of ['client', 'raw'] as const) {
        const code = embeddingSnippet(language, style, 'embed', hostile, 'https://gw.example/');
        expect(code.code).toContain('https://gw.example/api/inference/v1/embeddings'.slice(0, language === 'python' && style === 'client' ? 18 : 50));
        expect(code.code).not.toContain(' ');
        const tokens = tokenize(code.code, code.syntax);
        expect(tokens.map(token => token.text).join('')).toBe(code.code);
      }
    }
    expect(embeddingSnippet('python', 'client', 'embed', [], 'https://gw.example').code).toContain('"The quick brown fox"');
    expect(embeddingSnippet('curl', 'client', 'embed', ['a'], 'https://gw.example').note).toContain('curl is raw HTTP');
    expect(embeddingSnippet('curl', 'raw', 'embed', ['it\'s'], 'https://gw.example').code).toContain(`'\\''`);
  });
});
