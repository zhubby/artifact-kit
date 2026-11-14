"use client";

import { useState } from "react";
import { useRemote } from "@/lib/api";
import { useUI } from "@/components/providers";
import { ErrorBox, Loading } from "@/components/common";

interface Metadata { startup_id: string | null; received_at: string | null; metadata: Record<string, unknown> }

export function NodeMetadata({ id }: { id: string }) {
  const [open, setOpen] = useState(false);
  const { t, date } = useUI();
  const snapshot = useRemote<Metadata>(open ? `/nodes/${id}/metadata` : null, 30000);
  return <details className="mt-4 text-[11px] text-muted-foreground" onToggle={event => setOpen(event.currentTarget.open)}>
    <summary className="cursor-pointer">{t("nodes.metadata")}</summary>
    {open && <div className="mt-3 space-y-2">
      <ErrorBox message={snapshot.error} />
      {!snapshot.data && !snapshot.error && <Loading />}
      {snapshot.data && <>
        <p>{date(snapshot.data.received_at)}</p>
        {Object.keys(snapshot.data.metadata).length === 0 ? <p>{t("nodes.notReported")}</p> :
          <dl className="divide-y divide-border">{Object.entries(snapshot.data.metadata).map(([key,value]) => <div key={key} className="py-2">
            <dt className="break-all font-mono text-foreground">{key}</dt>
            <dd className="mt-1 whitespace-pre-wrap break-all font-mono">{typeof value === "string" ? value : JSON.stringify(value, null, 2)}</dd>
          </div>)}</dl>}
      </>}
    </div>}
  </details>;
}
