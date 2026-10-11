# Grafana with fenecdb-datasource built from this checkout: the frontend with
# webpack, the backend with mage for the image's architecture. Built from the
# repository's root, since the backend takes the Go SDK beside it
# (integrations/go):
#
#   docker build -f integrations/grafana/Dockerfile -t fenecdb-grafana .
#
# monitoring/docker-compose.yml builds it as its Grafana. The plugin is not
# signed: GF_PLUGINS_ALLOW_LOADING_UNSIGNED_PLUGINS=fenecdb-datasource lets
# this Grafana load it (integrations/grafana/README.md says how to sign it).

# ----------------------------------------------------------------- frontend
FROM node:24.21.0-alpine AS frontend
WORKDIR /src/integrations/grafana
COPY integrations/grafana/package.json integrations/grafana/package-lock.json integrations/grafana/.npmrc ./
RUN --mount=type=cache,target=/root/.npm npm ci --no-audit --no-fund --loglevel=error
COPY integrations/grafana/ ./
RUN npm run build

# ------------------------------------------------------------------ backend
FROM golang:1.26.5-alpine AS backend
ENV GOTOOLCHAIN=local CGO_ENABLED=0
RUN --mount=type=cache,target=/root/go/pkg/mod go install github.com/magefile/mage@v1.17.2
WORKDIR /src/integrations/grafana
COPY integrations/go/ /src/integrations/go/
COPY integrations/grafana/go.mod integrations/grafana/go.sum ./
RUN --mount=type=cache,target=/root/go/pkg/mod go mod download
COPY integrations/grafana/Magefile.go integrations/grafana/package.json ./
COPY integrations/grafana/src/plugin.json ./src/
COPY integrations/grafana/pkg/ ./pkg/
ARG TARGETARCH
RUN --mount=type=cache,target=/root/go/pkg/mod --mount=type=cache,target=/root/.cache/go-build \
    case "$TARGETARCH" in arm64) target=build:linuxARM64 ;; *) target=build:linux ;; esac && \
    mage -v "$target"

# ------------------------------------------------------------------ Grafana
FROM grafana/grafana:13.2.2
COPY --from=frontend /src/integrations/grafana/dist/ /var/lib/grafana/plugins/fenecdb-datasource/
COPY --from=backend /src/integrations/grafana/dist/ /var/lib/grafana/plugins/fenecdb-datasource/
ENV GF_PLUGINS_ALLOW_LOADING_UNSIGNED_PLUGINS=fenecdb-datasource
