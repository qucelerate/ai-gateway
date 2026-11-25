---
description: "Where to ask for help, what to include in a report (gateway doctor output, versions, request_id), what never to post, and how to report a vulnerability."
---

# Getting help

QuCelerate AI Gateway is open-source software under the MIT license. There is no commercial support offering, no support contract, and no guaranteed response time. Maintainers and other users help on a best-effort basis through the project's public issue tracker.

## Help yourself first

- [Troubleshooting guide](./troubleshooting.md): symptoms, the commands that diagnose them, and fixes. Start with `gateway doctor`.
- [Error-code reference](./errors.md): every problem `code` the inference and admin APIs return, with its HTTP status, likely causes, what to do, and whether to retry.
- [Troubleshooting overview](../troubleshooting/index.md): a short table of symptoms by HTTP status.

## Where to ask

Open an issue on the GitHub repository, `https://github.com/qucelerate/ai-gateway/issues`. Search the existing issues first; your problem may already have an answer.

Do not report security vulnerabilities in an issue. See [reporting a vulnerability](#reporting-a-vulnerability) below.

## What to include

An issue that includes the following can usually be answered without a round of questions:

1. **`gateway doctor --json` output**, run in the same environment as the gateway (in Docker Compose: `docker compose --profile app exec gateway gateway doctor --json`). It contains no secret values, but it does name hosts, buckets and provider accounts. Review it and redact anything you do not want to make public before you post it.
2. **Gateway version**: the image tag or digest, or `gateway_version` from the doctor output.
3. **Deployment shape**: Docker Compose, Kubernetes or Helm, or a plain binary; the profiles you run (`all-in-one`, or `api`, `worker` and `data-plane`); the PostgreSQL version and whether it is managed; and any reverse proxy or load balancer in front.
4. **The `request_id`** of a failing request. It is in the problem body's `request_id` field and in the `X-Request-Id` response header. Maintainers cannot see your gateway, but the ID lets you find the matching log lines, which you can then share after redacting them.
5. **The full problem document** (status, `code`, `detail`), or the last events of a stream that failed.
6. **Client library and version**, or the exact `curl` command with the key replaced by a placeholder.
7. **Steps to reproduce**: what you ran, what you expected, and what happened instead.

Never paste any of these into an issue:

- API keys, agent tokens, or session cookies.
- Provider credentials: cloud keys, service-account files, client secrets.
- `API_KEY_PEPPER`, `GATEWAY_CREDENTIAL_ENCRYPTION_KEY`, or the OIDC session secrets.
- Prompt or response content, or trace content, that you are not free to share.

If one of these was posted by mistake, revoke or rotate it at once. Deleting the comment is not enough.

## Reporting a vulnerability

Report suspected vulnerabilities privately, as the repository's `SECURITY.md` describes. Use GitHub's private vulnerability reporting: open the repository's **Security** tab and choose **Report a vulnerability**. If private reporting is not available, contact a maintainer directly. Do not open a public issue, pull request or discussion. Include what you can of:

- the affected component (inference API, admin API or UI, a provider adapter, the Helm chart)
- the version or commit
- steps to reproduce
- the impact you expect

Redact real credentials, tenant data and prompts.

Reports are reviewed by the maintainers on a best-effort basis. There is no guaranteed response time and no bug bounty. Fixes land on `main`, and there are no maintained release branches yet. See also [security and privacy](../security/index.md).
