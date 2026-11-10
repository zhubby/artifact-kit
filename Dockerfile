FROM rust:1.95-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY proto ./proto
RUN cargo build --locked --release --bin artifact-control

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates git openssh-client openssl && rm -rf /var/lib/apt/lists/* && useradd --uid 10001 --create-home artifact && mkdir -p /var/lib/artifact && chown 10001:10001 /var/lib/artifact
COPY --from=builder /build/target/release/artifact-control /usr/local/bin/artifact-control
USER 10001:10001
WORKDIR /var/lib/artifact
EXPOSE 8080 8443
ENTRYPOINT ["artifact-control"]
