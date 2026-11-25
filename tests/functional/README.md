# Functional suite

Drives the gateway through the admin UI, as a tenant would. Two Playwright
projects:

- `stack` (`specs/`): the built images on a Compose stack with AWS faked.
- `live` (`live/`): a deployed gateway, with real sign-in and real AWS,
  Azure and Google accounts; see *Against a deployed gateway* below.

## On its own stack

The Compose stack is the gateway, admin UI, proxy and PostgreSQL. CI runs it after
the container smoke test (`.github/workflows/container-images.yml`), so the
images are published only if it passes.

```sh
cd tests/functional
npm ci
npx playwright install chromium
./run.sh                 # builds the images, runs the specs, removes the stack
./run.sh --no-build      # with gateway:local and gateway-admin:local as built
KEEP_STACK=1 ./run.sh    # leaves the stack up (http://localhost:18180)
```

`run.sh` writes throwaway secrets to `tests/functional/.env` and runs its own
Compose project, `gateway-functional`, on ports 18180 (UI and APIs), 15532
(PostgreSQL) and 14599 (the fake); `GATEWAY_HTTP_PORT`, `POSTGRES_PORT` and
`FAKES_PORT` change them. Extra arguments go to `playwright test`.

What is not real:

- **Sign-in.** `support/seed.ts` writes an organization, its owner and a
  session to the database, keyed with the stack's session secret, and gives
  the browser its cookies. The OIDC flow itself is covered by the Rust tests.
- **AWS.** `fakes/aws.mjs` answers STS AssumeRole, the Bedrock model listings,
  Converse and ConverseStream (AWS event-stream frames, a word per delta). It shares the gateway container's network, so the gateway
  reaches it on loopback, the only plain-HTTP address it accepts. It records
  each request (`/__requests`): the specs check the role and external ID
  assumed, and that Bedrock was called with the role's credentials.

Each spec seeds its own organization, so specs do not depend on each other or
on order.

### The client libraries

`specs/clients.spec.ts` runs every client's example (`clients/*/examples`)
against the stack: it provisions a tenant through the admin UI (the steps
in `support/aws-tenant.ts`, shared with `connect-aws`), then runs each
example's chat and streamed call with the tenant's API key and checks both
answers. It runs only when `CLIENT_EXAMPLES` names the examples to run, which
`clients/release/build-examples.sh` builds and lists:

```sh
clients/release/build-examples.sh /tmp/examples
CLIENT_EXAMPLES=/tmp/examples/examples.json CLIENT_RESULTS=/tmp/compatibility.json \
  tests/functional/run.sh specs/clients.spec.ts
```

`CLIENT_RESULTS` receives the versioned result: each client's version and
outcome, and the gateway tested (`GATEWAY_UNDER_TEST`). CI runs it against
the published images of every version in `clients/release/gateways.txt`
(`.github/workflows/clients-compatibility.yml`).

## Against a deployed gateway

`live.sh` and the `live/` specs are kept in the development repository only;
public releases do not include them.

```sh
aws sso login --profile <admin-profile>
AWS_PROFILE=<admin-profile> tests/functional/live.sh connect-aws
tests/functional/live.sh connect-azure    # az signed in to the directory
tests/functional/live.sh connect-google   # gcloud as an owner of the project
```

With no spec named, live.sh runs all three. The `live` project exists only
when live.sh runs it (`LIVE=1`), so `npm test` and run.sh never touch a
deployed gateway. A browser window opens; sign in when asked. The session is
kept in `.auth/live.json` (owner-only, ignored by git) until it expires, so
reruns within the hour need no sign-in.

Each spec does, with nothing faked, what a tenant would in Connect a cloud,
running the setup the page shows with its cloud's CLI:

- **AWS** (`connect-aws`): creates the stack the "Launch in AWS
  CloudFormation" link describes. A stack of that name made for other
  parameters (another organization) is left alone and the run fails.
- **Azure** (`connect-azure`): consents to the gateway's app in the directory
  in the browser when `LIVE_AZURE_CONSENT=1` (otherwise the directory is
  taken as consented already), then runs the Azure CLI commands shown for the
  resource, skipping a role assignment that exists. az must be signed in to
  that directory.
- **Google** (`connect-google`): runs the gcloud script shown for the project
  (a service account with Agent Platform access that the gateway may
  impersonate; no key), as shown: the script itself is rerunnable.

Azure and Google connections name a principal (the directory, the service
account) that the gateway's operator must bind to the organization; when
the check reports it awaiting approval, the spec binds it as the operator
would, with `kubectl -n gateway-dev exec deploy/gateway-api --
/usr/local/bin/gateway cloud-principal allow` (`LIVE_OPERATOR_COMMAND`
replaces everything before `cloud-principal`).

Then each spec:

1. verifies the connection (retrying while a new grant propagates);
2. adds the model under its alias at the given prices, unless the account is
   already in the catalog, and saves the connection;
3. adds a monthly organization budget if there is none;
4. creates a service account and key, and calls the model through the alias;
5. revokes the key and disables the service account (one that has made
   requests cannot be deleted), with any a failed run left behind.

What it sets up (the cloud grant, the catalog entry, the connection, the
binding, the budget) stays, so a rerun finds it and goes on. It is not run in
CI: sign-in needs a person.

Settings, with their defaults. The gateway URL and the account, tenant,
resource and project have none and must be set; live.sh reads
`tests/functional/.env.live` (ignored by git), one `NAME=value` per line:

- all: `LIVE_GATEWAY_URL` (required),
  `LIVE_ORGANIZATION` (the only one, if unset), `LIVE_BUDGET` (`25.00`),
  `LIVE_OPERATOR_COMMAND`;
- AWS: `LIVE_AWS_ACCOUNT` (required), `LIVE_AWS_REGION` (us-east-2),
  `LIVE_MODEL` (`us.amazon.nova-micro-v1:0`), `LIVE_ALIAS` (`nova-micro`),
  `LIVE_PRICE_INPUT`/`_OUTPUT`/`_CACHED_INPUT`;
- Azure: `LIVE_AZURE_TENANT` (required), `LIVE_AZURE_SERVICE` (`Azure AI Foundry`),
  `LIVE_AZURE_RESOURCE` (required), `LIVE_AZURE_ENDPOINT`
  (else the page's), `LIVE_AZURE_MODEL` (`gpt-6-luna`), `LIVE_AZURE_ALIAS`
  (`gpt-luna`), `LIVE_AZURE_PRICE_INPUT`/`_OUTPUT`/`_CACHED_INPUT`,
  `LIVE_AZURE_CONSENT`;
- Google: `LIVE_GOOGLE_PROJECT` (required),
  `LIVE_GOOGLE_LOCATION` (`global`), `LIVE_GOOGLE_MODEL` (`gemini-4-argon`),
  `LIVE_GOOGLE_ALIAS` (`gemini-argon`),
  `LIVE_GOOGLE_PRICE_INPUT`/`_OUTPUT`/`_CACHED_INPUT`.

Prices are USD per million tokens.
