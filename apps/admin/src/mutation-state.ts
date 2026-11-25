import { useCallback, useState } from 'react';
import type { ApiProblem } from './api';

export type MutationResult<R> = { data: R; requestId?: string };

export type MutationState<R> = {
  submitting: boolean;
  problem: ApiProblem | null;
  result: MutationResult<R> | null;
  submit: (run: () => Promise<MutationResult<R>>) => Promise<boolean>;
  clearProblem: () => void;
  clearResult: () => void;
};

export function useMutationState<R>(): MutationState<R> {
  const [submitting, setSubmitting] = useState(false);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [result, setResult] = useState<MutationResult<R> | null>(null);

  const submit = useCallback(async (run: () => Promise<MutationResult<R>>) => {
    setSubmitting(true);
    setProblem(null);
    try {
      const outcome = await run();
      setResult(outcome);
      return true;
    } catch (caught) {
      const problem = (caught as { problem?: ApiProblem }).problem ?? null;
      setProblem(problem);
      setResult(null);
      return false;
    } finally {
      setSubmitting(false);
    }
  }, []);

  const clearProblem = useCallback(() => setProblem(null), []);
  const clearResult = useCallback(() => setResult(null), []);

  return { submitting, problem, result, submit, clearProblem, clearResult };
}

export type SecretReveal = {
  secret: string;
  label: string;
  requestId?: string;
} | null;

export function useSecretReveal() {
  const [secret, setSecret] = useState<SecretReveal>(null);

  const reveal = useCallback((value: string, label: string, requestId?: string) => {
    setSecret({ secret: value, label, requestId });
  }, []);

  const dismiss = useCallback(() => setSecret(null), []);

  return { secret, reveal, dismiss };
}
