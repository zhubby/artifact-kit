"use client";

import { useUI } from "@/components/providers";

import { useState, type FormEvent, type ReactNode } from "react";
import { CopyButton, ErrorBox, Field } from "@/components/common";
import { Button } from "@/components/ui/button";
import { Input, Textarea } from "@/components/ui/input";
import { Modal } from "@/components/ui/dialog";
import { mutation, useRemote } from "@/lib/api";
import { asError, LocalizedError } from "@/lib/errors";
import type { Credential, ModelProfile, Node, Pool, Project } from "@/lib/types";

export function SubmitForm({
  children,
  onSubmit,
  label,
  onCancel,
}: {
  children: ReactNode;
  onSubmit: (form: FormData) => Promise<void>;
  label?: string;
  onCancel?: () => void;
}) {
  const { t } = useUI();
  const [error, setError] = useState<Error>();
  const [busy, setBusy] = useState(false);
  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const form = new FormData(event.currentTarget);
    setBusy(true);
    setError(undefined);
    try {
      await onSubmit(form);
    } catch (error) {
      setError(
        error instanceof SyntaxError
          ? new LocalizedError("common.invalidJson")
          : asError(error),
      );
    } finally {
      setBusy(false);
    }
  }
  return (
    <form onSubmit={submit} className="form-stack">
      <ErrorBox message={error} />
      {children}
      <div className="form-footer">
        {onCancel && (
          <Button variant="secondary" type="button" onClick={onCancel}>
            {t("common.cancel")}
          </Button>
        )}
        <Button type="submit" disabled={busy}>
          {busy ? t("common.processing") : (label ?? t("common.save"))}
        </Button>
      </div>
    </form>
  );
}

const string = (form: FormData, name: string) => String(form.get(name) || "");
const csv = (value: string) =>
  value
    .split(",")
    .map((part) => part.trim())
    .filter(Boolean);

export function ProjectForm({
  pools,
  credentials,
  project,
  done,
}: {
  pools: Pool[];
  credentials: Credential[];
  project?: Project;
  done: () => void;
}) {
  const { t } = useUI();
  const models = useRemote<Pick<ModelProfile,"id"|"name"|"enabled">[]>("/model-options");
  const [model, setModel] = useState(project?.model_profile_id || "");
  const [bindings, setBindings] = useState(project?.source_credentials || []);
  return (
    <SubmitForm
      onCancel={done}
      onSubmit={async (form) => {
        await mutation(
          project ? `/projects/${project.id}` : "/projects",
          {
            name: string(form, "name"),
            repository_url: string(form, "repository_url"),
            credential_id: string(form, "credential_id") || null,
            pool_ids: form.getAll("pools"),
            signing_profiles: csv(string(form, "signing_profiles")),
            model_profile_id: model || null,
            build_context: string(form, "build_context"),
            source_credentials: bindings,
          },
          project ? "PATCH" : "POST",
        );
        done();
      }}
    >
      <Field label={t("projects.name")}>
        <Input
          name="name"
          defaultValue={project?.name}
          required
          maxLength={100}
          placeholder={t("projects.namePlaceholder")}
        />
      </Field>
      <Field
        label={t("projects.repository")}
        hint={t("projects.repositoryHint")}
      >
        <Input
          name="repository_url"
          defaultValue={project?.repository_url}
          required
          placeholder="https://git.example.com/team/app.git"
        />
      </Field>
      <Field label={t("projects.credential")}>
        <select
          name="credential_id"
          className="input"
          defaultValue={project?.credential_id || ""}
        >
          <option value="">{t("projects.anonymous")}</option>
          {credentials.map((credential) => (
            <option value={credential.id} key={credential.id}>
              {credential.name} · {credential.kind}
            </option>
          ))}
        </select>
      </Field>
      <fieldset>
        <legend className="mb-3 text-sm font-medium">
          {t("projects.pools")}
        </legend>
        <div className="flex flex-wrap gap-4">
          {pools.map((pool) => (
            <label key={pool.id} className="flex items-center gap-2 text-xs">
              <input
                type="checkbox"
                name="pools"
                value={pool.id}
                defaultChecked={
                  project
                    ? project.pool_ids.includes(pool.id)
                    : pool.name === "default"
                }
              />
              {pool.name}
            </label>
          ))}
        </div>
      </fieldset>
      <Field label={t("projects.signing")} hint={t("projects.signingHint")}>
        <Input
          name="signing_profiles"
          defaultValue={project?.signing_profiles.join(", ")}
          placeholder="apple-team, windows-release"
        />
      </Field>
      <Field label={t("ai.modelProfile")} hint={t("ai.projectModelHint")}>
        <select className="input" value={model} onChange={(event) => setModel(event.target.value)}>
          <option value="">{t("ai.platformDefault")}</option>
          {(models.data || []).map((profile) => <option key={profile.id} value={profile.id}>{profile.name}</option>)}
        </select>
      </Field>
      <Field label={t("ai.projectContext")} hint={t("ai.projectContextHint")}>
        <Textarea name="build_context" defaultValue={project?.build_context} maxLength={16000} />
      </Field>
      <fieldset className="space-y-3">
        <legend className="text-sm font-medium">{t("ai.submoduleCredentials")}</legend>
        <p className="text-xs leading-5 text-muted-foreground">{t("ai.submoduleHint")}</p>
        {bindings.map((binding, index) => <div className="grid gap-2" key={index}>
          <Input aria-label={t("projects.repository")} placeholder="https://git.example.com/team/library.git" value={binding.repository_url} onChange={(event) => setBindings(bindings.map((entry, i) => i === index ? {...entry, repository_url: event.target.value} : entry))} required />
          <div className="flex gap-2">
            <select className="input min-w-0 flex-1" aria-label={t("projects.credential")} required value={binding.credential_id} onChange={(event) => setBindings(bindings.map((entry, i) => i === index ? {...entry, credential_id: event.target.value} : entry))}>
              <option value="">{t("projects.credential")}</option>
              {credentials.map((credential) => <option key={credential.id} value={credential.id}>{credential.name}</option>)}
            </select>
            <Button type="button" variant="ghost" onClick={() => setBindings(bindings.filter((_, i) => i !== index))}>{t("ai.remove")}</Button>
          </div>
        </div>)}
        <Button type="button" variant="secondary" onClick={() => setBindings([...bindings, {repository_url: "", credential_id: ""}])}>{t("ai.addBinding")}</Button>
      </fieldset>
    </SubmitForm>
  );
}

