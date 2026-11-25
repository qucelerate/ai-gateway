import React, { useState } from 'react';
import { Button, FormField, TextInput } from './ui';

const REMEMBERED = 'gateway.sign-in.organization';

/** The organization last signed in to on this browser, if any. A
 * convenience only: storage can be unavailable or empty. */
export function rememberedOrganization(): string {
  try {
    return localStorage.getItem(REMEMBERED) ?? '';
  } catch {
    return '';
  }
}

function remember(slug: string) {
  try {
    if (slug) localStorage.setItem(REMEMBERED, slug);
    else localStorage.removeItem(REMEMBERED);
  } catch {
    /* storage unavailable */
  }
}

/**
 * "Sign in to your organization": the person names their organization, and
 * the gateway sends them to its own identity provider if it has one (its
 * own provider otherwise, so the answer does not reveal which organizations
 * exist). The fallback to email-first sign-in (email-sign-in.tsx).
 */
export function OrganizationSignIn({
  pending,
  onSignIn,
}: {
  pending: boolean;
  onSignIn: (organization: string) => void;
}) {
  const [organization, setOrganization] = useState(rememberedOrganization);
  return (
    <form
      className="organization-sign-in"
      aria-label="Sign in to your organization"
      onSubmit={event => {
        event.preventDefault();
        const slug = organization.trim();
        remember(slug);
        onSignIn(slug);
      }}
    >
      <FormField label="Organization" htmlFor="sign-in-organization" hint="The short name your administrator gave you, for example acme.">
        <TextInput
          id="sign-in-organization"
          value={organization}
          onChange={event => setOrganization(event.target.value)}
          autoComplete="organization"
          autoCapitalize="none"
          spellCheck={false}
          required
        />
      </FormField>
      <Button type="submit" loading={pending}>
        Continue
      </Button>
    </form>
  );
}
