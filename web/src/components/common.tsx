"use client";

import { usePathname } from "next/navigation";
import AnimatedContent from "@/components/react-bits/AnimatedContent";
import { useUI } from "@/components/providers";

import {
  AlertTriangle,
  Check,
  CheckCircle2,
  Circle,
  CircleDashed,
  Clock3,
  Copy,
  Download,
  LoaderCircle,
  XCircle,
} from "lucide-react";
import { useState, type ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import type { Artifact } from "@/lib/types";
import { bytes, short } from "@/lib/utils";
import { LocalizedError } from "@/lib/errors";

export function Status({
  value,
  small = false,
}: {
  value: string;
  small?: boolean;
}) {
  const { t } = useUI();
  const labels: Record<string, string> = {
    resolving: t("status.resolving"),
    planning: t("status.planning"),
    replanning: t("status.replanning"),
    needs_input: t("status.needs_input"),
    queued: t("status.queued"),
    leased: t("status.leased"),
    running: t("status.running"),
    succeeded: t("status.succeeded"),
    failed: t("status.failed"),
    canceled: t("status.canceled"),
    canceling: t("status.canceling"),
    lost: t("status.lost"),
    pending: t("status.pending"),
    not_requested: t("status.not_requested"),
    online: t("status.online"),
    offline: t("status.offline"),
    draining: t("status.draining"),
    revoked: t("status.revoked"),
  };
  const Icon = ["running", "resolving", "canceling"].includes(value)
    ? LoaderCircle
    : ["succeeded", "online"].includes(value)
      ? CheckCircle2
      : ["failed", "lost", "revoked"].includes(value)
        ? XCircle
        : ["queued", "leased", "pending"].includes(value)
          ? Clock3
          : CircleDashed;
  return (
    <span className={cn("status", small && "status-small", `status-${value}`)}>
      <Icon
        className={cn(
          "size-3.5",
          ["running", "resolving"].includes(value) &&
            "motion-safe:animate-spin",
        )}
      />
      {labels[value] || value}
    </span>
  );
}

export function Empty({
  title,
  detail,
  action,
}: {
  title: string;
  detail: string;
  action?: ReactNode;
}) {
  return (
    <div className="empty-state">
      <div className="mb-4 flex size-11 items-center justify-center rounded-xl border border-border bg-secondary">
        <CircleDashed size={22} />
      </div>
      <h3 className="font-medium text-foreground">{title}</h3>
      <p className="mt-2 max-w-md text-sm leading-6">{detail}</p>
      {action && <div className="mt-5">{action}</div>}
    </div>
  );
}
export function Loading() {
  const { t } = useUI();
  return (
    <div
      role="status"
      className="flex items-center gap-3 py-20 text-sm text-muted-foreground"
    >
      <LoaderCircle className="size-4 animate-spin" />
      {t("common.loading")}
    </div>
  );
}
export function ErrorBox({ message }: { message?: string | Error | null }) {
  const { t } = useUI();
  if (!message) return null;
  return (
    <div role="alert" className="error-box">
      <AlertTriangle className="mt-0.5 size-4 shrink-0" />
      <span>
        <strong className="mb-1 block font-medium">
          {t("common.requestFailed")}
        </strong>
        {message instanceof LocalizedError
          ? t(message.key, message.values)
          : message instanceof Error
            ? message.message
            : message}
      </span>
    </div>
  );
}
export function Field({
  label,
  hint,
  children,
}: {
  label: string;
  hint?: string;
  children: ReactNode;
}) {
  return (
    <label className="grid gap-2 text-sm">
      <span className="font-medium">{label}</span>
      {children}
      {hint && (
        <span className="text-xs leading-5 text-muted-foreground">{hint}</span>
      )}
    </label>
  );
}
export function PageTitle({
  eyebrow,
  title,
  description,
  children,
}: {
  eyebrow: string;
  title: string;
  description: string;
  children?: ReactNode;
}) {
  const pathname = usePathname();
  return (
    <AnimatedContent className="page-title" replayKey={pathname}>
      <div>
        <p className="eyebrow">{eyebrow}</p>
        <h1>{title}</h1>
        <p className="mt-2 text-sm leading-6 text-muted-foreground">
          {description}
        </p>
      </div>
      <div className="flex shrink-0 gap-2">{children}</div>
    </AnimatedContent>
  );
}
export function SectionTitle({
  title,
  detail,
  live = false,
  children,
}: {
  title: string;
  detail?: string;
  live?: boolean;
  children?: ReactNode;
}) {
  return (
    <div className="section-title">
      <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
        <h2>{title}</h2>
        {detail && (
          <span
            className={cn(
              "text-xs text-muted-foreground",
              !live && "hidden sm:block",
            )}
          >
            {detail}
          </span>
        )}
      </div>
      {children}
    </div>
  );
}
export function CopyButton({ value }: { value: string }) {
  const { t } = useUI();
  const [copied, setCopied] = useState(false);
  return (
    <Button
      variant="ghost"
      size="icon"
      aria-label={t("common.copy")}
      onClick={() => {
        void navigator.clipboard.writeText(value).then(() => {
          setCopied(true);
          setTimeout(() => setCopied(false), 1500);
        });
      }}
    >
      {copied ? <Check /> : <Copy />}
    </Button>
  );
}
export function ArtifactTable({ artifacts }: { artifacts: Artifact[] }) {
  const { t } = useUI();
  return (
    <div className="table-scroll">
      <table>
        <thead>
          <tr>
            <th>{t("artifacts.target")}</th>
            <th>{t("common.size")}</th>
            <th>SHA-256</th>
            <th className="text-right">{t("common.download")}</th>
          </tr>
        </thead>
        <tbody>
          {artifacts.map((artifact) => (
            <tr key={artifact.id}>
              <td>
                <div className="font-medium">{artifact.path}</div>
                <div className="mt-1 font-mono text-xs text-muted-foreground">
                  {artifact.target}
                </div>
              </td>
              <td className="font-mono text-xs">{bytes(artifact.size)}</td>
              <td>
                <span
                  title={artifact.sha256}
                  className="font-mono text-xs text-muted-foreground"
                >
                  {short(artifact.sha256)}…{artifact.sha256.slice(-8)}
                </span>
              </td>
              <td className="text-right">
                <Button variant="ghost" size="icon" asChild>
                  <a
                    href={`/api/v1/artifacts/${artifact.id}/download`}
                    aria-label={t("artifacts.download", {
                      path: artifact.path,
                    })}
                  >
                    <Download />
                  </a>
                </Button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
export function StepIcon({ status }: { status?: string }) {
  return status === "succeeded" ? (
    <CheckCircle2 className="size-4 text-success" />
  ) : status === "failed" ? (
    <XCircle className="size-4 text-destructive" />
  ) : status === "running" ? (
    <LoaderCircle className="size-4 animate-spin text-primary" />
  ) : (
    <Circle className="size-4 text-muted-foreground" />
  );
}
