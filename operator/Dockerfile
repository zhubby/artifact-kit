ARG RUST_IMAGE=rust:1.95-bookworm
ARG BASE_IMAGE=debian:bookworm-slim
FROM ${RUST_IMAGE} AS builder
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY operator ./operator
COPY proto ./proto
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/build/target cargo build --locked --release --bin artifactory-operator --bin artifactory-planner-runtime --bin artifactory-source-service --bin artifactory-control --bin artifactory-emissary --bin artifactory-telemetry-service && cp target/release/artifactory-operator target/release/artifactory-planner-runtime target/release/artifactory-source-service target/release/artifactory-control target/release/artifactory-emissary target/release/artifactory-telemetry-service /usr/local/bin/

FROM ${BASE_IMAGE}
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates git git-lfs openssh-client openssl && rm -rf /var/lib/apt/lists/* && useradd --uid 10001 --create-home artifactory
COPY --from=builder /usr/local/bin/artifactory-* /usr/local/bin/
COPY operator/entrypoint.sh /usr/local/bin/entrypoint
USER 10001:10001
ENTRYPOINT ["/usr/local/bin/entrypoint"]
CMD ["operator"]
