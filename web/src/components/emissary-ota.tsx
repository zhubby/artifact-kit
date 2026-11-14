"use client";

import { useState } from "react";
import { useUI } from "@/components/providers";
import { ErrorBox, Field, SectionTitle, Loading } from "@/components/common";
import { SubmitForm } from "@/components/forms";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Modal } from "@/components/ui/dialog";
import { mutation, useRemote } from "@/lib/api";
import { asError } from "@/lib/errors";
import type { EmissaryRelease, EmissaryUpdate, Node, Pool, User } from "@/lib/types";

const retryable = (u: EmissaryUpdate) => ["failed", "rolled_back", "canceled"].includes(u.status);
const cancelable = (u: EmissaryUpdate) => !u.finished_at && ["queued", "offered", "downloading", "draining"].includes(u.status);

function UpdateActions({ update, reload }: { update: EmissaryUpdate; reload: () => Promise<unknown> }) {
  const { t } = useUI();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<Error>();
  async function act(action: string) {
    setBusy(true); setError(undefined);
    try { await mutation(`/emissary-updates/${update.id}/${action}`, {}); await reload(); }
    catch (e) { setError(asError(e)); } finally { setBusy(false); }
  }
  return <div className="space-y-2"><div className="flex gap-2">
    {retryable(update) && <Button size="sm" variant="secondary" disabled={busy} onClick={() => void act("retry")}>{t("ota.retry")}</Button>}
    {cancelable(update) && <Button size="sm" variant="secondary" disabled={busy || update.cancel_requested} onClick={() => void act("cancel")}>{t(update.cancel_requested ? "ota.cancelRequested" : "ota.cancel")}</Button>}
  </div><ErrorBox message={error} /></div>;
}

export function NodeOTA({ node, user, releases, updates, reload, actionsReady = true }: { node: Node; user: User; releases: EmissaryRelease[]; updates: EmissaryUpdate[]; reload: () => Promise<unknown>; actionsReady?: boolean }) {
  const { t } = useUI();
  const [open, setOpen] = useState(false);
  const history = updates.filter(u => u.node_id === node.id);
  const latest = history[0];
  const active = history.find(u => !u.finished_at);
  const supported = node.capabilities.includes("emissary-ota-v1");
  const desired = releases.find(r => r.id === node.emissary_desired_release)?.version;
  return <div className="mt-4 space-y-3 border-t border-border pt-4 text-xs" data-testid="node-ota">
    <dl className="grid grid-cols-2 gap-x-4 gap-y-2">
      <dt className="text-muted-foreground">{t("ota.current")}</dt><dd className="break-all font-mono">{node.emissary_current_version || node.diagnostics.emissary_version || "—"}</dd>
      <dt className="text-muted-foreground">{t("ota.target")}</dt><dd className="break-all font-mono">{active?.version || desired || t("ota.disabled")}</dd>
      <dt className="text-muted-foreground">{t("ota.launcher")}</dt><dd className="font-mono">{node.emissary_launcher_version || "—"}</dd>
      <dt className="text-muted-foreground">{t("ota.phase")}</dt><dd>{supported ? t(`ota.stages.${latest?.status || "idle"}`) : t("ota.bootstrap")}</dd>
    </dl>
    {node.ota_draining && <p className="notice">{t(active?.status === "activating" ? "ota.activatingHint" : "ota.drainHint")}</p>}
    {node.emissary_pool_paused && <p className="text-amber-600 dark:text-amber-400">{t("ota.poolPaused")}</p>}
    {latest?.error && <p className="break-words text-destructive" role="status">{latest.error}</p>}
    {latest?.status === "rolled_back" && <p>{t("ota.restored", { version: node.emissary_current_version || "—" })}</p>}
    {user.role === "admin" && actionsReady && <div className="flex flex-wrap items-start gap-2">
      <Button size="sm" variant="secondary" disabled={!!active || node.revoked} onClick={() => setOpen(true)}>{t("ota.setTarget")}</Button>
      {latest && !node.revoked && <UpdateActions update={latest} reload={reload} />}
    </div>}
    <Modal open={open} onOpenChange={setOpen} title={t("ota.setTarget")} description={t("ota.targetHint")}>
      {open && <SubmitForm onCancel={() => setOpen(false)} onSubmit={async form => {
        await mutation(`/nodes/${node.id}/emissary-update`, { release_id: form.get("release") || null }, "PATCH");
        setOpen(false); await reload();
      }}>
        <Field label={t("ota.target")}><select className="input" name="release" defaultValue={node.emissary_target_release || ""}>
          <option value="">{t("ota.inherit")}</option>{releases.map(r => <option value={r.id} key={r.id}>{r.version}</option>)}
        </select></Field>
      </SubmitForm>}
    </Modal>
  </div>;
}

