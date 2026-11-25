import { defineConfig, devices } from '@playwright/test';

// Two projects: `stack`, run through run.sh against its own Compose stack
// (which sets GATEWAY_URL), and `live`, run through live.sh against a
// deployed gateway with real sign-in and a real cloud account. `live` changes
// a real organization and AWS account, so it exists only when live.sh asks
// for it (LIVE=1); a bare `playwright test` runs `stack` alone.
const baseURL = process.env.GATEWAY_URL ?? 'http://localhost:18180';
// The deployed gateway has no default: live.sh reads LIVE_GATEWAY_URL from
// .env.live.
const liveURL = process.env.LIVE_GATEWAY_URL;
if (process.env.LIVE === '1' && !liveURL) throw new Error('LIVE_GATEWAY_URL must be set for a live run');

export default defineConfig({
  // The specs share one gateway; each stack spec seeds its own organization.
  fullyParallel: false,
  workers: 1,
  forbidOnly: !!process.env.CI,
  retries: 0,
  timeout: 60_000,
  expect: { timeout: 10_000 },
  reporter: process.env.CI ? [['list'], ['html', { open: 'never' }]] : 'list',
  use: {
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
  },
  projects: [
    { name: 'stack', testDir: './specs', use: { ...devices['Desktop Chrome'], baseURL } },
    ...(process.env.LIVE === '1' ? [{
      name: 'live',
      testDir: './live',
      // Sign-in waits for a person; real clouds answer slower.
      timeout: 15 * 60_000,
      expect: { timeout: 30_000 },
      use: { ...devices['Desktop Chrome'], baseURL: liveURL },
    }] : []),
  ],
});
