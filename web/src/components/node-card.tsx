"use client";

import { ArrowUpRight, Layers3, Monitor } from "lucide-react";
import { Status } from "@/components/common";
import { useUI } from "@/components/providers";
import SpotlightCard from "@/components/react-bits/SpotlightCard";
import { Button } from "@/components/ui/button";
import type { Node } from "@/lib/types";

export const nodePlatform = (os: string) =>
  ({ darwin: "macOS", linux: "Linux", windows: "Windows" })[os] || os;

export const nodeStatus = (node: Node) =>
  node.revoked
    ? "revoked"
    : node.draining || node.ota_draining
      ? "draining"
      : node.online
        ? "online"
        : "offline";

export function NodeCard({
  node,
  onDetails,
}: {
  node: Node;
  onDetails: () => void;
}) {
  const { t, number } = useUI();
  const utilization =
    node.slots > 0
      ? Math.min(100, Math.max(0, (node.active_jobs / node.slots) * 100))
      : 0;
  return (
    <SpotlightCard as="article" className="node-card" aria-label={node.name}>
      <div className="flex items-center justify-between gap-3">
        <span className="node-icon">
          <Monitor size={19} aria-hidden="true" />
        </span>
        <Status small value={nodeStatus(node)} />
      </div>
      <div className="mt-4 min-w-0">
        <h2 className="truncate text-base font-medium" title={node.name}>
          {node.name}
        </h2>
        <div className="mt-2 flex min-w-0 items-center gap-2 text-[11px] text-muted-foreground">
          <span className="shrink-0 font-mono">
            {nodePlatform(node.os)} / {node.arch}
          </span>
          <span aria-hidden="true" className="text-border">
            ·
          </span>
          <span
            className="flex min-w-0 items-center gap-1.5"
            title={node.pool_name}
          >
            <Layers3 size={12} className="shrink-0" aria-hidden="true" />
            <span className="truncate">{node.pool_name}</span>
          </span>
        </div>
      </div>
      <div className="mt-6">
        <div className="mb-2.5 flex items-baseline justify-between gap-3">
          <span className="text-[11px] text-muted-foreground">
            {t("nodes.capacity")}
          </span>
          <span className="font-mono text-sm tabular-nums">
            {number(node.active_jobs)}
            <span className="mx-1 text-muted-foreground">/</span>
            {number(node.slots)}
          </span>
        </div>
        <div
          className="node-capacity-track"
          role="meter"
          aria-label={t("nodes.capacity")}
          aria-valuemin={0}
          aria-valuemax={Math.max(node.slots, node.active_jobs, 1)}
          aria-valuenow={node.active_jobs}
          aria-valuetext={t("nodes.capacityValue", {
            used: node.active_jobs,
            total: node.slots,
          })}
        >
          <div
            className="h-full rounded-full bg-primary"
            style={{ width: `${utilization}%` }}
          />
        </div>
      </div>
      <div className="mt-5 flex justify-end border-t border-border pt-3">
        <Button
          variant="ghost"
          size="sm"
          onClick={onDetails}
          aria-label={t("nodes.viewNamed", { name: node.name })}
        >
          {t("nodes.viewDetails")}
          <ArrowUpRight size={14} aria-hidden="true" />
        </Button>
      </div>
    </SpotlightCard>
  );
}
