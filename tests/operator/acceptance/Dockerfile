ARG BASE_IMAGE=localhost:5001/artifactory/operator:rust-native
FROM ${BASE_IMAGE}
USER root
RUN apt-get update && apt-get install -y --no-install-recommends tini && rm -rf /var/lib/apt/lists/*
COPY fixture /usr/local/bin/fixture
USER 10001:10001
ENTRYPOINT ["/usr/local/bin/fixture"]
