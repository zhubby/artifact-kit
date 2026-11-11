"use client";

import { useState } from "react";
import { Plus, KeyRound, Trash2 } from "lucide-react";
import { useUI } from "@/components/providers";
import { Empty, ErrorBox, Field, Loading, SectionTitle, Status } from "@/components/common";
import { SubmitForm, SecretDialog } from "@/components/forms";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Modal } from "@/components/ui/dialog";
import { mutation, useRemote } from "@/lib/api";
import { asError } from "@/lib/errors";
import type { AIWorker, ModelProfile } from "@/lib/types";

export function AISettings() {
  const { t, date, number } = useUI();
  const models = useRemote<ModelProfile[]>("/model-profiles");
  const workers = useRemote<AIWorker[]>("/ai-workers", 5000);
  const [editing, setEditing] = useState<ModelProfile | "new">();
  const [secret, setSecret] = useState<string>();
  const [error, setError] = useState<Error>();
  async function action(path: string, body: unknown, method = "POST") {
    setError(undefined);
    try { await mutation(path, body, method); await Promise.all([models.reload(), workers.reload()]); }
    catch (error) { setError(asError(error)); }
  }
  return <div className="space-y-6">
    <ErrorBox message={error || models.error || workers.error} />
    <div className="notice">{t("ai.settingsDescription")}</div>
    <section className="panel">
      <SectionTitle title={t("ai.models")}><Button size="sm" onClick={() => setEditing("new")}><Plus />{t("ai.addModel")}</Button></SectionTitle>
      {!models.data ? <div className="px-5"><Loading /></div> : models.data.length === 0 ? <Empty title={t("ai.noModels")} detail={t("ai.noModelsHint")} /> : <div className="table-scroll"><table>
        <thead><tr><th>{t("common.name")}</th><th>{t("ai.model")}</th><th>{t("ai.budget")}</th><th>{t("common.status")}</th><th /></tr></thead>
        <tbody>{models.data.map((profile) => <tr key={profile.id}>
          <td><button className="font-medium text-primary" onClick={() => setEditing(profile)}>{profile.name}</button><p className="mt-1 break-all text-xs text-muted-foreground">{profile.base_url}</p></td>
          <td><span className="font-mono text-xs">{profile.model}</span><p className="mt-1 text-xs text-muted-foreground">{profile.api}</p></td>
          <td className="text-xs">{t("ai.budgetValue", {calls: profile.max_calls, seconds: profile.timeout_seconds})}</td>
          <td><span className="tag">{profile.enabled ? t("ai.enabled") : t("ai.disabled")}</span>{profile.is_default && <span className="ml-2 tag">{t("ai.default")}</span>}</td>
          <td><Button variant="ghost" size="icon" aria-label={t("ai.deleteModel", {name: profile.name})} onClick={() => void action(`/model-profiles/${profile.id}`, {}, "DELETE")}><Trash2 /></Button></td>
        </tr>)}</tbody>
      </table></div>}
    </section>
    <section className="panel">
      <SectionTitle title={t("ai.workers")} detail={t("ai.workerHint")}><Button variant="secondary" size="sm" onClick={() => {
        setError(undefined);
        void mutation<{token: string}>("/ai-enrollment-tokens", {}).then((result) => setSecret(result.token)).catch((error) => setError(asError(error)));
      }}><KeyRound />{t("ai.enrollWorker")}</Button></SectionTitle>
      {!workers.data ? <div className="px-5"><Loading /></div> : workers.data.length === 0 ? <Empty title={t("ai.noWorkers")} detail={t("ai.noWorkersHint")} /> : <div className="table-scroll"><table>
        <thead><tr><th>{t("common.name")}</th><th>{t("common.status")}</th><th>{t("ai.lastSeen")}</th><th>{t("ai.certificateExpires")}</th><th /></tr></thead>
        <tbody>{workers.data.map((worker) => <tr key={worker.id}>
          <td className="font-medium">{worker.name}{worker.active_run && <p className="mt-1 font-mono text-xs text-muted-foreground">{worker.active_run.slice(0, 8)}</p>}</td>
          <td><Status value={worker.revoked ? "revoked" : worker.draining ? "draining" : worker.online ? "online" : "offline"} /></td>
          <td className="text-xs">{date(worker.last_seen)}</td><td className="text-xs">{date(worker.certificate_expires_at)}</td>
          <td><div className="flex gap-2"><Button size="sm" variant="secondary" disabled={worker.revoked} onClick={() => void action(`/ai-workers/${worker.id}`, {draining: !worker.draining, revoked: worker.revoked}, "PATCH")}>{worker.draining ? t("ai.resume") : t("nodes.drain")}</Button><Button size="sm" variant="ghost" disabled={worker.revoked} onClick={() => void action(`/ai-workers/${worker.id}`, {draining: true, revoked: true}, "PATCH")}>{t("common.revoke")}</Button></div></td>
        </tr>)}</tbody>
      </table></div>}
      {workers.data && <p className="border-t border-border p-4 text-xs text-muted-foreground">{t("ai.workerCount", {count: number(workers.data.length)})}</p>}
    </section>
    <Modal open={!!editing} onOpenChange={(open) => {if (!open) setEditing(undefined);}} title={editing === "new" ? t("ai.addModel") : t("ai.editModel")} description={t("ai.modelSecretHint")}>
      {editing && <ModelForm profile={editing === "new" ? undefined : editing} defaultModel={!models.data?.some((profile) => profile.is_default)} done={() => {setEditing(undefined); void models.reload();}} />}
    </Modal>
    <SecretDialog value={secret} title={t("ai.enrollmentTitle")} close={() => setSecret(undefined)} hint={t("ai.enrollmentHint")} />
  </div>;
}

