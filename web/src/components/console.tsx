"use client";

import { PreferencesControls } from "@/components/preferences-controls";
import { Atmosphere } from "@/components/atmosphere";
import { BrandMark } from "@/components/brand-mark";
import SpotlightCard from "@/components/react-bits/SpotlightCard";
import { useUI } from "@/components/providers";

import Link from "next/link";
import { usePathname, useRouter } from "next/navigation";
import { Suspense, useEffect, useRef, useState } from "react";
import {
  Activity,
  ArrowRight,
  ArrowUpRight,
  Boxes,
  BookOpen,
  CheckCircle2,
  ChevronRight,
  CircleDot,
  Cpu,
  FolderGit2,
  GitBranch,
  KeyRound,
  LayoutDashboard,
  LogOut,
  Menu,
  Monitor,
  Package,
  Play,
  Plus,
  RefreshCw,
  Search,
  Server,
  Settings2,
  ShieldCheck,
  Terminal,
  X,
} from "lucide-react";
import {
  ArtifactTable,
  Empty,
  ErrorBox,
  Field,
  Loading,
  PageTitle,
  SectionTitle,
  Status,
} from "@/components/common";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Modal } from "@/components/ui/dialog";
import {
  NodeForm,
  ProjectForm,
  SecretDialog,
  SubmitForm,
} from "@/components/forms";
import { BuildDetail } from "@/components/build-detail";
import { BuildTable } from "@/components/build-table";
import { BuildWizard } from "@/components/build-wizard";
import { NodeOTA } from "@/components/emissary-ota";
import { Settings } from "@/components/settings";
import { mutation, useRemote } from "@/lib/api";
import { asError } from "@/lib/errors";
import type {
  EmissaryRelease,
  EmissaryUpdate,
  Artifact,
  Build,
  Credential,
  Node,
  Overview,
  Pool,
  Project,
  Release,
  User,
} from "@/lib/types";
import { bytes, cn, short } from "@/lib/utils";

const platform = (os: string) =>
  ({ darwin: "macOS", linux: "Linux", windows: "Windows" })[os] || os;

