import { execFileSync } from 'node:child_process';
import { commands } from './shell-script.js';

// The tenant's side of Connect a cloud for Azure, with the Azure CLI: the
// script the page shows, run as the caller (az's signed-in account), so it
// is exactly what the gateway asks a tenant for. Rerunnable: each role
// assignment that exists is not created again.

function az(args: string[]): string {
  return execFileSync('az', [...args, '-o', 'tsv'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });
}

/** The directory az is signed in to. */
export function callerTenant(): string {
  return az(['account', 'show', '--query', 'tenantId']).trim();
}

function assigned(clientId: string, role: string, scope: string): boolean {
  return az(['role', 'assignment', 'list', '--assignee', clientId, '--role', role, '--scope', scope, '--query', 'length(@)']).trim() !== '0';
}

/** Runs the shown Azure CLI script for `clientId` on `scope`, skipping each
 * role assignment that exists already. Returns the roles it assigned. */
export function runAzScript(script: string, clientId: string, scope: string): string[] {
  const created: string[] = [];
  for (const command of commands(script)) {
    if (!command.startsWith('az ')) throw new Error(`unexpected command in the Azure CLI script: ${command}`);
    const role = command.startsWith('az role assignment create ') ? /--role "([^"]+)"/.exec(command)?.[1] : undefined;
    if (role && assigned(clientId, role, scope)) continue;
    // The script is the gateway's own, shown to the tenant to paste.
    execFileSync('bash', ['-euo', 'pipefail', '-c', `${command} -o none`], { stdio: ['ignore', 'inherit', 'inherit'] });
    if (role) created.push(role);
  }
  return created;
}
