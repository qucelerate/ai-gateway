// What the fake AWS (fakes/aws.mjs) was asked, for assertions.

export type FakeRequest = {
  method: string;
  path: string;
  /** The access key the request was signed with. */
  signedWith: string | null;
  roleArn?: string | null;
  externalId?: string | null;
  model?: string;
  /** A Bedrock Runtime call: ConverseStream (true) or Converse. */
  stream?: boolean;
};

const base = () => process.env.FAKES_URL ?? 'http://127.0.0.1:14599';

export async function fakeRequests(): Promise<FakeRequest[]> {
  const response = await fetch(`${base()}/__requests`);
  if (!response.ok) throw new Error(`fake AWS answered ${response.status}`);
  return (await response.json()) as FakeRequest[];
}

export async function resetFakes(): Promise<void> {
  await fetch(`${base()}/__reset`, { method: 'POST' });
}
