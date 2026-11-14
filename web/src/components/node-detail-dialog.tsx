"use client";

import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import {
  Activity,
  Cpu,
  Database,
  HardDrive,
  Info,
  Settings2,
  ShieldCheck,
  Terminal,
} from "lucide-react";
import { ErrorBox, Status } from "@/components/common";
import { NodeOTA } from "@/components/emissary-ota";
import { NodeForm } from "@/components/forms";
import { nodePlatform, nodeStatus } from "@/components/node-card";
import {
  NodeDataError,
  NodeFacts,
  NodeRawData,
  NodeTimestamp,
} from "@/components/node-detail-parts";
import { NodeMetadata } from "@/components/node-metadata";
import { useUI } from "@/components/providers";
import { Button } from "@/components/ui/button";
import { Modal } from "@/components/ui/dialog";
import { mutation, useRemote } from "@/lib/api";
import { asError } from "@/lib/errors";
import type {
  EmissaryRelease,
  EmissaryUpdate,
  Node,
  NodeInventory,
  NodeTelemetry,
  Pool,
  User,
} from "@/lib/types";
import { bytes, cn } from "@/lib/utils";

function NodeSection({
  title,
  children,
}: {
  title: string;
  children: ReactNode;
}) {
  return (
    <section className="node-detail-section">
      <h3>{title}</h3>
      {children}
    </section>
  );
}

function Tags({ values }: { values: string[] }) {
  const { t } = useUI();
  return values.length ? (
    <div className="flex flex-wrap gap-2">
      {values.map((value) => (
        <span className="tag max-w-full break-all" key={value}>
          {value}
        </span>
      ))}
    </div>
  ) : (
    <p className="node-empty">{t("nodes.notReported")}</p>
  );
}

function Overview({
  node,
  inventory,
}: {
  node: Node;
  inventory?: NodeTelemetry["inventory"];
}) {
  const { t } = useUI();
  const machine = inventory?.inventory;
  const formatBytes = (value?: number | null) =>
    value == null ? t("nodes.unknown") : bytes(value);
  return (
    <div className="space-y-7">
      <NodeSection title={t("nodes.identity")}>
        <NodeFacts
          entries={[
            [t("nodes.nodeId"), node.id],
            [t("nodes.hostname"), node.diagnostics.hostname],
            [t("nodes.pool"), node.pool_name],
            [t("nodes.poolId"), node.pool_id],
            [t("nodes.capacity"), `${node.active_jobs} / ${node.slots}`],
            [
              t("nodes.createdAt"),
              <NodeTimestamp key="created" value={node.created_at} />,
            ],
            [
              t("nodes.lastSeenLabel"),
              <NodeTimestamp key="seen" value={node.last_seen} />,
            ],
            [
              t("nodes.certificateLabel"),
              <NodeTimestamp
                key="certificate"
                value={node.certificate_expires_at}
              />,
            ],
          ]}
        />
      </NodeSection>
      <NodeSection title={t("nodes.hardware")}>
        <NodeFacts
          entries={[
            [t("nodes.platform"), `${nodePlatform(node.os)} / ${node.arch}`],
            [t("nodes.osVersion"), machine?.os_version],
            [t("nodes.kernel"), machine?.kernel_version],
            [t("nodes.cpuModel"), machine?.cpu_model],
            [
              t("nodes.cpus"),
              machine?.logical_cpus ?? node.diagnostics.metrics?.logical_cpus,
            ],
            [t("nodes.physicalCores"), machine?.physical_cores],
            [
              t("nodes.physicalMemory"),
              formatBytes(machine?.physical_memory_bytes),
            ],
            [t("nodes.cpuLimit"), machine?.effective_cpu_limit],
            [
              t("nodes.memoryLimit"),
              formatBytes(machine?.effective_memory_limit_bytes),
            ],
            [t("nodes.container"), machine?.container],
            [t("nodes.virtualization"), machine?.virtualization],
            [
              t("nodes.bootTime"),
              <NodeTimestamp
                key="boot"
                value={
                  machine?.boot_time == null
                    ? null
                    : new Date(machine.boot_time * 1000).toISOString()
                }
              />,
            ],
          ]}
        />
        {inventory && (
          <p className="mt-3 text-xs text-muted-foreground">
            {t("nodes.inventorySampledAt")}{" "}
            <NodeTimestamp value={inventory.sampled_at} />
          </p>
        )}
      </NodeSection>
      {!!machine?.accelerators?.length && (
        <NodeSection title={t("nodes.accelerators")}>
          <div className="space-y-4">
            {machine.accelerators.map((device) => (
              <NodeFacts
                key={device.id}
                entries={[
                  [t("nodes.device"), device.id],
                  [t("nodes.model"), device.model],
                  [t("nodes.vendor"), device.vendor],
                  [t("nodes.driver"), device.driver],
                  [
                    t("nodes.deviceMemory"),
                    formatBytes(device.memory_total_bytes),
                  ],
                ]}
              />
            ))}
          </div>
        </NodeSection>
      )}
      <NodeSection title={t("nodes.labels")}>
        {Object.keys(node.labels).length ? (
          <NodeFacts entries={Object.entries(node.labels)} />
        ) : (
          <p className="node-empty">{t("nodes.notReported")}</p>
        )}
      </NodeSection>
      {machine && (
        <NodeRawData value={machine} label={t("nodes.fullInventory")} />
      )}
    </div>
  );
}

