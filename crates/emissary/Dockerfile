ARG RUST_IMAGE=rust:1.95-bookworm
ARG BASE_IMAGE=debian:bookworm-slim
FROM ${RUST_IMAGE} AS builder
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY vendor ./vendor
COPY crates ./crates
COPY operator ./operator
COPY proto ./proto
ARG ARTIFACTORY_BUILD_VERSION=0.1.0
ARG ARTIFACTORY_BUILD_COMMIT=development
ENV ARTIFACTORY_BUILD_VERSION=${ARTIFACTORY_BUILD_VERSION} ARTIFACTORY_BUILD_COMMIT=${ARTIFACTORY_BUILD_COMMIT}
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/build/target cargo build --locked --release --bin artifactory-emissary && cp target/release/artifactory-emissary /usr/local/bin/
FROM ${BASE_IMAGE}
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates git git-lfs openssh-client && rm -rf /var/lib/apt/lists/* && useradd --uid 10001 --create-home emissary
COPY --from=builder /usr/local/bin/artifactory-emissary /usr/local/bin/
USER 10001:10001
ENTRYPOINT ["/usr/local/bin/artifactory-emissary"]
