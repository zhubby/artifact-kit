import type { Page } from "@playwright/test";
import type {
  Artifact,
  Build,
  Node,
  NodeTelemetry,
  Project,
  Release,
  User,
} from "../../src/lib/types";

const created = "2026-11-10T08:30:00Z";
export const artifact: Artifact = {
  id: "artifact-main",
  path: "dist/DesktopClient.zip",
  size: 35840,
  sha256: "abc12345".repeat(8),
  target: "macos",
  created_at: created,
  project_name: "Desktop Client",
  build_id: "build-main",
};
export const project: Project = {
  id: "project-main",
  name: "Desktop Client",
  repository_url: "https://git.example.com/team/desktop-client.git",
  credential_id: "credential-main",
  pool_ids: ["pool-main"],
  signing_profiles: ["apple-internal"],
  build_count: 1234,
  last_build: { id: "build-main", status: "running", created_at: created },
};
export const repositoryRefs = {
  default_ref: "refs/heads/main",
  branches: [
    { name: "main", ref: "refs/heads/main", oid: "a".repeat(40) },
    { name: "release/v1", ref: "refs/heads/release/v1", oid: "b".repeat(40) },
  ],
  tags: [{ name: "v1.2.0", ref: "refs/tags/v1.2.0", oid: "c".repeat(40) }],
};
export const build: Build = {
  id: "build-main",
  project_id: project.id,
  project_name: project.name,
  requested_ref: "main",
  commit_sha: "9d9c687dc4f55b23539aa5417b703078047efa74",
  status: "running",
  publish_status: "not_requested",
  publish_error: null,
  error: null,
  release_version: null,
  created_at: created,
  finished_at: null,
  jobs: [
    {
      id: "job-macos",
      target: "macos",
      status: "running",
      target_config: {
        os: "darwin",
        arch: "arm64",
        steps: [
          { name: "compile", phase: "build" },
          { name: "verify", phase: "verify" },
        ],
      },
      attempts: [
        {
          id: "attempt-main",
          node_id: "node-main",
          node_name: "Mac Studio",
          status: "running",
          lease_expires_at: "2026-11-10T09:00:00Z",
          error: null,
          started_at: created,
          finished_at: null,
          steps: [
            {
              step_index: 0,
              name: "compile",
              phase: "build",
              status: "succeeded",
              exit_code: 0,
              started_at: created,
              finished_at: "2026-11-10T08:31:00Z",
            },
            {
              step_index: 1,
              name: "verify",
              phase: "verify",
              status: "running",
              exit_code: null,
              started_at: "2026-11-10T08:31:00Z",
              finished_at: null,
            },
          ],
        },
      ],
    },
    {
      id: "job-linux",
      target: "linux",
      status: "queued",
      target_config: {
        os: "linux",
        arch: "amd64",
        steps: [{ name: "compile", phase: "build" }],
      },
      attempts: [],
    },
  ],
  artifacts: [artifact],
  config_yaml:
    "version: 1\n# 原始配置，不翻译\ntargets:\n  macos:\n    os: darwin\n",
};
export const nodes: Node[] = [
  {
    id: "node-main",
    name: "Mac Studio",
    os: "darwin",
    arch: "arm64",
    pool_id: "pool-main",
    pool_name: "default",
    labels: { team: "desktop" },
    capabilities: ["xcode", "codesign", "swift", "git", "node"],
    signing_profiles: ["apple-internal"],
    slots: 2,
    active_jobs: 1,
    draining: false,
    revoked: false,
    online: true,
    last_seen: created,
    certificate_expires_at: "2027-11-10T08:30:00Z",
    diagnostics: {
      emissary_version: "0.1.0",
      buffered_log_bytes: 1024,
      metrics: {
        logical_cpus: 12,
        managed_free_bytes: 73014444032,
        emissary_rss_bytes: 2500000,
      },
      tools: {
        xcode: {
          available: true,
          version: "Xcode 26.0",
          path: "/Applications/Xcode.app",
        },
      },
    },
  },
  {
    id: "node-linux",
    name: "Linux Runner",
    os: "linux",
    arch: "amd64",
    pool_id: "pool-main",
    pool_name: "default",
    labels: {},
    capabilities: ["gcc", "go", "rust", "git"],
    signing_profiles: [],
    slots: 2,
    active_jobs: 0,
    draining: false,
    revoked: false,
    online: false,
    last_seen: created,
    certificate_expires_at: "2027-11-10T08:30:00Z",
    diagnostics: {
      emissary_version: "0.1.0",
      metrics: { logical_cpus: 8, managed_free_bytes: 42949672960 },
    },
  },
];
export const nodeTelemetry: NodeTelemetry = {
  host: {
    node_id: nodes[0].id,
    session_id: "session-main",
    sampled_at: new Date().toISOString(),
    received_at: new Date().toISOString(),
    fresh: true,
    dropped_batches: 0,
    sample: {
      cpu_percent: 24.5,
      memory_total_bytes: 68719476736,
      memory_used_bytes: 17179869184,
      available_memory_bytes: 51539607552,
      build_disk_available_bytes: 73014444032,
      swap_total_bytes: 2147483648,
      swap_used_bytes: 0,
      load_one: 1.25,
      active_attempts: 1,
      disks: [{ device: "disk3s1", mount: "/Volumes/Build", total_bytes: 1099511627776, available_bytes: 73014444032, read_bytes: 123456789, written_bytes: 987654321 }],
      networks: [{ interface: "en0", received_bytes: 12345678, transmitted_bytes: 23456789, receive_errors: 0, transmit_errors: 0 }],
      accelerators: [{ id: "gpu-0", utilization_percent: 12.5, memory_used_bytes: 1073741824 }],
    },
  },
  inventory: {
    sampled_at: created,
    digest: "inventory-main",
    inventory: {
      os: "darwin", arch: "arm64", os_version: "macOS 26.1", kernel_version: "Darwin 25.1",
      cpu_model: "Apple M4 Max", physical_cores: 12, logical_cpus: 12,
      physical_memory_bytes: 68719476736, effective_cpu_limit: 12,
      effective_memory_limit_bytes: 68719476736, container: null, virtualization: null,
      tools: { xcode: { available: true, version: "Xcode 26.0" }, cargo: { available: false, version: null } },
      capabilities: nodes[0].capabilities, signing_profile_refs: ["apple-internal"],
      collectors: { host: "sysinfo" },
      accelerators: [{ id: "gpu-0", vendor: "Apple", model: "Apple GPU", driver: null, memory_total_bytes: 68719476736 }],
    },
  },
};
const release: Release = {
  id: "release-main",
  project_id: project.id,
  project_name: project.name,
  version: "v1.2.0-internal",
  build_id: "build-main",
  commit_sha: build.commit_sha!,
  created_at: created,
  artifact_count: 1,
  artifacts: [artifact],
};

