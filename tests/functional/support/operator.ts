import { execFileSync } from 'node:child_process';

// The gateway operator's side: binding a tenant's cloud principal to its
// organization (docs/operations.md, *Binding cloud principals*), which a
// shared gateway identity needs before it acts as that principal. The live
// runs bind on dev with the gateway CLI in its API pod;
// LIVE_OPERATOR_COMMAND replaces the command (words split on spaces).

const OPERATOR = (process.env.LIVE_OPERATOR_COMMAND
  || 'kubectl -n gateway-dev exec deploy/gateway-api -- /usr/local/bin/gateway').split(' ');

/** Binds `principal` (`--azure-tenant ID` or `--google-service-account
 * ADDRESS`) to the organization, as the operator would after confirming
 * the organization controls it. Binding again is harmless. */
export function bindCloudPrincipal(organizationId: string, principal: ['--azure-tenant' | '--google-service-account', string]) {
  const [command, ...args] = OPERATOR;
  execFileSync(command, [...args, 'cloud-principal', 'allow', '--organization', organizationId, ...principal], { stdio: ['ignore', 'inherit', 'inherit'] });
}
