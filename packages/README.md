# Node packages

Node.js modules live in this directory. Each package keeps its own dependencies, lockfile and build scripts.

- `agent-worker`: Node.js 24 Pi planning worker, built as `artifactory-agent-worker`. It supports both the standalone Poll worker and Kubernetes Runtime jobs.

Run checks from the repository root:

```sh
npm --prefix packages/agent-worker ci
npm --prefix packages/agent-worker run check
npm --prefix packages/agent-worker test
```

Build the worker image with the repository root as its context so that it includes the shared Protobuf definitions:

```sh
docker build -f packages/agent-worker/Dockerfile -t artifactory-agent-worker .
```
