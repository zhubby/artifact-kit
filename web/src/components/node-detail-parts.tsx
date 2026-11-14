"use client";

import type { ReactNode } from "react";
import { RefreshCw } from "lucide-react";
import { ErrorBox } from "@/components/common";
import { useUI } from "@/components/providers";
import { Button } from "@/components/ui/button";

export function NodeFacts({ entries }: { entries: [string, ReactNode][] }) {
  const { t } = useUI();
  return (
    <dl className="node-facts">
      {entries.map(([label, value]) => (
        <div key={label}>
          <dt>{label}</dt>
          <dd>
            {value === null || value === undefined || value === ""
              ? t("nodes.unknown")
              : value}
          </dd>
        </div>
      ))}
    </dl>
  );
}

export function NodeTimestamp({ value }: { value?: string | null }) {
  const { t, locale } = useUI();
  if (!value || !Number.isFinite(Date.parse(value)))
    return <>{t("nodes.unknown")}</>;
  return (
    <time dateTime={value} title={value}>
      {new Intl.DateTimeFormat(locale, {
        dateStyle: "medium",
        timeStyle: "medium",
      }).format(new Date(value))}
    </time>
  );
}

export function NodeDataError({
  error,
  retry,
}: {
  error?: Error;
  retry: () => Promise<unknown>;
}) {
  const { t } = useUI();
  if (!error) return null;
  return (
    <div className="space-y-3">
      <p className="text-sm text-muted-foreground">
        {t("nodes.dataUnavailable")}
      </p>
      <ErrorBox message={error} />
      <Button size="sm" variant="secondary" onClick={() => void retry()}>
        <RefreshCw size={14} />
        {t("nodes.retryData")}
      </Button>
    </div>
  );
}

export function NodeRawData({
  value,
  label,
}: {
  value: unknown;
  label?: string;
}) {
  const { t } = useUI();
  return (
    <details className="node-raw-data">
      <summary>{label || t("nodes.rawData")}</summary>
      <pre>{JSON.stringify(value, null, 2)}</pre>
    </details>
  );
}
