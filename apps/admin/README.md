# Gateway admin

Phase 1 is a standalone React/Vite shell for the Gateway control plane. It calls only the existing browser session endpoints: `GET /api/admin/v1/me`, `GET /api/admin/v1/organizations`, `POST /api/admin/v1/context`, and `POST /api/admin/v1/logout`; the existing OIDC start endpoint is used only by the sign-in recovery action. All requests use same-origin credentials. Mutating requests copy the `__Host-gateway_csrf` cookie to `X-CSRF-Token`; the server still enforces origin and permission checks.

## Run

```sh
IBM_TELEMETRY_DISABLED=true npm ci
npm run dev
```

The UI is built with IBM's Carbon design system. Carbon's packages run IBM usage telemetry when installed; `IBM_TELEMETRY_DISABLED=true` turns it off, as the Docker build and CI do. The theme (light, dark, or the system's) is chosen in the header and remembered in the browser. Fonts (IBM Plex) are served with the app, since its Content Security Policy allows only its own origin. The build writes `third-party-licenses.md` and `ibm-plex-license.txt` beside `index.html`.

Supported runtime: Node.js >= 22.22.2 and npm >= 10.9.3. The Vite development server is separate from Gateway and proxies `/api/*` to `VITE_GATEWAY_PROXY_TARGET` (default `http://127.0.0.1:3000`):

```sh
VITE_GATEWAY_PROXY_TARGET=http://127.0.0.1:3000 npm run dev
```

This proxy keeps browser requests same-origin so cookie authentication works; direct cross-origin cookie authentication is not supported. `VITE_GATEWAY_PROXY_TARGET` is the internal URL Vite uses to reach Gateway. Separately, the backend's `GATEWAY_ORIGIN` must equal the browser-visible Vite dev origin (for example `http://localhost:5173`) so CSRF Origin validation accepts browser requests. In production, serve this app behind a same-origin reverse proxy that forwards `/api/*` to Gateway and falls back to `index.html` for `/admin/*` history routes. Vite's development proxy is not a production deployment mechanism.

For checks: `npm test` and `npm run build`. Organization selection is now supported through `POST /context`; the UI keeps the last server-confirmed organization and role visible while a selection is pending or fails. Team switching and resource APIs remain out of scope.

Navigation beyond Overview is present as clearly marked, non-functional planned screens. No future API shapes are guessed or called. Session failures, no-membership, loading, server errors, and context selection failures are explicit and actionable.
