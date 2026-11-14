"use client";

import { useRemote } from "@/lib/api";
import type { NodeMetadataSnapshot } from "@/lib/types";
import { useUI } from "@/components/providers";
import { Loading } from "@/components/common";
import {
  NodeDataError,
  NodeFacts,
  NodeTimestamp,
} from "@/components/node-detail-parts";

export function NodeMetadata({ id }: { id: string }) {
  const { t } = useUI();
  const snapshot = useRemote<NodeMetadataSnapshot>(
    `/nodes/${id}/metadata`,
    30000,
  );
  return (
    <div className="space-y-6">
      <NodeDataError error={snapshot.error} retry={snapshot.reload} />
      {!snapshot.data && !snapshot.error && <Loading />}
      {snapshot.data && (
        <>
          <NodeFacts
            entries={[
              [t("nodes.startupId"), snapshot.data.startup_id],
              [t("nodes.metadataDigest"), snapshot.data.digest],
              [
                t("nodes.receivedAt"),
                <NodeTimestamp
                  key="received"
                  value={snapshot.data.received_at}
                />,
              ],
            ]}
          />
          {Object.keys(snapshot.data.metadata).length === 0 ? (
            <p className="node-empty">{t("nodes.notReported")}</p>
          ) : (
            <dl className="node-metadata-list">
              {Object.entries(snapshot.data.metadata)
                .sort(([a], [b]) => a.localeCompare(b))
                .map(([key, value]) => (
                  <div key={key}>
                    <dt>{key}</dt>
                    <dd>
                      <pre>
                        {typeof value === "string"
                          ? value
                          : JSON.stringify(value, null, 2)}
                      </pre>
                    </dd>
                  </div>
                ))}
            </dl>
          )}
        </>
      )}
    </div>
  );
}
