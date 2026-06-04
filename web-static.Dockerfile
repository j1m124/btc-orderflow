# Serves a PRE-BUILT www/dist (compiled in CI) with Caddy. Tiny + fast — no
# toolchains in the image. Built and pushed to GHCR by
# .github/workflows/web-image.yml, then deployed by Railway as a Docker image.
#
# For an all-in-one image that compiles the WASM inside Railway instead, use
# web.Dockerfile.
FROM caddy:2-alpine
COPY Caddyfile /etc/caddy/Caddyfile
COPY www/dist /srv