export function NodeForm({
  node,
  pools,
  done,
}: {
  node: Node;
  pools: Pool[];
  done: () => void;
}) {
  const { t } = useUI();
  return (
    <SubmitForm
      onCancel={done}
      onSubmit={async (form) => {
        await mutation(
          `/nodes/${node.id}`,
          {
            draining: form.has("draining"),
            revoked: node.revoked,
            slots: Number(form.get("slots")),
            pool_id: string(form, "pool_id"),
            labels: JSON.parse(string(form, "labels")),
            signing_profiles: csv(string(form, "signing_profiles")),
          },
          "PATCH",
        );
        done();
      }}
    >
      <Field label={t("nodes.pool")}>
        <select className="input" name="pool_id" defaultValue={node.pool_id}>
          {pools.map((pool) => (
            <option value={pool.id} key={pool.id}>
              {pool.name}
            </option>
          ))}
        </select>
      </Field>
      <Field label={t("nodes.concurrency")} hint={t("nodes.concurrencyHint")}>
        <Input
          name="slots"
          type="number"
          min={1}
          max={32}
          defaultValue={node.slots}
          required
        />
      </Field>
      <Field label={t("nodes.labels")}>
        <Textarea
          name="labels"
          defaultValue={JSON.stringify(node.labels, null, 2)}
          spellCheck={false}
        />
      </Field>
      <Field label={t("projects.signing")} hint={t("nodes.signingHint")}>
        <Input
          name="signing_profiles"
          defaultValue={node.signing_profiles.join(", ")}
        />
      </Field>
      <label className="flex gap-2 text-sm">
        <input type="checkbox" name="draining" defaultChecked={node.draining} />
        {t("nodes.drain")}
      </label>
    </SubmitForm>
  );
}

export function CredentialForm({ done }: { done: () => void }) {
  const { t } = useUI();
  const [kind, setKind] = useState("https");
  return (
    <SubmitForm
      onCancel={done}
      onSubmit={async (form) => {
        await mutation("/credentials", {
          name: string(form, "name"),
          kind,
          username: string(form, "username"),
          secret: string(form, "secret"),
          known_hosts: string(form, "known_hosts"),
        });
        done();
      }}
    >
      <Field label={t("credentials.name")}>
        <Input name="name" required maxLength={100} />
      </Field>
      <Field label={t("credentials.method")}>
        <select
          className="input"
          value={kind}
          onChange={(event) => setKind(event.target.value)}
        >
          <option value="https">{t("credentials.https")}</option>
          <option value="ssh">{t("credentials.ssh")}</option>
        </select>
      </Field>
      {kind === "https" && (
        <Field label={t("credentials.username")}>
          <Input name="username" autoComplete="off" />
        </Field>
      )}
      <Field
        label={
          kind === "ssh" ? t("credentials.privateKey") : t("credentials.secret")
        }
        hint={t("credentials.secretHint")}
      >
        {kind === "ssh" ? (
          <Textarea
            name="secret"
            required
            autoComplete="off"
            spellCheck={false}
          />
        ) : (
          <Input
            name="secret"
            type="password"
            required
            autoComplete="new-password"
          />
        )}
      </Field>
      {kind === "ssh" && (
        <Field
          label={t("credentials.knownHosts")}
          hint={t("credentials.knownHostsHint")}
        >
          <Textarea name="known_hosts" required spellCheck={false} />
        </Field>
      )}
    </SubmitForm>
  );
}

export function SecretDialog({
  value,
  title,
  close,
  hint,
}: {
  value?: string;
  title: string;
  close: () => void;
  hint?: string;
}) {
  const { t } = useUI();
  return (
    <Modal
      open={!!value}
      onOpenChange={(open) => {
        if (!open) close();
      }}
      title={title}
      description={t("secrets.description")}
    >
      <div className="flex items-start gap-2 rounded-md border border-border bg-background p-3">
        <code className="min-w-0 flex-1 break-all text-xs leading-6 text-primary">
          {value}
        </code>
        <CopyButton value={value || ""} />
      </div>
      <div className="mt-5 text-xs leading-6 text-muted-foreground">
        {hint || t("secrets.emissaryHint")}
      </div>
      <Button className="mt-6 w-full" onClick={close}>
        {t("secrets.saved")}
      </Button>
    </Modal>
  );
}
