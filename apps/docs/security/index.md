# Security & privacy

This public guide is intentionally general. Your deployment remains responsible for its identity, network, retention, and compliance choices.

## Safe handling

- Use HTTPS and keep bearer credentials in a secret store.
- Never put keys in URLs, source control, browser bundles, or telemetry fields.
- Minimize prompt and embedding input; avoid sending data you do not need to process.
- Redact content and authorization headers from logs.
- Treat model output, SSE data, and problem `detail` text as untrusted.
- Rotate credentials through your deployment's normal process.

## Data boundaries

This site does not collect tenant data, credentials, prompts, vectors, or analytics. Its search runs in your browser over an index built with the site, and sends nothing anywhere; the site sets no cookies and measures no use. It makes no retention or provider-training promise beyond the contract shown here; confirm those policies for the deployment you use.

## Reporting a vulnerability

Report a suspected vulnerability privately through GitHub: open the repository's **Security** tab and choose **Report a vulnerability**. Do not open a public issue, pull request, or discussion for it, and do not include real credentials, tenant data, or prompts in the report.

If private vulnerability reporting is not available to you, report a suspected vulnerability to a maintainer directly, never in an issue or pull request.

The gateway is open-source software under the MIT license, with no commercial support offering. Maintainers review reports on a best-effort basis; there is no guaranteed response time and no bug bounty. The repository's `SECURITY.md` is the authoritative policy.

If a credential has been exposed, see [rollback and incidents](/operations/incidents.md#a-credential-is-exposed).
