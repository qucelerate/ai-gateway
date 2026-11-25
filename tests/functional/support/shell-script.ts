// The commands of a setup script as Connect a cloud shows it: comments
// dropped, backslash-continued lines joined.

export function commands(script: string): string[] {
  const joined = script.replace(/\\\n\s*/g, '');
  return joined
    .split('\n')
    .map(line => line.trim())
    .filter(line => line !== '' && !line.startsWith('#'));
}