function ModelForm({profile, defaultModel, done}: {profile?: ModelProfile; defaultModel: boolean; done: () => void}) {
  const { t } = useUI();
  const [api, setApi] = useState(profile?.api || "openai-completions");
  const [endpoint, setEndpoint] = useState(profile?.base_url || "https://api.openai.com/v1");
  return <SubmitForm onCancel={done} onSubmit={async (form) => {
    const value = (name: string) => String(form.get(name) || "");
    await mutation(profile ? `/model-profiles/${profile.id}` : "/model-profiles", {
      name: value("name"), api, base_url: endpoint, model: value("model"), api_key: value("api_key"),
      enabled: form.has("enabled"), is_default: form.has("is_default"),
      max_calls: Number(value("max_calls")), timeout_seconds: Number(value("timeout_seconds")),
      parameters: {context_window: Number(value("context_window")), max_tokens: Number(value("max_tokens")), thinking_level: value("thinking_level"), ...(profile?.parameters.temperature === undefined ? {} : {temperature: profile.parameters.temperature}), ...(value("requests_per_minute") ? {requests_per_minute: Number(value("requests_per_minute"))} : {})},
    }, profile ? "PATCH" : "POST"); done();
  }}>
    <Field label={t("common.name")}><Input name="name" required maxLength={100} defaultValue={profile?.name} /></Field>
    <Field label={t("ai.api")}><select className="input" value={api} onChange={(event) => {setApi(event.target.value); if (!profile) setEndpoint(event.target.value === "anthropic-messages" ? "https://api.anthropic.com" : "https://api.openai.com/v1");}}><option value="openai-completions">OpenAI Chat Completions</option><option value="openai-responses">OpenAI Responses</option><option value="anthropic-messages">Anthropic Messages</option></select></Field>
    <Field label={t("ai.endpoint")}><Input value={endpoint} onChange={(event) => setEndpoint(event.target.value)} required type="url" /></Field>
    <Field label={t("ai.model")}><Input name="model" required maxLength={200} defaultValue={profile?.model} /></Field>
    <Field label="API Key" hint={t("ai.keyHint")}><Input name="api_key" type="password" autoComplete="new-password" required={!profile} /></Field>
    <div className="grid grid-cols-2 gap-3">
      <Field label={t("ai.maxCalls")}><Input name="max_calls" type="number" required min={1} max={100} defaultValue={profile?.max_calls || 20} /></Field>
      <Field label={t("ai.timeout")}><Input name="timeout_seconds" type="number" required min={30} max={3600} defaultValue={profile?.timeout_seconds || 600} /></Field>
      <Field label={t("ai.contextWindow")}><Input name="context_window" type="number" required min={1024} defaultValue={profile?.parameters.context_window || 128000} /></Field>
      <Field label={t("ai.maxTokens")}><Input name="max_tokens" type="number" required min={256} defaultValue={profile?.parameters.max_tokens || 8192} /></Field>
      <Field label={t("ai.rpm")} hint={t("ai.rpmHint")}><Input name="requests_per_minute" type="number" min={1} max={600} defaultValue={profile?.parameters.requests_per_minute} /></Field>
    </div>
    <Field label={t("ai.thinking")}><select className="input" name="thinking_level" defaultValue={profile?.parameters.thinking_level || "off"}>{["off", "minimal", "low", "medium", "high", "xhigh", "max"].map((level) => <option key={level} value={level}>{level}</option>)}</select></Field>
    <label className="flex gap-2 text-sm"><input type="checkbox" name="enabled" defaultChecked={profile?.enabled ?? true} />{t("ai.enabled")}</label>
    <label className="flex gap-2 text-sm"><input type="checkbox" name="is_default" defaultChecked={profile?.is_default ?? defaultModel} />{t("ai.makeDefault")}</label>
  </SubmitForm>;
}
