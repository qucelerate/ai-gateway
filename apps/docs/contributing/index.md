# Contribution rules

Contributions should make the public contract clearer without exposing private data.

## Before opening a change

- Keep docs in `apps/docs/` and use concise, direct language.
- Use only repository facts or clearly label scoped guidance.
- Use `https://gateway.example.com`, `$GATEWAY_API_KEY`, opaque fake UUIDs, and fake request IDs in examples.
- Never add tenant data, real keys, prompts copied from users, secrets, analytics, proxy behavior, or an interactive playground.
- Keep examples under `/api/inference/v1/` and update the checked-in OpenAPI copy when the source artifact changes.
- Do not invent vulnerability-reporting contacts, support promises, provider guarantees, or retention claims.

## Accessibility bar

Use one clear page title, ordered headings, descriptive links, semantic fenced code blocks, sufficient contrast, visible keyboard focus, and never rely on color alone. Test narrow viewports and keyboard navigation.

## Validate locally

```sh
cd apps/docs
npm ci
npm run check:contract
npm run build
npx playwright install chromium
npm run check:a11y
```

`check:contract` checks schemas, examples and version links against the OpenAPI; `check:a11y` runs axe (WCAG 2.2 A/AA) on every built page in light and dark mode, and needs Chromium installed once. CI also checks Markdown links (Lychee) and scans for secrets (gitleaks, `.gitleaks.toml`). Every `curl` example must call a path the OpenAPI defines, send its required headers, and send a body that matches its request schema; problem and SSE examples must match their schemas. Keep generated output and dependencies out of commits; the lockfile is intentionally committed.

## Ownership and review

The repository's `.github/CODEOWNERS` names the owners of `apps/docs/`, `docs/` and `SECURITY.md`; GitHub asks them to review every change to those paths. Before a release, its owner reviews the pages that release changes, besides the checks CI runs (links, schemas, examples, secrets, accessibility, version links).

Label anything whose contract may still change with the [stability labels](/guide/concepts.md#stability-labels): a **Status: stable.** or **Status: experimental.** line near the top of its page.

## Feedback

Report a mistake, a gap or an unclear page as an issue in the gateway repository, naming the page and what you expected. Never include credentials, tenant data or prompts. Report a suspected vulnerability as [security & privacy](/security/index.md#reporting-a-vulnerability) says, not as an issue.

## Review checklist

- [ ] Scope and implementation status are explicit.
- [ ] No credentials or sensitive data appear in prose or code.
- [ ] Links resolve in the built site.
- [ ] OpenAPI and fixture facts are not embellished.
- [ ] Keyboard and mobile reading order remain usable.