export function Console() {
  const { t, date, number } = useUI();
  const navigation = [
    { path: "/", label: t("navigation.overview"), icon: LayoutDashboard },
    { path: "/projects", label: t("navigation.projects"), icon: FolderGit2 },
    { path: "/builds", label: t("navigation.builds"), icon: Activity },
    { path: "/nodes", label: t("navigation.nodes"), icon: Server },
    { path: "/artifacts", label: t("navigation.artifacts"), icon: Boxes },
    { path: "/releases", label: t("navigation.releases"), icon: Package },
  ];
  const pathname = usePathname();
  const router = useRouter();
  const pieces = pathname.split("/").filter(Boolean);
  const userRemote = useRemote<User>("/auth/me", 60000);
  const user = userRemote.data;
  const projects = useRemote<Project[]>(user ? "/projects" : null);
  const builds = useRemote<Build[]>(user ? "/builds" : null, 5000);
  const [search, setSearch] = useState("");
  const [nodeOffset, setNodeOffset] = useState(0);
  const nodeSearch = pieces[0] === "nodes" ? search : "";
  const nodes = useRemote<Node[]>(user ? `/nodes?limit=100&offset=${nodeOffset}&q=${encodeURIComponent(nodeSearch)}` : null);
  const emissaryReleases = useRemote<EmissaryRelease[]>(user ? "/emissary-releases" : null);
  const emissaryUpdates = useRemote<EmissaryUpdate[]>(user ? "/emissary-updates" : null, 3000);
  const overview = useRemote<Overview>(user ? "/overview" : null);
  const releases = useRemote<Release[]>(user ? "/releases" : null);
  const artifacts = useRemote<Artifact[]>(
    user && pieces[0] === "artifacts" ? "/artifacts" : null,
  );
  const pools = useRemote<Pool[]>(user ? "/pools" : null);
  const credentials = useRemote<Credential[]>(
    user?.role === "admin" ? "/credentials" : null,
  );
  const searchRef = useRef<HTMLInputElement>(null);
  const [mobile, setMobile] = useState(false);
  const [newProject, setNewProject] = useState(false);
  const [editingProject, setEditingProject] = useState<Project>();
  const [editingNode, setEditingNode] = useState<Node>();
  const [enrolling, setEnrolling] = useState(false);
  const [secret, setSecret] = useState<string>();
  const [revoke, setRevoke] = useState<Node>();
  const [error, setError] = useState<Error>();
  useEffect(() => { setNodeOffset(0); }, [nodeSearch, pathname]);
  useEffect(() => {
    setSearch("");
    setMobile(false);
  }, [pathname]);
  useEffect(() => {
    function key(event: KeyboardEvent) {
      if ((event.metaKey || event.ctrlKey) && event.key === "k") {
        event.preventDefault();
        searchRef.current?.focus();
      }
      if (event.key === "Escape") setMobile(false);
    }
    window.addEventListener("keydown", key);
    return () => window.removeEventListener("keydown", key);
  }, []);
  async function refresh() {
    await Promise.all([
      projects.reload(),
      builds.reload(),
      nodes.reload(),
      overview.reload(),
      releases.reload(),
      artifacts.reload(),
      credentials.reload(),
    ]);
  }
  const contains = (...values: (string | null | undefined)[]) =>
    values.some((value) => value?.toLowerCase().includes(search.toLowerCase()));
  if (!user)
    return (
      <main className="mx-auto max-w-6xl px-8">
        <ErrorBox message={userRemote.error} />
        <Loading />
      </main>
    );
  const project = projects.data?.find((project) => project.id === pieces[1]);
  const current =
    pieces[0] === "settings"
      ? t("navigation.security")
      : navigation.find(
          (entry) => entry.path === (pieces[0] ? `/${pieces[0]}` : "/"),
        )?.label || t("common.workspace");
  return (
    <div className="app-shell">
      {mobile && (
        <button
          className="fixed inset-0 z-30 bg-black/60 md:hidden"
          aria-label={t("navigation.close")}
          onClick={() => setMobile(false)}
        />
      )}
      <aside
        className={cn("sidebar", mobile && "open")}
        aria-label={t("navigation.main")}
      >
        <Link href="/" className="brand">
          <span className="brand-mark">
            <BrandMark />
          </span>
          artifactory
        </Link>
        <div className="nav-label">{t("navigation.workspaceGroup")}</div>
        <nav>
          {navigation.map((entry) => (
            <Link
              key={entry.path}
              href={entry.path}
              className={cn(
                "nav-item",
                (entry.path === "/"
                  ? pathname === "/"
                  : pathname.startsWith(entry.path)) && "active",
              )}
              aria-current={
                (
                  entry.path === "/"
                    ? pathname === "/"
                    : pathname.startsWith(entry.path)
                )
                  ? "page"
                  : undefined
              }
            >
              <entry.icon />
              {entry.label}
              {entry.path === "/nodes" && (
                <span className="nav-count">
                  {overview.data?.nodes_online ?? "—"}
                </span>
              )}
              {entry.path === "/builds" && !!overview.data?.running && (
                <span className="nav-count">{overview.data.running}</span>
              )}
            </Link>
          ))}
        </nav>
        <a href="/docs" className="nav-item">
          <BookOpen />
          {t("navigation.docs")}
        </a>
        <div className="nav-label mt-5">{t("navigation.adminGroup")}</div>
        <Link
          href="/settings"
          className={cn("nav-item", pathname === "/settings" && "active")}
        >
          <KeyRound />
          {t("navigation.security")}
        </Link>
        <div className="sidebar-note">
          <div className="mb-2 flex items-center gap-2 text-foreground">
            <ShieldCheck size={13} />
            {t("navigation.trusted")}
          </div>
          {t("navigation.toolchains")}
          <br />
          {t("navigation.leases")}
        </div>
        <div className="flex items-center gap-3 border-t border-border px-3 pt-4">
          <span className="flex size-8 items-center justify-center rounded bg-secondary text-xs uppercase text-primary">
            {user.username.slice(0, 2)}
          </span>
          <div className="min-w-0 flex-1">
            <p className="truncate text-xs">{user.username}</p>
            <p className="mt-1 font-mono text-[9px] text-muted-foreground">
              {t(`security.${user.role}`)}
            </p>
          </div>
          <Button
            variant="ghost"
            size="icon"
            aria-label={t("navigation.logout")}
            onClick={() =>
              void mutation("/auth/logout", {}).then(() =>
                window.location.assign("/login"),
              )
            }
          >
            <LogOut />
          </Button>
        </div>
      </aside>
      <div className="workspace">
        <Atmosphere className="console-atmosphere" />
        <header className="topbar">
          <div className="flex items-center gap-3">
            <Button
              variant="ghost"
              size="icon"
              className="mobile-menu"
              aria-label={t("navigation.open")}
              aria-expanded={mobile}
              onClick={() => setMobile(!mobile)}
            >
              {mobile ? <X /> : <Menu />}
            </Button>
            <div className="hidden items-center gap-3 text-xs text-muted-foreground sm:flex">
              <span>{t("common.workspace")}</span>
              <ChevronRight size={12} />
              <span className="text-foreground">{current}</span>
            </div>
          </div>
          <div className="search">
            <Search size={15} />
            <input
              ref={searchRef}
              value={search}
              onChange={(event) => setSearch(event.target.value)}
              aria-label={t("search.label")}
              placeholder={t("search.placeholder")}
            />
            <kbd>⌘ K</kbd>
          </div>
          <div className="connection-info hidden items-center gap-2 text-[10px] text-muted-foreground lg:flex">
            <span
              className={cn(
                "size-1.5 rounded-full",
                overview.error
                  ? "bg-destructive"
                  : overview.data
                    ? "bg-success"
                    : "bg-muted-foreground",
              )}
            />
            {overview.error
              ? t("connection.error")
              : overview.data
                ? t("connection.connected")
                : t("connection.connecting")}
            <span className="ml-2 border-l border-border pl-3 font-mono">
              v0.1.0
            </span>
          </div>
          <PreferencesControls />
        </header>
        <main className="main">
          <ErrorBox
            message={error || projects.error || builds.error || nodes.error}
          />
          {pathname === "/" ? (
            <>
              <PageTitle
                eyebrow={t("overview.eyebrow")}
                title={t("overview.title")}
                description={t("overview.description")}
              >
                <Button
                  variant="secondary"
                  aria-label={t("overview.refresh")}
                  onClick={() => void refresh()}
                >
                  <RefreshCw />
                  {t("common.refresh")}
                </Button>
                {user.role !== "viewer" && (
                  <Button
                    disabled={!projects.data?.length}
                    onClick={() => router.push("/builds/new")}
                  >
                    <Play />
                    {t("builds.start")}
                  </Button>
                )}
              </PageTitle>
              <div className="stats">
                {[
                  {
                    label: t("overview.running"),
                    value: overview.data?.running,
                    icon: Activity,
                    foot: overview.data
                      ? t("overview.queued", { count: overview.data.queued })
                      : t("common.awaitingData"),
                  },
                  {
                    label: t("overview.nodes"),
                    value: overview.data?.nodes_online,
                    icon: Server,
                    foot: nodes.data
                      ? t("overview.registered", { count: overview.data?.nodes_registered ?? nodes.data.length })
                      : t("common.awaitingData"),
                  },
                  {
                    label: t("overview.today"),
                    value: overview.data?.builds_today,
                    icon: GitBranch,
                    foot: t("overview.byTime"),
                  },
                  {
                    label: t("overview.releases"),
                    value: overview.data?.releases,
                    icon: Package,
                    foot: t("overview.immutable"),
                  },
                ].map((stat) => (
                  <SpotlightCard
                    className="stat"
                    key={stat.icon.displayName || stat.icon.name}
                  >
                    <div className="stat-label">
                      {stat.label}
                      <stat.icon size={15} />
                    </div>
                    <div className="stat-value">
                      {stat.value === undefined ? "—" : number(stat.value)}
                    </div>
                    <div className="stat-foot">{stat.foot}</div>
                  </SpotlightCard>
                ))}
              </div>
              <div className="grid items-start gap-5 xl:grid-cols-[minmax(0,1fr)_285px]">
                <section className="panel min-w-0">
                  <SectionTitle
                    title={t("overview.recent")}
                    detail={t("overview.latestRuns")}
                  >
                    <Link
                      href="/builds"
                      className="flex items-center gap-1 text-[11px] text-muted-foreground hover:text-primary"
                    >
                      {t("builds.all")}
                      <ArrowUpRight size={13} />
                    </Link>
                  </SectionTitle>
                  {builds.data ? (
                    <BuildTable builds={builds.data.slice(0, 6)} />
                  ) : (
                    <div className="px-6">
                      <Loading />
                    </div>
                  )}
                </section>
                <section className="panel">
                  <SectionTitle
                    title={t("overview.resources")}
                    detail={t("common.live")}
                  />
                  {nodes.data?.length ? (
                    nodes.data.slice(0, 5).map((node) => (
                      <Link
                        key={node.id}
                        href="/nodes"
                        className="flex items-center gap-3 border-b border-border px-5 py-4 last:border-0"
                      >
                        <span className="rounded border border-border bg-secondary p-2">
                          <Monitor
                            size={16}
                            className="text-muted-foreground"
                          />
                        </span>
                        <div className="min-w-0 flex-1">
                          <p className="truncate text-xs">{node.name}</p>
                          <p className="mt-1 font-mono text-[9px] text-muted-foreground">
                            {platform(node.os)} · {node.arch}
                          </p>
                        </div>
                        <Status
                          small
                          value={
                            node.revoked
                              ? "revoked"
                              : node.draining
                                ? "draining"
                                : node.online
                                  ? "online"
                                  : "offline"
                          }
                        />
                      </Link>
                    ))
                  ) : (
                    <div className="px-5 py-8 text-xs leading-7 text-muted-foreground">
                      {t("overview.noNodes")}
                      <br />
                      {t("overview.installEmissary")}
                    </div>
                  )}
                  <Link
                    href="/nodes"
                    className="flex items-center justify-between border-t border-border px-5 py-4 text-xs text-primary"
                  >
                    {t("overview.manageNodes")}
                    <ArrowRight size={14} />
                  </Link>
                </section>
              </div>
              <div className="notice mt-6 flex items-start gap-3">
                <Terminal className="mt-1 size-4 shrink-0" />
                <div>
                  <span className="text-foreground">
                    {t("overview.traceable")}
                  </span>
                  <br />
                  {t("overview.configPrefix")}{" "}
                  <code className="text-xs text-primary">
                    ExecutionPlan v1
                  </code>{" "}
                  {t("overview.configSuffix")}
                </div>
              </div>
            </>
          ) : pieces[0] === "projects" ? (
            <>
              <PageTitle
                eyebrow={t("projects.eyebrow")}
                title={project?.name || t("navigation.projects")}
                description={
                  project ? project.repository_url : t("projects.description")
                }
              >
                {user.role === "admin" && (
                  <Button
                    variant={project ? "secondary" : "default"}
                    onClick={() => {
                      setEditingProject(project);
                      setNewProject(true);
                    }}
                  >
                    {project ? <Settings2 /> : <Plus />}
                    {project ? t("projects.settings") : t("projects.add")}
                  </Button>
                )}
                {project && user.role !== "viewer" && (
                  <Button onClick={() => router.push(`/builds/new?project=${project.id}`)}>
                    <Play />
                    {t("builds.start")}
                  </Button>
                )}
              </PageTitle>
              {pieces[1] ? (
                project ? (
                  <>
                    <div className="build-meta">
                      <span>
                        {t("projects.poolCount", {
                          count: project.pool_ids.length,
                        })}
                      </span>
                      <span>
                        {t("projects.signingCount", {
                          count: project.signing_profiles.length,
                        })}
                      </span>
                      <span>{t("projects.config")}</span>
                    </div>
                    <section className="panel">
                      <SectionTitle title={t("projects.builds")} />
                      {builds.data && (
                        <BuildTable
                          builds={builds.data.filter(
                            (build) => build.project_id === project.id,
                          )}
                        />
                      )}
                    </section>
                  </>
                ) : projects.data ? (
                  <Empty
                    title={t("projects.notFound")}
                    detail={t("projects.notFoundDetail")}
                  />
                ) : (
                  <Loading />
                )
              ) : !projects.data ? (
                <Loading />
              ) : projects.data.length === 0 ? (
                <section className="panel">
                  <Empty
                    title={t("projects.first")}
                    detail={t("projects.firstDetail")}
                    action={
                      user.role === "admin" ? (
                        <Button onClick={() => setNewProject(true)}>
                          <Plus />
                          {t("projects.add")}
                        </Button>
                      ) : undefined
                    }
                  />
                </section>
              ) : (
                <section className="panel">
                  <SectionTitle
                    title={t("projects.all")}
                    detail={t("common.projectCount", {
                      count: projects.data.length,
                    })}
                  />
                  <div className="table-scroll">
                    <table>
                      <thead>
                        <tr>
                          <th>{t("projects.project")}</th>
                          <th>{t("projects.repository")}</th>
                          <th>{t("projects.buildCount")}</th>
                          <th>{t("overview.recent")}</th>
                          <th />
                        </tr>
                      </thead>
                      <tbody>
                        {projects.data
                          .filter((project) =>
                            contains(project.name, project.repository_url),
                          )
                          .map((project) => (
                            <tr key={project.id}>
                              <td>
                                <Link
                                  href={`/projects/${project.id}`}
                                  className="flex items-center gap-3 font-medium hover:text-primary"
                                >
                                  <FolderGit2
                                    size={17}
                                    className="text-muted-foreground"
                                  />
                                  {project.name}
                                </Link>
                              </td>
                              <td
                                className="max-w-[320px] truncate font-mono text-[11px] text-muted-foreground"
                                title={project.repository_url}
                              >
                                {project.repository_url}
                              </td>
                              <td className="font-mono text-xs">
                                {number(project.build_count)}
                              </td>
                              <td>
                                {project.last_build ? (
                                  <Status value={project.last_build.status} />
                                ) : (
                                  <span className="text-xs text-muted-foreground">
                                    {t("projects.noBuilds")}
                                  </span>
                                )}
                              </td>
                              <td>
                                <Link
                                  href={`/projects/${project.id}`}
                                  aria-label={t("projects.view", {
                                    name: project.name,
                                  })}
                                >
                                  <ArrowUpRight size={15} />
                                </Link>
                              </td>
                            </tr>
                          ))}
                      </tbody>
                    </table>
                  </div>
                </section>
              )}
            </>
          ) : pieces[0] === "builds" ? (
            pieces[1] === "new" ? (
              projects.data ? <Suspense fallback={<Loading />}><BuildWizard projects={projects.data} user={user} /></Suspense> : <Loading />
            ) : pieces[1] ? (
              <BuildDetail id={pieces[1]} user={user} />
            ) : (
              <>
                <PageTitle
                  eyebrow={t("builds.eyebrow")}
                  title={t("navigation.builds")}
                  description={t("builds.description")}
                >
                  {user.role !== "viewer" && (
                    <Button
                      disabled={!projects.data?.length}
                      onClick={() => router.push("/builds/new")}
                    >
                      <Play />
                      {t("builds.start")}
                    </Button>
                  )}
                </PageTitle>
                <section className="panel">
                  <SectionTitle
                    title={t("builds.all")}
                    detail={t("builds.latest")}
                  />
                  {builds.data ? (
                    <BuildTable
                      builds={builds.data.filter((build) =>
                        contains(
                          build.project_name,
                          build.requested_ref,
                          build.id,
                          build.release_version,
                        ),
                      )}
                    />
                  ) : (
                    <div className="px-6">
                      <Loading />
                    </div>
                  )}
                </section>
              </>
            )
          ) : pieces[0] === "nodes" ? (
            <>
              <PageTitle
                eyebrow={t("nodes.eyebrow")}
                title={t("navigation.nodes")}
                description={t("nodes.description")}
              >
                {user.role === "admin" && (
                  <Button onClick={() => setEnrolling(true)}>
                    <Plus />
                    {t("nodes.register")}
                  </Button>
                )}
              </PageTitle>
              {!nodes.data ? (
                <Loading />
              ) : nodes.data.length === 0 ? (
                <section className="panel">
                  <Empty
                    title={t("nodes.empty")}
                    detail={t("nodes.emptyDetail")}
                  />
                </section>
              ) : (
                <div className="node-grid">
                  {nodes.data.map((node) => (
                      <SpotlightCard
                        as="article"
                        className="node-card"
                        key={node.id}
                      >
                        <div className="flex items-start gap-3">
                          <span className="rounded-lg border border-border bg-secondary p-2.5">
                            <Monitor className="size-5 text-primary" />
                          </span>
                          <div className="min-w-0 flex-1">
                            <h2 className="truncate text-sm font-medium">
                              {node.name}
                            </h2>
                            <p className="mt-1.5 font-mono text-[10px] text-muted-foreground">
                              {platform(node.os)} / {node.arch} ·{" "}
                              {node.pool_name}
                            </p>
                          </div>
                          <Status
                            small
                            value={
                              node.revoked
                                ? "revoked"
                                : node.draining
                                  ? "draining"
                                  : node.online
                                    ? "online"
                                    : "offline"
                            }
                          />
                        </div>
                        <div className="node-metrics">
                          <div>
                            <strong>
                              {node.active_jobs}/{node.slots}
                            </strong>
                            <span>{t("nodes.slots")}</span>
                          </div>
                          <div>
                            <strong>
                              {node.diagnostics.metrics?.logical_cpus ?? "—"}
                            </strong>
                            <span>{t("nodes.cpus")}</span>
                          </div>
                          <div>
                            <strong>
                              {bytes(
                                node.diagnostics.metrics?.managed_free_bytes,
                              )}
                            </strong>
                            <span>{t("nodes.disk")}</span>
                          </div>
                        </div>
                        <div className="mt-4 flex flex-wrap gap-1.5">
                          {node.capabilities.slice(0, 9).map((capability) => (
                            <span key={capability} className="tag">
                              {capability}
                            </span>
                          ))}
                          {node.capabilities.length > 9 && (
                            <span className="tag">
                              +{node.capabilities.length - 9}
                            </span>
                          )}
                        </div>
                        {node.signing_profiles.length > 0 && (
                          <div className="mt-3 flex items-center gap-2 text-[10px] text-primary">
                            <ShieldCheck size={12} />
                            {node.signing_profiles.join(", ")}
                          </div>
                        )}
                        <NodeOTA node={node} user={user} releases={emissaryReleases.data || []} updates={emissaryUpdates.data || []} reload={async () => { await Promise.all([nodes.reload(), emissaryUpdates.reload()]); }} />
                        <details className="mt-4 text-[11px] text-muted-foreground">
                          <summary className="cursor-pointer">
                            {t("nodes.diagnostics")}
                          </summary>
                          <div className="mt-3 space-y-2 leading-6">
                            <p>
                              {t("nodes.lastSeen")}
                              {date(node.last_seen)}
                            </p>
                            <p>
                              {t("nodes.certificate")}
                              {date(node.certificate_expires_at)}
                            </p>
                            <p>
                              Emissary：
                              {node.diagnostics.emissary_version ||
                                t("nodes.notReported")}{" "}
                              {t("nodes.heap")}
                              {bytes(
                                node.diagnostics.metrics?.emissary_rss_bytes,
                              )}
                            </p>
                            <p>
                              {t("nodes.buffer")}
                              {bytes(node.diagnostics.buffered_log_bytes)}
                            </p>
                            {Object.entries(node.diagnostics.tools || {})
                              .filter(([, tool]) => tool.available)
                              .map(([name, tool]) => (
                                <div
                                  key={name}
                                  className="border-t border-border pt-2"
                                >
                                  <span className="font-mono text-foreground">
                                    {name}
                                  </span>
                                  <pre className="mt-1 whitespace-pre-wrap break-all font-mono text-[9px]">
                                    {tool.version || tool.path}
                                  </pre>
                                </div>
                              ))}
                          </div>
                        </details>
                        <div className="mt-5 flex items-center justify-between border-t border-border pt-4">
                          <span className="font-mono text-[9px] text-muted-foreground">
                            #{short(node.id)}
                          </span>
                          {user.role === "admin" && (
                            <div className="flex gap-1">
                              <Button
                                size="sm"
                                variant="ghost"
                                onClick={() => setEditingNode(node)}
                              >
                                {t("common.settings")}
                              </Button>
                              {!node.revoked && (
                                <Button
                                  size="sm"
                                  variant="ghost"
                                  className="text-destructive"
                                  onClick={() => setRevoke(node)}
                                >
                                  {t("common.revoke")}
                                </Button>
                              )}
                            </div>
                          )}
                        </div>
                      </SpotlightCard>
                    ))}
                </div>
              )}
              <nav className="flex items-center justify-end gap-3" aria-label={t("nodes.pages")}>
                <Button variant="secondary" disabled={nodeOffset === 0 || !nodes.data} onClick={() => setNodeOffset((offset) => Math.max(0, offset - 100))}>{t("nodes.previous")}</Button>
                <span className="text-sm text-muted-foreground">{t("nodes.page", { page: nodeOffset / 100 + 1 })}</span>
                <Button variant="secondary" disabled={!nodes.data || nodes.data.length < 100} onClick={() => setNodeOffset((offset) => offset + 100)}>{t("nodes.next")}</Button>
              </nav>
            </>
          ) : pieces[0] === "artifacts" ? (
            <>
              <PageTitle
                eyebrow={t("artifacts.eyebrow")}
                title={t("navigation.artifacts")}
                description={t("artifacts.description")}
              />
              <section className="panel">
                <SectionTitle
                  title={t("artifacts.files")}
                  detail={t("artifacts.latest")}
                />
                {!artifacts.data ? (
                  <div className="px-6">
                    <Loading />
                  </div>
                ) : artifacts.data.length ? (
                  <ArtifactTable
                    artifacts={artifacts.data.filter((artifact) =>
                      contains(
                        artifact.path,
                        artifact.target,
                        artifact.project_name,
                      ),
                    )}
                  />
                ) : (
                  <Empty
                    title={t("artifacts.empty")}
                    detail={t("artifacts.emptyDetail")}
                  />
                )}
              </section>
            </>
          ) : pieces[0] === "releases" ? (
            pieces[1] ? (
              <ReleaseDetail id={pieces[1]} />
            ) : (
              <>
                <PageTitle
                  eyebrow={t("releases.eyebrow")}
                  title={t("navigation.releases")}
                  description={t("releases.description")}
                />
                <section className="panel">
                  <SectionTitle title={t("releases.catalog")} />
                  {!releases.data ? (
                    <div className="px-6">
                      <Loading />
                    </div>
                  ) : releases.data.length ? (
                    <div className="table-scroll">
                      <table>
                        <thead>
                          <tr>
                            <th>{t("releases.versionProject")}</th>
                            <th>{t("releases.commit")}</th>
                            <th>{t("releases.artifacts")}</th>
                            <th>{t("releases.publishedAt")}</th>
                            <th />
                          </tr>
                        </thead>
                        <tbody>
                          {releases.data
                            .filter((release) =>
                              contains(release.version, release.project_name),
                            )
                            .map((release) => (
                              <tr key={release.id}>
                                <td>
                                  <Link
                                    href={`/releases/${release.id}`}
                                    className="font-medium text-primary hover:underline"
                                  >
                                    {release.version}
                                  </Link>
                                  <div className="mt-1 text-xs text-muted-foreground">
                                    {release.project_name}
                                  </div>
                                </td>
                                <td className="font-mono text-xs text-muted-foreground">
                                  {short(release.commit_sha)}
                                </td>
                                <td className="font-mono text-xs">
                                  {number(release.artifact_count)}
                                </td>
                                <td className="font-mono text-xs text-muted-foreground">
                                  {date(release.created_at)}
                                </td>
                                <td>
                                  <Link
                                    href={`/releases/${release.id}`}
                                    aria-label={t("releases.view", {
                                      version: release.version,
                                    })}
                                  >
                                    <ArrowUpRight size={15} />
                                  </Link>
                                </td>
                              </tr>
                            ))}
                        </tbody>
                      </table>
                    </div>
                  ) : (
                    <Empty
                      title={t("releases.empty")}
                      detail={t("releases.emptyDetail")}
                    />
                  )}
                </section>
              </>
            )
          ) : pieces[0] === "settings" ? (
            <Settings user={user} projects={projects.data || []} />
          ) : (
            <Empty
              title={t("notFound.title")}
              detail={t("notFound.description")}
              action={
                <Button asChild>
                  <Link href="/">{t("notFound.back")}</Link>
                </Button>
              }
            />
          )}
        </main>
        <footer className="flex flex-wrap justify-between gap-2 border-t border-border px-8 py-4 text-[10px] text-muted-foreground">
          <span>{t("footer.description")}</span>
          <span className="font-mono">{t("footer.technical")}</span>
        </footer>
      </div>
      <Modal
        open={newProject}
        onOpenChange={setNewProject}
        title={
          editingProject ? t("projects.settings") : t("projects.dialogTitle")
        }
        description={t("projects.dialogDetail")}
      >
        {newProject && (
          <ProjectForm
            project={editingProject}
            pools={pools.data || []}
            credentials={credentials.data || []}
            done={() => {
              setNewProject(false);
              setEditingProject(undefined);
              void refresh();
            }}
          />
        )}
      </Modal>
      <Modal
        open={!!editingNode}
        onOpenChange={(open) => {
          if (!open) setEditingNode(undefined);
        }}
        title={t("nodes.configure")}
        description={t("nodes.configureHint")}
      >
        {editingNode && (
          <NodeForm
            node={editingNode}
            pools={pools.data || []}
            done={() => {
              setEditingNode(undefined);
              void refresh();
            }}
          />
        )}
      </Modal>
      <Modal
        open={enrolling}
        onOpenChange={setEnrolling}
        title={t("nodes.enroll")}
        description={t("nodes.enrollHint")}
      >
        {enrolling && (
          <SubmitForm
            label={t("nodes.generateToken")}
            onSubmit={async (form) => {
              const result = await mutation<{ token: string }>(
                "/enrollment-tokens",
                { pool_id: form.get("pool_id") },
              );
              setEnrolling(false);
              setSecret(result.token);
            }}
          >
            <Field label={t("nodes.joinPool")}>
              <select className="input" name="pool_id">
                {pools.data?.map((pool) => (
                  <option key={pool.id} value={pool.id}>
                    {pool.name}
                  </option>
                ))}
              </select>
            </Field>
            <div className="notice">{t("nodes.install")}</div>
          </SubmitForm>
        )}
      </Modal>
      <SecretDialog
        value={secret}
        title={t("nodes.saveToken")}
        close={() => setSecret(undefined)}
      />
      <Modal
        open={!!revoke}
        onOpenChange={(open) => {
          if (!open) setRevoke(undefined);
        }}
        title={t("nodes.revokeTitle")}
        description={t("nodes.revokeDescription", { name: revoke?.name || "" })}
      >
        <div className="form-footer">
          <Button variant="secondary" onClick={() => setRevoke(undefined)}>
            {t("nodes.keep")}
          </Button>
          <Button
            variant="destructive"
            onClick={() => {
              if (revoke)
                void mutation(
                  `/nodes/${revoke.id}`,
                  {
                    draining: true,
                    revoked: true,
                    slots: revoke.slots,
                    pool_id: revoke.pool_id,
                    labels: revoke.labels,
                    signing_profiles: revoke.signing_profiles,
                  },
                  "PATCH",
                )
                  .then(() => {
                    setRevoke(undefined);
                    void refresh();
                  })
                  .catch((error) => setError(asError(error)));
            }}
          >
            {t("nodes.confirmRevoke")}
          </Button>
        </div>
      </Modal>
    </div>
  );
}

