import { defineConfig } from 'vitest/config';
import { loadEnv } from 'vite';
import react from '@vitejs/plugin-react';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);

/** IBM Plex is served with the app, so its license (SIL OFL 1.1) is too. */
const plexLicense = {
  name: 'plex-license',
  generateBundle(this: { emitFile: (file: { type: 'asset'; fileName: string; source: string }) => void }) {
    const source = readFileSync(require.resolve('@ibm/plex-sans/LICENSE.txt'), 'utf8');
    this.emitFile({ type: 'asset', fileName: 'ibm-plex-license.txt', source });
  },
};

export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, '.', '');
  return {
    plugins: [react(), plexLicense],
    // Bundled dependencies' licenses (Carbon is Apache-2.0), served with the
    // app at /third-party-licenses.md.
    build: { license: { fileName: 'third-party-licenses.md' } },
    server: {
      proxy: {
        '/api': {
          target: env.VITE_GATEWAY_PROXY_TARGET || 'http://127.0.0.1:3000',
          changeOrigin: false,
        },
      },
    },
    test: { environment: 'jsdom', setupFiles: ['./src/test-setup.ts'] },
  };
});
