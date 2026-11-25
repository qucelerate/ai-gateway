import { execFileSync } from 'node:child_process';

// The tenant's side of Connect a cloud for Google, with gcloud: the script
// the page shows, pasted into a shell as the caller (gcloud's active
// account), so it is exactly what the gateway asks a tenant for. The script
// is rerunnable: it creates the service account only if it is missing.

/** Runs the shown gcloud script, exactly as shown, stopping at the first
 * command that fails (the script fails when its retries run out). */
export function runGcloudScript(script: string) {
  // The script is the gateway's own, shown to the tenant to paste.
  execFileSync('bash', ['-euo', 'pipefail', '-c', script], { stdio: ['ignore', 'inherit', 'inherit'] });
}

/** The account gcloud runs as. */
export function activeAccount(): string {
  return execFileSync('gcloud', ['config', 'get', 'account'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim();
}