export function EmissarySettings({ user }: { user: User }) {
  const { t, date } = useUI();
  const releases = useRemote<EmissaryRelease[]>("/emissary-releases");
  const pools = useRemote<Pool[]>("/pools");
  const updates = useRemote<EmissaryUpdate[]>("/emissary-updates", 3000);
  const admin = user.role === "admin";
  async function reload() { await Promise.all([releases.reload(), pools.reload(), updates.reload()]); }
  return <div className="space-y-6" data-testid="emissary-settings">
    <ErrorBox message={releases.error || pools.error || updates.error} />
    <section className="panel"><SectionTitle title={t("ota.releases")} detail={t("ota.importHint")} />
      <div className="space-y-4 p-6 pt-0">
        {admin && <SubmitForm label={t("ota.import")} onSubmit={async form => { await mutation("/emissary-releases", { tag: form.get("tag") }); await reload(); }}>
          <Field label={t("ota.tag")}><Input name="tag" required maxLength={100} placeholder="v0.2.0" /></Field>
        </SubmitForm>}
        {!releases.data ? <Loading /> : <div className="flex flex-wrap gap-2">{releases.data.map(r => <span className="tag" key={r.id}>{r.version}</span>)}{!releases.data.length && <p className="text-xs text-muted-foreground">{t("ota.noReleases")}</p>}</div>}
      </div>
    </section>
    <section className="panel"><SectionTitle title={t("ota.pools")} detail={t("ota.poolHint")} />
      <div className="grid gap-6 p-6 pt-0 md:grid-cols-2">{pools.data?.map(pool => <div className="space-y-3 rounded-lg border border-border p-4" key={`${pool.id}:${pool.emissary_target_release}:${pool.emissary_updates_paused}:${pool.emissary_update_concurrency}`}>
        <h3 className="text-sm font-medium">{pool.name}</h3>
        {admin ? <SubmitForm onSubmit={async form => {
          await mutation(`/pools/${pool.id}/emissary-update`, { release_id: form.get("release") || null, concurrency: Number(form.get("concurrency")), paused: form.get("paused") === "on" }, "PATCH"); await reload();
        }}>
          <Field label={t("ota.target")}><select className="input" name="release" defaultValue={pool.emissary_target_release || ""}><option value="">{t("ota.disabled")}</option>{releases.data?.map(r => <option key={r.id} value={r.id}>{r.version}</option>)}</select></Field>
          <Field label={t("ota.concurrency")}><Input name="concurrency" type="number" required min={1} max={32} defaultValue={pool.emissary_update_concurrency || 1} /></Field>
          <label className="flex gap-2 text-xs"><input type="checkbox" name="paused" defaultChecked={pool.emissary_updates_paused} />{t("ota.pause")}</label>
        </SubmitForm> : <p className="text-xs">{releases.data?.find(r => r.id === pool.emissary_target_release)?.version || t("ota.disabled")} · {pool.emissary_updates_paused ? t("ota.pause") : t("ota.stages.idle")}</p>}
      </div>)}</div>
    </section>
    <section className="panel"><SectionTitle title={t("ota.history")} />
      <div className="table-scroll"><table><thead><tr>{[t("common.name"),t("ota.target"),t("ota.phase"),t("common.time"),t("ota.reason"),""].map((s,i)=><th key={i}>{s}</th>)}</tr></thead><tbody>
        {updates.data?.map(u => <tr key={u.id}><td>{u.node_name}</td><td className="font-mono">{u.previous_version} → {u.version}</td><td>{t(`ota.stages.${u.status}`)}</td><td className="text-xs">{date(u.updated_at)}</td><td className="max-w-sm break-words text-xs">{u.error || "—"}</td><td>{admin && <UpdateActions update={u} reload={reload} />}</td></tr>)}
      </tbody></table></div>
    </section>
  </div>;
}
