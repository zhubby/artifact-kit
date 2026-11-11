"use client";

import { useUI } from "@/components/providers";
import { ErrorBox, Field, SectionTitle, Status } from "@/components/common";
import { SubmitForm } from "@/components/forms";
import { Textarea } from "@/components/ui/input";
import { mutation, useRemote } from "@/lib/api";
import type { Build, PlanningHistory, User } from "@/lib/types";

export function BuildPlanning({build, user, refresh}: {build: Build; user: User; refresh: () => Promise<void>}) {
  const { t, number, date } = useUI();
  const history = useRemote<PlanningHistory>(`/builds/${build.id}/plans`, 3000);
  const current = history.data?.plans.at(-1);
  const run = history.data?.runs.at(-1);
  return <section className="panel mb-6 min-w-0">
    <SectionTitle title={t("ai.plan")} detail={t("ai.revisionCount", {count: build.revision_count || 0})}>{run && <span className="tag">{t("ai.callCount", {count: run.calls})}</span>}</SectionTitle>
    <div className="space-y-4 p-5">
      <p className="whitespace-pre-wrap text-sm leading-6">{build.goal}</p>
      <ErrorBox message={history.error} />
      <p className="text-xs leading-5 text-muted-foreground">{t("ai.zeroIntrusion")}</p>
      {build.source_snapshot_id && <p className="break-all font-mono text-xs text-muted-foreground">{t("ai.snapshot")} {build.source_snapshot_id}</p>}
      {build.status === "needs_input" && build.clarification && <div className="notice space-y-4">
        <p className="whitespace-pre-wrap text-sm">{build.clarification.question}</p>
        {user.role !== "viewer" && <SubmitForm label={t("ai.continue")} onSubmit={async (form) => {await mutation(`/builds/${build.id}/clarifications`, {answer: String(form.get("answer") || "")}); await refresh(); await history.reload();}}><Field label={t("ai.answer")}><Textarea name="answer" required maxLength={8000} /></Field></SubmitForm>}
      </div>}
      {run && <div className="flex flex-wrap gap-4 text-xs text-muted-foreground"><Status value={run.state} small /><span>{t("ai.tokens", {count: number(run.usage?.totalTokens || 0)})}</span>{run.error && <span>{run.error}</span>}</div>}
      {current ? <>
        <p className="whitespace-pre-wrap text-sm leading-6">{current.contents.summary}</p>
        <ul className="space-y-2 text-xs text-muted-foreground">{current.contents.evidence.map((item, index) => <li key={index}><code className="break-all text-primary">{item.path}</code><span className="ml-2">{item.reason}</span></li>)}</ul>
        <details><summary className="cursor-pointer text-sm text-primary">{t("ai.executionPlan")}</summary><div className="mt-4 space-y-4">{Object.entries(current.contents.recipe.targets).map(([name, target]) => <div key={name}><h3 className="mb-2 text-sm font-medium">{name} · {target.os}/{target.arch}</h3>{target.steps.map((step, index) => <div className="mb-3" key={index}><p className="mb-1 text-xs text-muted-foreground">{step.name} · {step.phase}</p><pre className="overflow-x-auto whitespace-pre-wrap break-all rounded-md bg-background p-3 text-xs">{step.run}</pre></div>)}</div>)}{current.contents.scripts.map((script) => <div key={script.name}><p className="mb-2 font-mono text-xs">{script.name}</p><pre className="overflow-x-auto whitespace-pre-wrap break-all rounded-md bg-background p-3 text-xs">{script.content}</pre></div>)}</div></details>
        <details><summary className="cursor-pointer text-sm text-muted-foreground">{t("ai.planHistory")}</summary><div className="mt-3 space-y-2">{history.data?.plans.map((entry) => <details key={entry.id}><summary className="cursor-pointer text-xs">{t("ai.version", {version: entry.revision})} · {date(entry.created_at)}</summary><p className="mt-2 whitespace-pre-wrap text-xs leading-5">{entry.contents.summary}</p><pre className="mt-2 overflow-x-auto whitespace-pre-wrap break-all text-xs">{JSON.stringify(entry.contents, null, 2)}</pre></details>)}</div></details>
      </> : <p className="text-sm text-muted-foreground">{t("ai.planningHint")}</p>}
    </div>
  </section>;
}