function ReleaseDetail({ id }: { id: string }) {
  const { t, date } = useUI();
  const remote = useRemote<Release>(`/releases/${id}`);
  const release = remote.data;
  if (!release)
    return (
      <>
        <ErrorBox message={remote.error} />
        <Loading />
      </>
    );
  return (
    <>
      <PageTitle
        eyebrow={t("releases.detailEyebrow")}
        title={release.version}
        description={t("releases.resultDescription", {
          name: release.project_name,
          date: date(release.created_at),
        })}
      >
        <Button variant="secondary" asChild>
          <Link href={`/builds/${release.build_id}`}>
            <Activity />
            {t("releases.sourceBuild")}
          </Link>
        </Button>
      </PageTitle>
      <div className="build-meta">
        <span className="meta-item">
          <GitBranch size={14} />
          {release.commit_sha}
        </span>
        <span className="meta-item">
          <ShieldCheck size={14} />
          {t("releases.immutable")}
        </span>
      </div>
      <section className="panel">
        <SectionTitle
          title={t("builds.publish")}
          detail={t("common.artifactCount", {
            count: release.artifacts?.length || 0,
          })}
        />
        <ArtifactTable artifacts={release.artifacts || []} />
      </section>
      <div className="notice mt-5">{t("releases.notice")}</div>
    </>
  );
}
