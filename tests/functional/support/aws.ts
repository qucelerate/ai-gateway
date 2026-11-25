import { execFileSync } from 'node:child_process';

// The tenant's side of Connect a cloud, with the AWS CLI: the stack the
// "Launch in AWS CloudFormation" link would create, from the link itself
// (template, stack name, region and parameters), so it is exactly what the
// gateway asks a tenant for. Credentials are the caller's (AWS_PROFILE).

export type QuickCreate = {
  region: string;
  stackName: string;
  templateUrl: string;
  parameters: Record<string, string>;
};

/** The stack a CloudFormation quick-create link describes. */
export function quickCreate(link: string): QuickCreate {
  const url = new URL(link);
  const region = url.searchParams.get('region');
  // The console keeps the stack's settings after `#/stacks/quickcreate?`.
  const query = new URLSearchParams(url.hash.slice(url.hash.indexOf('?') + 1));
  const stackName = query.get('stackName');
  const templateUrl = query.get('templateURL');
  if (!region || !stackName || !templateUrl) throw new Error(`not a quick-create link: ${link}`);
  const parameters = Object.fromEntries([...query].filter(([name]) => name.startsWith('param_')).map(([name, value]) => [name.slice('param_'.length), value]));
  return { region, stackName, templateUrl, parameters };
}

function aws(region: string, args: string[]): string {
  return execFileSync('aws', [...args, '--region', region, '--output', 'json'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });
}

/** The account the caller's credentials are for. */
export function callerAccount(region: string): string {
  return JSON.parse(aws(region, ['sts', 'get-caller-identity'])).Account;
}

type Stack = { StackStatus: string; Parameters?: { ParameterKey: string; ParameterValue: string }[] };

function describe(stack: QuickCreate): Stack | null {
  try {
    return JSON.parse(aws(stack.region, ['cloudformation', 'describe-stacks', '--stack-name', stack.stackName])).Stacks[0];
  } catch (error) {
    if (String((error as { stderr?: string }).stderr).includes('does not exist')) return null;
    throw error;
  }
}

/** Creates the stack unless it exists with these parameters. A stack made
 * for other parameters (another organization's external ID) is left alone:
 * replacing it would cut that organization off. */
export function ensureStack(stack: QuickCreate): 'created' | 'existing' {
  const current = describe(stack);
  if (current) {
    const parameters = Object.fromEntries((current.Parameters ?? []).map(item => [item.ParameterKey, item.ParameterValue]));
    const differs = Object.entries(stack.parameters).filter(([name, value]) => parameters[name] !== value);
    if (differs.length > 0) {
      throw new Error(`stack ${stack.stackName} exists for other parameters (${differs.map(([name]) => name).join(', ')}); delete it or use another account`);
    }
    if (!/_COMPLETE$/.test(current.StackStatus) || current.StackStatus.startsWith('DELETE') || current.StackStatus.includes('ROLLBACK')) {
      throw new Error(`stack ${stack.stackName} is ${current.StackStatus}`);
    }
    return 'existing';
  }
  aws(stack.region, [
    'cloudformation', 'create-stack',
    '--stack-name', stack.stackName,
    '--template-url', stack.templateUrl,
    '--capabilities', 'CAPABILITY_NAMED_IAM',
    '--parameters', ...Object.entries(stack.parameters).map(([name, value]) => `ParameterKey=${name},ParameterValue=${value}`),
  ]);
  aws(stack.region, ['cloudformation', 'wait', 'stack-create-complete', '--stack-name', stack.stackName]);
  return 'created';
}