export async function fixtureAPI(page: Page, role: User["role"] = "admin") {
  const events = { streams: 0, writes: [] as string[] };
  await page.route("**/api/v1/**", async (route) => {
    const request = route.request();
    const path = new URL(request.url()).pathname.slice("/api/v1".length);
    if (request.method() !== "GET") {
      events.writes.push(`${request.method()} ${path}`);
      return route.fulfill({
        status: 403,
        json: { error: "UI fixtures do not accept writes" },
      });
    }
    if (path.endsWith("/events")) {
      events.streams++;
      return route.fulfill({
        contentType: "text/event-stream",
        body: `retry: 600000\nevent: snapshot\ndata: ${JSON.stringify(build)}\n\nevent: log\ndata: ${JSON.stringify({ id: 1, attempt_id: "attempt-main", step: "compile", text: "native compilation complete\n原始构建日志\n" })}\n\n`,
      });
    }
    const responses: Record<string, unknown> = {
      "/auth/config": { registration_enabled: true },
      "/auth/me": { id: "user-main", username: "alex", role },
      "/projects": [project],
      [`/projects/${project.id}`]: project,
      [`/projects/${project.id}/refs`]: repositoryRefs,
      "/builds": [
        build,
        {
          ...build,
          id: "build-complete",
          status: "succeeded",
          publish_status: "succeeded",
          finished_at: "2026-11-10T08:32:30Z",
          release_version: release.version,
        },
      ],
      [`/builds/${build.id}`]: build,
      "/nodes": nodes,
      [`/nodes/${nodes[0].id}/telemetry`]: {
        ...nodeTelemetry,
        host: {
          ...nodeTelemetry.host,
          sampled_at: new Date().toISOString(),
          received_at: new Date().toISOString(),
        },
      },
      [`/nodes/${nodes[1].id}/telemetry`]: { host: null, inventory: null },
      [`/nodes/${nodes[0].id}/metadata`]: { node_id: nodes[0].id, startup_id: "startup-main", digest: "a".repeat(64), received_at: created, metadata: { "custom.site": "lab", "tools.cargo": { available: false, version: null } } },
      [`/nodes/${nodes[1].id}/metadata`]: { startup_id: null, digest: null, received_at: null, metadata: {} },
      "/emissary-releases": [],
      "/emissary-updates": [],
      "/overview": {
        projects: 1,
        nodes_online: 1,
        running: 1,
        queued: 2,
        releases: 12,
        builds_today: 42,
      },
      "/releases": [release],
      [`/releases/${release.id}`]: release,
      "/artifacts": [artifact],
      "/pools": [{ id: "pool-main", name: "default" }],
      "/credentials": [
        {
          id: "credential-main",
          name: "Read-only deploy",
          kind: "https",
          created_at: created,
        },
      ],
      "/tokens": [
        {
          id: "token-main",
          name: "Desktop release",
          scopes: ["read", "build"],
          project_ids: [project.id],
          expires_at: "2027-11-10T08:30:00Z",
          revoked: false,
        },
      ],
      "/model-options": [],
      "/model-profiles": [],
      "/ai-workers": [],
      [`/builds/${build.id}/plans`]: {plans: [], runs: [], events: []},
      "/users": [{ id: "user-main", username: "alex", role }],
      "/audit": [
        {
          id: 1,
          actor: "user-main",
          action: "project.created",
          resource_id: project.id,
          created_at: created,
        },
      ],
    };
    return Object.hasOwn(responses, path)
      ? route.fulfill({ json: responses[path] })
      : route.fulfill({
          status: 404,
          json: { error: `No fixture for ${path}` },
        });
  });
  return events;
}

export async function openPreferences(page: Page) {
  const trigger = page.getByRole("button", {
    name: /^(界面偏好|Interface preferences)$/,
  });
  if (await trigger.isVisible()) await trigger.click();
}

export async function closePreferences(page: Page) {
  if (await page.getByRole("dialog").isVisible())
    await page.keyboard.press("Escape");
}
