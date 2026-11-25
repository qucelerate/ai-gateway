import React from 'react';
import type { ProviderFamily } from './api';
import { accessNote, modelAccessDocs } from './cloud-access';

/** The per-cloud note that access may need a request, linking the docs. */
export function AccessNote({ family, className = 'muted' }: { family: ProviderFamily | string; className?: string }) {
  const note = accessNote(family);
  if (!note) return null;
  return (
    <small className={className}>
      {note}{' '}
      <a className="inline-action" href={modelAccessDocs} target="_blank" rel="noopener noreferrer">Model access on your cloud account</a>
    </small>
  );
}
