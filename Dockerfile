FROM rust:1.95-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY proto ./proto
RUN cargo build --locked --release --bin artifactory-control

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates git git-lfs openssh-client openssl && rm -rf /var/lib/apt/lists/* && useradd --uid 10001 --create-home artifactory && mkdir -p /var/lib/artifactory && chown 10001:10001 /var/lib/artifactory
COPY --from=builder /build/target/release/artifactory-control /usr/local/bin/artifactory-control
USER 10001:10001
WORKDIR /var/lib/artifactory
EXPOSE 8080 8443
ENTRYPOINT ["artifactory-control"]