function Resources({
  node,
  telemetry,
  failed,
}: {
  node: Node;
  telemetry?: NodeTelemetry;
  failed: boolean;
}) {
  const { t, number } = useUI();
  const host = telemetry?.host;
  if (!host) return <p className="node-empty">{t("nodes.noResourceSample")}</p>;
  const sample = host.sample;
  const formatBytes = (value?: number | null) =>
    value == null ? t("nodes.unknown") : bytes(value);
  const numeric = (value?: number | null) =>
    value == null ? t("nodes.unknown") : number(value);
  const percent = (value?: number | null) =>
    value == null
      ? t("nodes.unknown")
      : `${number(Math.round(value * 10) / 10)}%`;
  const recent = (value?: string | null) => {
    const age = value ? Date.now() - Date.parse(value) : NaN;
    return Number.isFinite(age) && age < 90000 && age >= -30000;
  };
  const fresh =
    !failed &&
    node.online &&
    host.fresh === true &&
    recent(host.sampled_at) &&
    recent(host.received_at);
  const metrics = [
    {
      label: `CPU ${t("nodes.cpuUsage")}`,
      value: percent(sample.cpu_percent),
      icon: Cpu,
    },
    {
      label: t("nodes.availableMemory"),
      value: formatBytes(sample.available_memory_bytes),
      icon: Activity,
    },
    {
      label: t("nodes.disk"),
      value: formatBytes(sample.build_disk_available_bytes),
      icon: HardDrive,
    },
  ];
  return (
    <div className="space-y-7">
      <div
        className={cn("node-resource-freshness", fresh && "is-fresh")}
        role="status"
      >
        <span className="font-medium">
          {t(fresh ? "nodes.freshSample" : "nodes.staleSample")}
        </span>
        <span>
          {t("nodes.sampledAt")} <NodeTimestamp value={host.sampled_at} />
        </span>
        <span>
          {t("nodes.receivedAt")} <NodeTimestamp value={host.received_at} />
        </span>
      </div>
      <div className="node-resource-grid">
        {metrics.map(({ label, value, icon: Icon }) => (
          <div className="node-resource-metric" key={label}>
            <span>
              <Icon size={15} aria-hidden="true" />
              {label}
            </span>
            <strong>{value}</strong>
          </div>
        ))}
      </div>
      <NodeFacts
        entries={[
          [t("nodes.memoryUsed"), formatBytes(sample.memory_used_bytes)],
          [t("nodes.memoryTotal"), formatBytes(sample.memory_total_bytes)],
          [t("nodes.swapUsed"), formatBytes(sample.swap_used_bytes)],
          [t("nodes.swapTotal"), formatBytes(sample.swap_total_bytes)],
          [t("nodes.loadOne"), numeric(sample.load_one)],
          [t("nodes.activeAttempts"), numeric(sample.active_attempts)],
          [t("nodes.sessionId"), host.session_id],
          [t("nodes.droppedBatches"), numeric(host.dropped_batches)],
        ]}
      />
      <NodeSection title={t("nodes.disks")}>
        {sample.disks?.length ? (
          <div className="node-detail-table">
            <table>
              <thead>
                <tr>
                  {[
                    t("nodes.device"),
                    t("nodes.mount"),
                    t("nodes.total"),
                    t("nodes.available"),
                    t("nodes.readBytes"),
                    t("nodes.writtenBytes"),
                  ].map((label) => (
                    <th key={label}>{label}</th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {sample.disks.map((disk, index) => (
                  <tr key={`${disk.device}:${disk.mount}:${index}`}>
                    <td>{disk.device}</td>
                    <td>{disk.mount}</td>
                    <td>{formatBytes(disk.total_bytes)}</td>
                    <td>{formatBytes(disk.available_bytes)}</td>
                    <td>{formatBytes(disk.read_bytes)}</td>
                    <td>{formatBytes(disk.written_bytes)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : (
          <p className="node-empty">{t("nodes.notReported")}</p>
        )}
      </NodeSection>
      <NodeSection title={t("nodes.networks")}>
        <p className="mb-3 text-xs text-muted-foreground">
          {t("nodes.cumulativeCounters")}
        </p>
        {sample.networks?.length ? (
          <div className="node-detail-table">
            <table>
              <thead>
                <tr>
                  {[
                    t("nodes.interface"),
                    t("nodes.receivedBytes"),
                    t("nodes.transmittedBytes"),
                    t("nodes.receiveErrors"),
                    t("nodes.transmitErrors"),
                  ].map((label) => (
                    <th key={label}>{label}</th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {sample.networks.map((network) => (
                  <tr key={network.interface}>
                    <td>{network.interface}</td>
                    <td>{formatBytes(network.received_bytes)}</td>
                    <td>{formatBytes(network.transmitted_bytes)}</td>
                    <td>{numeric(network.receive_errors)}</td>
                    <td>{numeric(network.transmit_errors)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : (
          <p className="node-empty">{t("nodes.notReported")}</p>
        )}
      </NodeSection>
      <NodeSection title={t("nodes.accelerators")}>
        {sample.accelerators?.length ? (
          <div className="node-detail-table">
            <table>
              <thead>
                <tr>
                  {[
                    t("nodes.device"),
                    t("nodes.cpuUsage"),
                    t("nodes.memoryUsed"),
                  ].map((label) => (
                    <th key={label}>{label}</th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {sample.accelerators.map((accelerator) => (
                  <tr key={accelerator.id}>
                    <td>{accelerator.id}</td>
                    <td>{percent(accelerator.utilization_percent)}</td>
                    <td>{formatBytes(accelerator.memory_used_bytes)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : (
          <p className="node-empty">{t("nodes.notReported")}</p>
        )}
      </NodeSection>
      <NodeRawData value={host} />
    </div>
  );
}

function Tools({ node, inventory }: { node: Node; inventory?: NodeInventory }) {
  const { t } = useUI();
  const diagnostics = node.diagnostics.tools || {};
  const reported = inventory?.tools || {};
  const tools = [
    ...new Set([...Object.keys(diagnostics), ...Object.keys(reported)]),
  ].sort();
  return (
    <div className="space-y-7">
      <NodeSection title={t("nodes.capabilities")}>
        <Tags
          values={[
            ...new Set([
              ...node.capabilities,
              ...(inventory?.capabilities || []),
            ]),
          ]}
        />
      </NodeSection>
      <NodeSection title={t("nodes.tools")}>
        {tools.length ? (
          <div className="node-tool-list">
            {tools.map((name) => {
              const tool = { ...diagnostics[name], ...reported[name] };
              return (
                <div key={name} className="node-tool">
                  <div className="mb-2 flex flex-wrap items-center justify-between gap-2">
                    <h4>{name}</h4>
                    <span
                      className={cn(
                        "text-xs",
                        tool.available === true
                          ? "text-success"
                          : "text-muted-foreground",
                      )}
                    >
                      {t(
                        tool.available === true
                          ? "nodes.toolAvailable"
                          : tool.available === false
                            ? "nodes.toolUnavailable"
                            : "nodes.unknown",
                      )}
                    </span>
                  </div>
                  <NodeFacts
                    entries={[
                      [t("nodes.toolVersion"), tool.version],
                      [t("nodes.toolPath"), tool.path],
                    ]}
                  />
                </div>
              );
            })}
          </div>
        ) : (
          <p className="node-empty">{t("nodes.notReported")}</p>
        )}
      </NodeSection>
      <NodeSection title={t("nodes.signingAuthorization")}>
        <Tags values={node.signing_profiles} />
      </NodeSection>
      <NodeSection title={t("nodes.configuredSigning")}>
        <Tags
          values={
            node.diagnostics.configured_signing_profiles ||
            inventory?.signing_profile_refs ||
            []
          }
        />
      </NodeSection>
      {!!Object.keys(inventory?.collectors || {}).length && (
        <NodeSection title={t("nodes.collectors")}>
          <NodeFacts entries={Object.entries(inventory!.collectors!)} />
        </NodeSection>
      )}
    </div>
  );
}

function Operations({
  node,
  user,
  pools,
  reload,
}: {
  node: Node;
  user: User;
  pools: Pool[];
  reload: () => Promise<unknown>;
}) {
  const { t } = useUI();
  const [editing, setEditing] = useState(false);
  const [revoking, setRevoking] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<Error>();
  const releases = useRemote<EmissaryRelease[]>("/emissary-releases", 30000);
  const updates = useRemote<EmissaryUpdate[]>("/emissary-updates", 3000);
  async function refresh() {
    await Promise.all([reload(), releases.reload(), updates.reload()]);
  }
  async function revoke() {
    setBusy(true);
    setError(undefined);
    try {
      await mutation(
        `/nodes/${node.id}`,
        {
          draining: true,
          revoked: true,
          slots: node.slots,
          pool_id: node.pool_id,
          labels: node.labels,
          signing_profiles: node.signing_profiles,
        },
        "PATCH",
      );
      setRevoking(false);
      await refresh();
    } catch (cause) {
      setError(asError(cause));
    } finally {
      setBusy(false);
    }
  }
  return (
    <div className="space-y-7">
      <NodeSection title={t("ota.title")}>
        <NodeDataError
          error={releases.error || updates.error}
          retry={refresh}
        />
        <NodeOTA
          node={node}
          user={user}
          releases={releases.data || []}
          updates={updates.data || []}
          reload={refresh}
          actionsReady={
            !!releases.data &&
            !!updates.data &&
            !releases.error &&
            !updates.error
          }
        />
      </NodeSection>
      <NodeSection title={t("nodes.runtime")}>
        <NodeFacts
          entries={[
            [
              t("ota.current"),
              node.emissary_current_version ||
                node.diagnostics.emissary_version,
            ],
            [t("nodes.emissaryCommit"), node.diagnostics.emissary_commit],
            [t("nodes.protocol"), node.diagnostics.protocol_major],
            [
              t("nodes.processMemory"),
              node.diagnostics.metrics?.emissary_rss_bytes == null
                ? t("nodes.unknown")
                : bytes(node.diagnostics.metrics.emissary_rss_bytes),
            ],
            [
              t("nodes.logBuffer"),
              node.diagnostics.buffered_log_bytes == null
                ? t("nodes.unknown")
                : bytes(node.diagnostics.buffered_log_bytes),
            ],
          ]}
        />
        <NodeRawData
          value={node.diagnostics}
          label={t("nodes.fullDiagnostics")}
        />
        {node.emissary_ota_status && (
          <NodeRawData
            value={node.emissary_ota_status}
            label={t("nodes.fullOtaStatus")}
          />
        )}
      </NodeSection>
      {user.role === "admin" && (
        <>
          <NodeSection title={t("nodes.configure")}>
            {editing ? (
              <NodeForm
                node={node}
                pools={pools}
                done={() => {
                  setEditing(false);
                  void refresh();
                }}
              />
            ) : (
              <>
                <p className="mb-4 text-sm leading-6 text-muted-foreground">
                  {t("nodes.configureHint")}
                </p>
                <Button
                  variant="secondary"
                  onClick={() => setEditing(true)}
                >
                  <Settings2 size={15} />
                  {t("common.settings")}
                </Button>
              </>
            )}
          </NodeSection>
          {!node.revoked && (
            <div className="node-danger-zone">
              <div>
                <h3>{t("nodes.revokeTitle")}</h3>
                <p>{t("nodes.revokeDescription", { name: node.name })}</p>
              </div>
              <Button
                variant="destructive"
                size="sm"
                onClick={() => {
                  setError(undefined);
                  setRevoking(true);
                }}
              >
                {t("common.revoke")}
              </Button>
            </div>
          )}
        </>
      )}
      <Modal
        open={revoking}
        onOpenChange={(open) => {
          if (!busy) setRevoking(open);
        }}
        title={t("nodes.revokeTitle")}
        description={t("nodes.revokeDescription", { name: node.name })}
      >
        <ErrorBox message={error} />
        <div className="form-footer">
          <Button
            variant="secondary"
            disabled={busy}
            onClick={() => setRevoking(false)}
          >
            {t("nodes.keep")}
          </Button>
          <Button
            variant="destructive"
            disabled={busy}
            onClick={() => void revoke()}
          >
            {t("nodes.confirmRevoke")}
          </Button>
        </div>
      </Modal>
    </div>
  );
}

export function NodeDetailDialog({
  node,
  user,
  pools,
  onClose,
  reload,
}: {
  node: Node;
  user: User;
  pools: Pool[];
  onClose: () => void;
  reload: () => Promise<unknown>;
}) {
  const { t } = useUI();
  const tabs = [
    { id: "overview", label: t("nodes.tabs.overview"), icon: Info },
    { id: "resources", label: t("nodes.tabs.resources"), icon: Activity },
    { id: "tools", label: t("nodes.tabs.tools"), icon: Terminal },
    { id: "metadata", label: t("nodes.tabs.metadata"), icon: Database },
    { id: "operations", label: t("nodes.tabs.operations"), icon: ShieldCheck },
  ] as const;
  const [tab, setTab] = useState<(typeof tabs)[number]["id"]>("overview");
  const tabRefs = useRef<(HTMLButtonElement | null)[]>([]);
  const panelRef = useRef<HTMLDivElement>(null);
  const id = useId();
  const telemetry = useRemote<NodeTelemetry>(
    `/nodes/${node.id}/telemetry`,
    30000,
  );
  const inventory = telemetry.data?.inventory?.inventory;
  useEffect(() => {
    panelRef.current?.parentElement?.scrollTo({ top: 0 });
  }, [tab]);
  async function refresh() {
    await Promise.all([reload(), telemetry.reload()]);
  }
  return (
    <Modal
      open
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
      size="wide"
      title={
        <span className="flex flex-wrap items-center gap-3">
          <span className="min-w-0 break-all">{node.name}</span>
          <Status small value={nodeStatus(node)} />
        </span>
      }
      description={`${nodePlatform(node.os)} / ${node.arch} · ${node.pool_name}`}
      navigation={
        <div
          className="node-detail-tabs"
          role="tablist"
          aria-label={t("nodes.detailTabs")}
        >
          {tabs.map((entry, index) => (
            <button
              key={entry.id}
              type="button"
              role="tab"
              id={`${id}-${entry.id}`}
              aria-controls={`${id}-panel`}
              aria-selected={tab === entry.id}
              tabIndex={tab === entry.id ? 0 : -1}
              ref={(element) => {
                tabRefs.current[index] = element;
              }}
              onClick={() => setTab(entry.id)}
              onKeyDown={(event) => {
                const next =
                  event.key === "ArrowRight"
                    ? (index + 1) % tabs.length
                    : event.key === "ArrowLeft"
                      ? (index + tabs.length - 1) % tabs.length
                      : event.key === "Home"
                        ? 0
                        : event.key === "End"
                          ? tabs.length - 1
                          : null;
                if (next === null) return;
                event.preventDefault();
                setTab(tabs[next].id);
                tabRefs.current[next]?.focus();
                tabRefs.current[next]?.scrollIntoView({
                  block: "nearest",
                  inline: "nearest",
                });
              }}
            >
              <entry.icon size={15} aria-hidden="true" />
              {entry.label}
            </button>
          ))}
        </div>
      }
    >
      <div
        ref={panelRef}
        role="tabpanel"
        id={`${id}-panel`}
        aria-labelledby={`${id}-${tab}`}
        tabIndex={0}
        className="node-detail-panel"
      >
        {tab !== "metadata" && tab !== "operations" && (
          <div className="mb-5 empty:hidden">
            <NodeDataError error={telemetry.error} retry={telemetry.reload} />
            {!telemetry.data && !telemetry.error && (
              <p role="status" className="py-3 text-sm text-muted-foreground">
                {t("common.loading")}
              </p>
            )}
          </div>
        )}
        {tab === "overview" && (
          <Overview node={node} inventory={telemetry.data?.inventory} />
        )}
        {tab === "resources" && telemetry.data && (
          <Resources
            node={node}
            telemetry={telemetry.data}
            failed={!!telemetry.error}
          />
        )}
        {tab === "tools" && <Tools node={node} inventory={inventory} />}
        {tab === "metadata" && <NodeMetadata id={node.id} />}
        {tab === "operations" && (
          <Operations node={node} user={user} pools={pools} reload={refresh} />
        )}
      </div>
    </Modal>
  );
}
