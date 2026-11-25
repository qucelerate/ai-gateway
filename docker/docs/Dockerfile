# The public documentation site (apps/docs), served by unprivileged nginx.
# It holds no tenant data and may be served on any host of its own
# (the architecture docs (https://gateway.qucelerate.com/architecture/), Admin UI and documentation deployment).
#
# The site is built outside the image, from a full git checkout, because
# its "Last updated" dates come from git history, which a build context
# never has (.dockerignore drops .git, and apps/docs too). Build it first,
# then pass it as the `site` context:
#
#   (cd apps/docs && npm ci && npm run build)
#   docker build -f docker/docs/Dockerfile \
#     --build-context site=apps/docs/.vitepress/dist -t gateway-docs docker/docs
#
# NGINX's own unprivileged image, from its GitHub registry, pinned by
# digest (the admin UI image uses the same one).
FROM ghcr.io/nginx/nginx-unprivileged:1.29-alpine@sha256:0c79d56aee561a1d81c63f00eee5fb5fe29279560cdc55e91425133104c7fbe6
COPY nginx.conf /etc/nginx/conf.d/default.conf
COPY security-headers.conf /etc/nginx/security-headers.conf
COPY 30-listen-on-ipv6.sh /docker-entrypoint.d/30-listen-on-ipv6.sh
COPY --from=site . /usr/share/nginx/html
EXPOSE 8080
