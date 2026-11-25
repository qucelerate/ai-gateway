# ai-gateway Helm chart

QuCelerate AI Gateway ([qucelerate/ai-gateway](https://github.com/qucelerate/ai-gateway),
documentation at <https://gateway.qucelerate.com>): the `api` and `worker`
profiles of one image, a migration Job before every install and upgrade,
and the admin UI. The admin UI and `/api` share one origin,
`https://<host>`, as the UI's cookie sessions and CSRF checks require: the
Ingress sends `/api` and `/healthz` to the API and everything else to the UI.

## Install

The chart is published as an OCI artifact with every release; its version
is the release's, and its images default to that release's public images
(`ghcr.io/qucelerate/ai-gateway`, `ai-gateway-admin` and `ai-gateway-docs`,
tagged with the version).

```sh
kubectl create namespace ai-gateway
# The secret below, then:
helm install ai-gateway oci://ghcr.io/qucelerate/charts/ai-gateway --version X.Y.Z \
  --namespace ai-gateway --set host=gateway.example.com
```

Before installing you need:

- **PostgreSQL** (16 or later), reachable from the cluster, with a database
  and a login role for the gateway. The migration Job applies the schema
  with the same `DATABASE_URL`.
- **The secret** `existingSecret` names (`ai-gateway-secrets`), created in
  the release namespace (below).
- **An ingress controller** and a TLS certificate for `host` in
  `ingress.tlsSecretName` (`ai-gateway-tls`): a Secret you create, or one
  cert-manager keeps (add its `cert-manager.io/cluster-issuer` annotation to
  `ingress.annotations`). The default annotations are ingress-nginx's;
  streamed completions need its unbuffered proxying and long timeouts, so
  give another controller the same.

Images are public, so no `imagePullSecrets` are needed. Each image is
signed with cosign (keyless, by the release workflow) and carries an SBOM
and SLSA provenance:

```sh
cosign verify ghcr.io/qucelerate/ai-gateway:X.Y.Z \
  --certificate-identity-regexp '^https://github.com/qucelerate/ai-gateway/\.github/workflows/release-images\.yml@refs/tags/v' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

## Secret

`ai-gateway-secrets` is created by you and never committed. Its keys
become the gateway's environment:

| Key | Required | Value |
| --- | --- | --- |
| `DATABASE_URL` | yes | `postgres://<user>:<password>@<host>:5432/<db>?sslmode=require` |
| `API_KEY_PEPPER` | yes | random, 32 bytes or more; API keys are stored as digests keyed with it |
| `OIDC_SESSION_HMAC_SECRET` | for sign-in | random, 32 bytes or more, base64url |
| `OIDC_SESSION_ENCRYPTION_KEY` | for sign-in | random, exactly 32 bytes, base64url |
| `GATEWAY_CREDENTIAL_ENCRYPTION_KEY` | to store provider credentials | random 32 bytes, base64url; losing it makes stored credentials unreadable (or use the `GATEWAY_CREDENTIAL_KMS_*` settings) |
| `OIDC_CLIENT_SECRET` | with OIDC, for a confidential client | the identity provider's client secret |

```sh
secret() { openssl rand -base64 32 | tr '+/' '-_' | tr -d '='; }
kubectl -n ai-gateway create secret generic ai-gateway-secrets \
  --from-literal=DATABASE_URL='postgres://...' \
  --from-literal=API_KEY_PEPPER="$(secret)" \
  --from-literal=OIDC_SESSION_HMAC_SECRET="$(secret)" \
  --from-literal=OIDC_SESSION_ENCRYPTION_KEY="$(secret)" \
  --from-literal=GATEWAY_CREDENTIAL_ENCRYPTION_KEY="$(secret)"
```

Changing the pepper invalidates every API key; changing the session keys
signs everyone out.

The chart sets `GATEWAY_PROFILE`, `BIND_ADDR`, `GATEWAY_ORIGIN`
(`https://<host>`), `OIDC_REDIRECT_URI` and `GATEWAY_BUILD` itself.
Everything else non-secret goes in `env`, for example:

```yaml
env:
  RUST_LOG: info
  # ingress-nginx sets X-Forwarded-For; sign-in limits then count clients.
  GATEWAY_TRUST_X_FORWARDED_FOR: "true"
  # OIDC (off by default): see the operations guide (https://gateway.qucelerate.com/operations/), Sign-in.
  OIDC_ENABLED: "true"
  OIDC_ISSUER: https://login.example.com/
  OIDC_CLIENT_ID: ai-gateway
  OIDC_AUDIENCE: ai-gateway
```

## First run

With the session secrets set, local accounts (email and password) are on
and, while no organization exists, the API logs a single-use setup link
that creates the first organization and its owner:

```sh
kubectl -n ai-gateway logs deploy/ai-gateway-api | grep -i setup
```

With OIDC, create an owner invitation instead (`gateway invitation create
--organization <slug> --role owner`, run with the same secret and
`GATEWAY_PROFILE=worker`), as the operations guide describes.

## Values

| Value | Default | What it does |
| --- | --- | --- |
| `host` | `gateway.example.invalid` | The admin UI's and API's host name. Set it. |
| `existingSecret` | `ai-gateway-secrets` | The secret above. |
| `image.repository` / `image.tag` | `ghcr.io/qucelerate/ai-gateway` / appVersion | The gateway image. |
| `adminImage.repository` / `adminImage.tag` | `ghcr.io/qucelerate/ai-gateway-admin` / appVersion | The admin UI image. |
| `imagePullSecrets` | `[]` | Only for a private mirror. |
| `nameOverride` | `""` | Resource name prefix (default `ai-gateway`). |
| `env` | `RUST_LOG`, `OIDC_ENABLED` | Non-secret settings. |
| `region` | `""` | `GATEWAY_REGION`, shown on *Organization → Routing*. |
| `api`, `worker`, `admin`, `migrate` | one replica each | Replicas and resources. |
| `ingress.*` | ingress-nginx, `ai-gateway-tls` | The one-origin Ingress. |
| `cloudIdentity.*` | none | The gateway's own cloud identities (below). |
| `controlPlane.enabled` | `true` | Off to install only a data plane. |
| `dataPlane.*` | off | An isolated inference-only data plane (below). |
| `metrics.*` | off | Prometheus metrics, pod annotations, a PodMonitor. |
| `network.*` | `[::]` | IPv4/IPv6 listening and Service address families. |
| `http3.*` | off | The gateway's own QUIC listener and a UDP Service. |
| `docs.*` | off | The documentation site on a host of its own. |

### Cloud identities

The gateway reaches tenants' clouds as its own identity in each (an AWS
role it assumes, an Entra directory it signs in to, a Google service account
it impersonates). `cloudIdentity` chooses, per cloud:

- **Static**: `cloudIdentity.existingSecret`, created by you like the main
  secret: `BEDROCK_AWS_ACCESS_KEY_ID` and `BEDROCK_AWS_SECRET_ACCESS_KEY`,
  `AZURE_TENANT_ID`, `AZURE_CLIENT_ID` and `AZURE_CLIENT_SECRET`.
- **Federated**: the pod's service-account token
  (`system:serviceaccount:<namespace>:ai-gateway`), projected per cloud with
  its audience and exchanged there (`cloudIdentity.aws.roleArn`,
  `cloudIdentity.azure.*`, `cloudIdentity.google.*`). The cluster's token
  issuer must be public and trusted by the cloud.

The API and worker run as that service account, which has no Kubernetes
API access.

### Data plane

`dataPlane.enabled` adds an isolated data plane: the same image as
`GATEWAY_PROFILE=data-plane`, serving `/api/inference/v1/*`, `/healthz` and
`/readyz` only. It routes from configuration versions read from the primary
and reserves and settles there; it never migrates. It reads its own secret,
`dataPlane.existingSecret` (`ai-gateway-data-plane-secrets`):

| Key | Value |
| --- | --- |
| `DATABASE_URL` | the primary, as the `gateway_data_plane` role |
| `API_KEY_PEPPER` | the control plane's |
| `GATEWAY_CREDENTIAL_ENCRYPTION_KEY` | the control plane's (or its `GATEWAY_CREDENTIAL_KMS_*` settings) |

Create the role once as a database administrator (`CREATE ROLE
gateway_data_plane LOGIN NOINHERIT PASSWORD '…'`); every migration grants it
what a data plane needs. Set `dataPlane.host` to give it its own Ingress; in
another region's cluster, install only the data plane with
`controlPlane.enabled: false`.

### Metrics

`metrics.enabled: true` serves Prometheus metrics on port `metrics.port`
(9464) at `/metrics` from the API, worker and data plane pods. The listener
has no authentication, so no Service or Ingress exposes it. Use the
`prometheus.io/*` pod annotations (`metrics.podAnnotations`) or a
`PodMonitor` (`metrics.podMonitor.enabled`, needs the Prometheus Operator's
CRDs).

### HTTP/3

`http3.enabled: true` runs the gateway's own QUIC listener in the API pods
(UDP `http3.containerPort`, presenting `ingress.tlsSecretName`) and publishes
it with a `<name>-api-h3` Service (LoadBalancer by default) on UDP
`http3.port`. Clients must reach that address on UDP under the host's name.
`http3.altSvc` stays `off` on the main host, which also serves the admin UI.

## Upgrades

`helm upgrade ai-gateway oci://ghcr.io/qucelerate/charts/ai-gateway --version <new>`:
the migration Job (a `pre-upgrade` hook, and an Argo CD `PreSync` hook)
runs first, then the API rolls with no unavailable pods and drains streams
on shutdown. To pin images apart from the chart, set `image.tag` and
`adminImage.tag`.
