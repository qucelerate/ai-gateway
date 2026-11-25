# The admin UI (apps/admin): a static build served by an unprivileged nginx.
# It must share an origin with the gateway's /api: put both behind one
# reverse proxy or ingress that sends /api/* and /healthz to the gateway and
# everything else here (docker/proxy/nginx.conf does it for Compose).
#
#   docker build -f docker/admin/Dockerfile -t gateway-admin .

# Base images come from their publishers' registries, not Docker Hub, pinned
# by digest (see the gateway's Dockerfile for how to update them).
#
# Docker's official node image, distributed through ECR Public.
ARG NODE_IMAGE=public.ecr.aws/docker/library/node:22.22.2-bookworm-slim@sha256:9f6d5975c7dca860947d3915877f85607946403fc55349f39b4bc3688448bb6e

FROM ${NODE_IMAGE} AS build
WORKDIR /app
COPY apps/admin/package.json apps/admin/package-lock.json ./
# Carbon packages run IBM usage telemetry on install; keep it off.
ENV IBM_TELEMETRY_DISABLED=true
RUN npm ci
COPY apps/admin/ ./
RUN npm run build

# NGINX's own unprivileged image, from its GitHub registry.
FROM ghcr.io/nginx/nginx-unprivileged:1.29-alpine@sha256:0c79d56aee561a1d81c63f00eee5fb5fe29279560cdc55e91425133104c7fbe6
COPY docker/admin/nginx.conf /etc/nginx/conf.d/default.conf
COPY docker/admin/security-headers.conf /etc/nginx/security-headers.conf
COPY docker/admin/30-listen-on-ipv6.sh /docker-entrypoint.d/30-listen-on-ipv6.sh
COPY --from=build /app/dist /usr/share/nginx/html
EXPOSE 8080
# OCI metadata; a CI build adds the revision, version and its repository.
LABEL org.opencontainers.image.title="QuCelerate AI Gateway admin UI" \
      org.opencontainers.image.description="The admin console of the QuCelerate AI Gateway, served by unprivileged nginx" \
      org.opencontainers.image.vendor="QuCelerate" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.url="https://gateway.qucelerate.com" \
      org.opencontainers.image.documentation="https://gateway.qucelerate.com" \
      org.opencontainers.image.source="https://github.com/qucelerate/ai-gateway"
