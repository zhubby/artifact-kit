import type { Page } from "@playwright/test";
import type {
  Artifact,
  Build,
  Node,
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
const nodes: Node[] = [
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
      agent_version: "0.1.0",
      buffered_log_bytes: 1024,
      metrics: {
        logical_cpus: 12,
        managed_free_bytes: 73014444032,
        agent_heap_bytes: 2500000,
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
      agent_version: "0.1.0",
      metrics: { logical_cpus: 8, managed_free_bytes: 42949672960 },
    },
  },
];
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
      "/auth/me": { id: "user-main", username: "alex", role },
      "/projects": [project],
      [`/projects/${project.id}`]: project,
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
      "/agent-releases": [],
      "/agent-updates": [],
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
