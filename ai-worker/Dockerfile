FROM node:24-bookworm-slim AS builder
WORKDIR /app/ai-worker
COPY ai-worker/package.json ai-worker/package-lock.json ./
RUN npm ci
COPY ai-worker/tsconfig.json ./
COPY ai-worker/src ./src
RUN npm run build && npm prune --omit=dev

FROM node:24-bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends openssl ca-certificates && rm -rf /var/lib/apt/lists/* && useradd --uid 10001 --create-home artifactory && mkdir -p /var/lib/artifactory-ai-worker && chown 10001:10001 /var/lib/artifactory-ai-worker
WORKDIR /app/ai-worker
COPY --from=builder /app/ai-worker/package.json ./
COPY --from=builder /app/ai-worker/node_modules ./node_modules
COPY --from=builder /app/ai-worker/dist ./dist
COPY --chown=10001:10001 proto /app/proto
USER 10001:10001
ENV ARTIFACTORY_AI_CONFIG=/var/lib/artifactory-ai-worker/worker.json
ENTRYPOINT ["node", "dist/main.js"]
