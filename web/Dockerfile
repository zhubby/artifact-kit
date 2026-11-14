FROM rust:1.95-bookworm AS docs-tools
RUN cargo install --locked --version 0.4.40 mdbook && \
    cargo install --locked --version 0.14.0 mdbook-mermaid

FROM node:22-bookworm-slim AS builder
WORKDIR /app/web
ENV NEXT_TELEMETRY_DISABLED=1 ARTIFACTORY_API_URL=http://control-plane:8080
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY --from=docs-tools /usr/local/cargo/bin/mdbook /usr/local/bin/mdbook
COPY --from=docs-tools /usr/local/cargo/bin/mdbook-mermaid /usr/local/bin/mdbook-mermaid
COPY book.toml /app/book.toml
COPY docs /app/docs
COPY web .
RUN npm run build

FROM node:22-bookworm-slim
WORKDIR /app
ENV NODE_ENV=production NEXT_TELEMETRY_DISABLED=1 HOSTNAME=0.0.0.0 PORT=3000
COPY --from=builder --chown=node:node /app/web/.next/standalone ./
COPY --from=builder --chown=node:node /app/web/.next/static ./.next/static
COPY --from=builder --chown=node:node /app/web/public ./public
USER node
EXPOSE 3000
CMD ["node", "server.js"]
