"use client";

import { useEffect, useRef, useState, type FormEvent, type ReactNode } from "react";
import { useRouter, useSearchParams } from "next/navigation";
import { ArrowLeft, ArrowRight, Check, GitBranch, LoaderCircle, Pencil, Play, RefreshCw } from "lucide-react";
import { Empty, ErrorBox, Field, PageTitle } from "@/components/common";
import { usePreferences, useUI } from "@/components/providers";
import { Button } from "@/components/ui/button";
import { Input, Textarea } from "@/components/ui/input";
import { api, mutation } from "@/lib/api";
import { asError, LocalizedError } from "@/lib/errors";
import type { Project, User } from "@/lib/types";

type SourceMode = "branches" | "tags" | "manual";
type GitRef = { name: string; ref: string; oid: string };
type RepositoryRefs = { default_ref: string | null; branches: GitRef[]; tags: GitRef[] };

const platforms = [
  { value: "darwin/arm64", label: "platformMacArm" },
  { value: "darwin/amd64", label: "platformMacIntel" },
  { value: "linux/amd64", label: "platformLinuxIntel" },
  { value: "linux/arm64", label: "platformLinuxArm" },
  { value: "windows/amd64", label: "platformWindows" },
] as const;
const questions = ["projectQuestion", "sourceQuestion", "goalQuestion", "platformQuestion", "reviewQuestion"] as const;
const hints = ["projectHint", "sourceHint", "goalHint", "platformHint", "reviewHint"] as const;
const editLabels = ["editProject", "editSource", "editGoal", "editPlatforms"] as const;
const byteLength = (value: string) => new TextEncoder().encode(value).length;

function useRepositoryRefs(projectID: string) {
  const [revision, setRevision] = useState(0);
  const [snapshot, setSnapshot] = useState<{
    projectID: string;
    data?: RepositoryRefs;
    error?: Error;
    loading: boolean;
  }>({ projectID: "", loading: false });
  useEffect(() => {
    if (!projectID) return;
    let active = true;
    const controller = new AbortController();
    setSnapshot({ projectID, loading: true });
    void api<RepositoryRefs>(`/projects/${encodeURIComponent(projectID)}/refs`, { signal: controller.signal })
      .then(data => {
        if (active) setSnapshot({ projectID, data, loading: false });
      })
      .catch(error => {
        if (active) setSnapshot({ projectID, error: asError(error), loading: false });
      });
    return () => {
      active = false;
      controller.abort();
    };
  }, [projectID, revision]);
  const current = snapshot.projectID === projectID ? snapshot : undefined;
  return {
    data: current?.data,
    error: current?.error,
    loading: current?.loading ?? Boolean(projectID),
    reload: () => setRevision(previous => previous + 1),
  };
}

function Choice({ name, value, checked, onChange, label, children }: {
  name: string;
  value: string;
  checked: boolean;
  onChange: () => void;
  label: string;
  children?: ReactNode;
}) {
  return <label className="wizard-choice" data-selected={checked}>
    <input type="radio" name={name} value={value} checked={checked} onChange={onChange} aria-label={label} />
    <span className="min-w-0 flex-1"><span className="block font-medium">{label}</span>{children}</span>
    {checked && <Check className="size-4 shrink-0 text-primary" aria-hidden="true" />}
  </label>;
}

export function BuildWizard({ projects, user }: { projects: Project[]; user: User }) {
  const searchParams = useSearchParams();
  const initialProjectID = searchParams.get("project") || "";
  return <Wizard key={initialProjectID} projects={projects} user={user} initialProjectID={initialProjectID} />;
}

function Wizard({ projects, user, initialProjectID }: { projects: Project[]; user: User; initialProjectID: string }) {
  const { t, locale } = useUI();
  const { motion } = usePreferences();
  const router = useRouter();
  const initialProject = initialProjectID
    ? projects.find(project => project.id === initialProjectID)
    : projects.length === 1 ? projects[0] : undefined;
  const [projectID, setProjectID] = useState(initialProject?.id || "");
  const [step, setStep] = useState(initialProject ? 1 : 0);
  const [returnToReview, setReturnToReview] = useState(false);
  const [sourceMode, setSourceMode] = useState<SourceMode>("branches");
  const [selectedRef, setSelectedRef] = useState("");
  const [sourceConfirmed, setSourceConfirmed] = useState(false);
  const [manualRef, setManualRef] = useState("");
  const [search, setSearch] = useState("");
  const [goalTemplate, setGoalTemplate] = useState("");
  const [goal, setGoal] = useState("");
  const [automaticPlatforms, setAutomaticPlatforms] = useState(true);
  const [selectedPlatforms, setSelectedPlatforms] = useState<string[]>([]);
  const [publish, setPublish] = useState(false);
  const [version, setVersion] = useState("");
  const [parameters, setParameters] = useState("{}");
  const [error, setError] = useState<Error>();
  const [busy, setBusy] = useState(false);
  const busyRef = useRef(false);
  const sourceInitialized = useRef(false);
  const questionRef = useRef<HTMLHeadingElement>(null);
  const motionRef = useRef(motion);
  motionRef.current = motion;
  const refs = useRepositoryRefs(user.role === "viewer" ? "" : projectID);
  const project = projects.find(project => project.id === projectID);
  const reference = (sourceMode === "manual" ? manualRef : selectedRef).trim();
  const cancelPath = initialProjectID && initialProject ? `/projects/${initialProjectID}` : "/builds";

  useEffect(() => {
    if (!refs.data || sourceInitialized.current) return;
    sourceInitialized.current = true;
    if (refs.data.default_ref) setSelectedRef(refs.data.default_ref);
  }, [refs.data]);
  useEffect(() => {
    questionRef.current?.focus({ preventScroll: true });
    questionRef.current?.scrollIntoView({ behavior: motionRef.current ? "smooth" : "instant", block: "nearest" });
  }, [step]);

  function selectProject(nextProjectID: string) {
    if (projectID !== nextProjectID) {
      setProjectID(nextProjectID);
      setSelectedRef("");
      setSourceConfirmed(false);
      setManualRef("");
      setSourceMode("branches");
      setSearch("");
      sourceInitialized.current = false;
    }
    setError(undefined);
  }
  function edit(index: number) {
    setReturnToReview(step === 4 || returnToReview);
    setStep(index);
    setError(undefined);
  }
  function validSource() {
    if (!reference || byteLength(reference) > 255 || reference.startsWith("-") || /[\0\n\r]/.test(reference)) return false;
    return sourceMode === "manual" || Boolean(refs.data?.[sourceMode].some(entry => entry.ref === reference));
  }
  function validate(index: number) {
    if (index === 0 && !project) throw new LocalizedError("wizard.invalidProject");
    if (index === 1 && !validSource()) throw new LocalizedError("wizard.invalidRef");
    if (index === 2 && (!goal.trim() || byteLength(goal.trim()) > 16000)) throw new LocalizedError("wizard.invalidGoal");
    if (index === 3 && !automaticPlatforms && !selectedPlatforms.length) throw new LocalizedError("wizard.invalidPlatforms");
  }
  function advance() {
    try {
      validate(step);
      setError(undefined);
      if (step === 1) setSourceConfirmed(true);
      if (returnToReview && (step !== 0 || (sourceConfirmed && validSource()))) {
        setStep(4);
        setReturnToReview(false);
      } else {
        setStep(previous => previous + 1);
      }
    } catch (error) {
      setError(asError(error));
    }
  }
  function parseParameters(): Record<string, string> {
    let parsed: unknown;
    try {
      parsed = JSON.parse(parameters.trim() || "{}");
    } catch {
      throw new LocalizedError("common.invalidJson");
    }
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)
      || byteLength(JSON.stringify(parsed)) > 16384
      || Object.entries(parsed).some(([name, value]) => !/^[A-Z][A-Z0-9_]{0,63}$/.test(name)
        || typeof value !== "string" || byteLength(value) > 4096 || value.includes("\0"))) {
      throw new LocalizedError("wizard.invalidParameters");
    }
    return parsed as Record<string, string>;
  }
  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (step !== 4 || busyRef.current || user.role === "viewer") return;
    try {
      for (let index = 0; index < 4; index++) validate(index);
      if (publish && (!version.trim() || byteLength(version.trim()) > 100 || /\p{Cc}/u.test(version.trim()))) {
        throw new LocalizedError("wizard.invalidVersion");
      }
      const parsed = parseParameters();
      busyRef.current = true;
      setBusy(true);
      setError(undefined);
      const result = await mutation<{ id: string }>(`/projects/${projectID}/builds`, {
        ref: reference,
        goal: goal.trim(),
        target_requirements: automaticPlatforms ? [] : selectedPlatforms.map(value => {
          const [os, arch] = value.split("/");
          return { os, arch };
        }),
        parameters: parsed,
        release_version: publish ? version.trim() : null,
      });
      router.replace(`/builds/${result.id}`);
    } catch (error) {
      busyRef.current = false;
      setBusy(false);
      setError(asError(error));
    }
  }

  if (user.role === "viewer") return <Empty title={t("wizard.title")} detail={t("wizard.noPermission")} />;
  if (!projects.length) return <>
    <Empty title={t("wizard.noProjects")} detail={t("wizard.noProjectsHint")} />
    <Button variant="secondary" onClick={() => router.push("/projects")}>{t("navigation.projects")}</Button>
  </>;

  const sourceEntries = sourceMode === "manual" ? [] : (refs.data?.[sourceMode] || [])
    .filter(entry => entry.name.toLocaleLowerCase().includes(search.toLocaleLowerCase()))
    .sort((left, right) => Number(right.ref === refs.data?.default_ref) - Number(left.ref === refs.data?.default_ref));
  const visibleSourceEntries = sourceEntries.slice(0, 100);
  const selectedEntry = sourceEntries.find(entry => entry.ref === selectedRef);
  if (selectedEntry && !visibleSourceEntries.includes(selectedEntry)) {
    visibleSourceEntries.splice(-1, 1, selectedEntry);
  }
  const platformDescription = automaticPlatforms ? t("wizard.autoPlatforms") : new Intl.ListFormat(locale).format(platforms
    .filter(platform => selectedPlatforms.includes(platform.value)).map(platform => t(`wizard.${platform.label}`)));
  const answers = [project?.name || t("wizard.notSelected"), reference || t("wizard.notSelected"), goal || t("wizard.notDescribed"), platformDescription || t("wizard.notSelected")];
  const summaryLabels = [t("projects.project"), t("builds.source"), t("ai.goal"), t("builds.platforms")];

  return <>
    <PageTitle eyebrow={t("wizard.eyebrow")} title={t("wizard.title")} description={t("wizard.description")}>
      <Button variant="ghost" disabled={busy} onClick={() => router.push(cancelPath)}><ArrowLeft />{t("wizard.cancel")}</Button>
    </PageTitle>
    <div className="wizard-layout">
      <div className="min-w-0">
        <ol className="wizard-transcript" aria-label={t("wizard.conversation")}>
          {questions.slice(0, Math.min(step, 4)).map((question, index) => <li key={question}>
            <p className="wizard-past-question"><span className="wizard-step-dot"><Check size={12} aria-hidden="true" /></span>{t(`wizard.${question}`)}</p>
            <div className="wizard-answer"><p className={index === 1 ? "font-mono text-xs" : "text-sm"}>{answers[index]}</p>
              <Button type="button" variant="ghost" size="icon" aria-label={t(`wizard.${editLabels[index]}`)} disabled={busy} onClick={() => edit(index)}><Pencil /></Button>
            </div>
          </li>)}
        </ol>
        <section className="wizard-current" aria-label={t("wizard.currentQuestion")}>
          <p className="eyebrow mb-3">{t("wizard.progress", { current: step + 1, total: 5 })}</p>
          <h2 ref={questionRef} tabIndex={-1} className="wizard-question">{t(`wizard.${questions[step]}`)}</h2>
          <p className="mb-6 mt-3 text-sm leading-6 text-muted-foreground">{t(`wizard.${hints[step]}`)}</p>
          <ErrorBox message={error} />
          {step === 0 && <fieldset className="wizard-options" aria-label={t("projects.project")}>
            {initialProjectID && !initialProject && <p className="text-sm text-destructive">{t("wizard.invalidProject")}</p>}
            {projects.map(candidate => <Choice key={candidate.id} name="project_id" value={candidate.id} checked={projectID === candidate.id} onChange={() => selectProject(candidate.id)} label={candidate.name}>
              <span className="wizard-choice-detail break-all">{candidate.repository_url}</span>
            </Choice>)}
          </fieldset>}
          {step === 1 && <div className="space-y-4">
            <div className="flex flex-wrap items-center justify-between gap-3">
              <div role="group" aria-label={t("wizard.sourceCategory")} className="wizard-segments">
                {(["branches", "tags", "manual"] as const).map(mode => <Button key={mode} type="button" size="sm" variant={sourceMode === mode ? "secondary" : "ghost"} aria-pressed={sourceMode === mode} onClick={() => {
                  sourceInitialized.current = true;
                  setSourceMode(mode);
                  setSearch("");
                  setError(undefined);
                }}>{t(`wizard.${mode}`)}</Button>)}
              </div>
              <Button type="button" variant="ghost" size="icon" disabled={refs.loading} onClick={refs.reload} aria-label={t("wizard.refreshRefs")}><RefreshCw className={refs.loading ? "animate-spin" : ""} /></Button>
            </div>
            {sourceMode === "manual" ? <Field label={t("builds.ref")} hint={t("wizard.manualHint")}>
              <Input name="ref" value={manualRef} onChange={event => { setManualRef(event.target.value); setError(undefined); }} maxLength={255} placeholder={t("wizard.manualPlaceholder")} autoComplete="off" />
            </Field> : <>
              <ErrorBox message={refs.error} />
              {refs.loading ? <p role="status" className="flex items-center gap-2 py-6 text-sm text-muted-foreground"><LoaderCircle className="size-4 animate-spin" />{t("wizard.loadingRefs")}</p> : refs.data && <>
                <Input type="search" aria-label={t("wizard.searchRefs")} placeholder={t("wizard.searchRefs")} value={search} onChange={event => setSearch(event.target.value)} />
                <fieldset className="wizard-options wizard-ref-list" aria-label={t(`wizard.${sourceMode}`)}>
                  {visibleSourceEntries.map(entry => <label className="wizard-choice" data-selected={selectedRef === entry.ref} key={entry.ref}>
                    <input type="radio" name="source_ref" checked={selectedRef === entry.ref} value={entry.ref} aria-label={`${t(sourceMode === "branches" ? "wizard.branch" : "wizard.tag")} ${entry.name}`} onChange={() => { setSelectedRef(entry.ref); setError(undefined); }} />
                    <GitBranch className="size-4 shrink-0 text-muted-foreground" aria-hidden="true" />
                    <span className="min-w-0 flex-1"><span className="block break-all text-sm font-medium">{entry.name}</span><span className="wizard-choice-detail font-mono" title={entry.oid}>{entry.oid.slice(0, 8)}</span></span>
                    {entry.ref === refs.data?.default_ref && <span className="tag">{t("wizard.defaultBranch")}</span>}
                  </label>)}
                  {!sourceEntries.length && <p className="py-5 text-sm leading-6 text-muted-foreground">{t("wizard.noRefs")}</p>}
                </fieldset>
                {sourceEntries.length > 100 && <p className="text-xs text-muted-foreground">{t("wizard.refsLimited", { count: 100 })}</p>}
              </>}
            </>}
          </div>}
          {step === 2 && <div className="space-y-5">
            <fieldset className="wizard-options" aria-label={t("ai.goal")}>
              {["goalBuild", "goalTest", "goalCustom"].map(template => <Choice key={template} name="goal_template" value={template} checked={goalTemplate === template} label={t(`wizard.${template}`)} onChange={() => {
                setGoalTemplate(template);
                if (template !== "goalCustom") setGoal(t(`wizard.${template}Text`));
                setError(undefined);
              }} />)}
            </fieldset>
            <Field label={t("ai.goal")} hint={t("wizard.goalInputHint")}><Textarea name="goal" value={goal} onChange={event => { setGoal(event.target.value); setError(undefined); }} maxLength={16000} placeholder={t("ai.goalPlaceholder")} className="min-h-32 font-sans text-sm leading-6" /></Field>
          </div>}
          {step === 3 && <div className="space-y-4">
            <fieldset className="wizard-options" aria-label={t("builds.platforms")}>
              <Choice name="platform_mode" value="auto" checked={automaticPlatforms} onChange={() => { setAutomaticPlatforms(true); setError(undefined); }} label={t("wizard.autoPlatforms")}><span className="wizard-choice-detail">{t("wizard.autoPlatformsHint")}</span></Choice>
              <Choice name="platform_mode" value="manual" checked={!automaticPlatforms} onChange={() => { setAutomaticPlatforms(false); setError(undefined); }} label={t("wizard.manualPlatforms")}><span className="wizard-choice-detail">{t("wizard.manualPlatformsHint")}</span></Choice>
            </fieldset>
            {!automaticPlatforms && <fieldset className="wizard-options" aria-label={t("builds.platforms")}>
              {platforms.map(platform => <label className="wizard-choice" data-selected={selectedPlatforms.includes(platform.value)} key={platform.value}>
                <input type="checkbox" name="target_platform" value={platform.value} checked={selectedPlatforms.includes(platform.value)} onChange={event => {
                  setSelectedPlatforms(previous => event.target.checked ? [...previous, platform.value] : previous.filter(value => value !== platform.value));
                  setError(undefined);
                }} />{t(`wizard.${platform.label}`)}
              </label>)}
            </fieldset>}
          </div>}
          {step === 4 && <form onSubmit={submit} noValidate>
            <fieldset disabled={busy} className="space-y-5">
              <div className="wizard-review">
                {summaryLabels.map((label, index) => <div key={label}><span className="text-xs text-muted-foreground">{label}</span><p className="whitespace-pre-wrap break-all text-sm leading-6">{answers[index]}</p><Button type="button" variant="ghost" size="icon" aria-label={t(`wizard.${editLabels[index]}`)} onClick={() => edit(index)}><Pencil /></Button></div>)}
              </div>
              <label className="wizard-choice" data-selected={publish}><input type="checkbox" name="publish" checked={publish} onChange={event => { setPublish(event.target.checked); setError(undefined); }} /><span><span className="block text-sm font-medium">{t("wizard.withRelease")}</span><span className="wizard-choice-detail">{t("wizard.releaseDescription")}</span></span></label>
              {publish && <Field label={t("builds.version")} hint={t("builds.releaseHint")}><Input name="release_version" value={version} onChange={event => { setVersion(event.target.value); setError(undefined); }} maxLength={100} placeholder={t("builds.releasePlaceholder")} /></Field>}
              <details className="wizard-advanced"><summary>{t("wizard.advanced")}</summary><div className="mt-4"><Field label={t("builds.parameters")} hint={t("wizard.advancedHint", { example: '{"BUILD_MODE":"release"}' })}><Textarea name="parameters" value={parameters} onChange={event => { setParameters(event.target.value); setError(undefined); }} spellCheck={false} /></Field></div></details>
              <p className="text-xs leading-5 text-muted-foreground">{t("wizard.submitHint")}</p>
              <div className="wizard-actions"><Button type="button" variant="secondary" onClick={() => { setStep(3); setError(undefined); }}><ArrowLeft />{t("wizard.back")}</Button><Button type="submit">{busy ? <LoaderCircle className="animate-spin" /> : <Play />}{busy ? t("common.processing") : t("builds.start")}</Button></div>
            </fieldset>
          </form>}
          {step < 4 && <div className="wizard-actions">
            {step > 0 && <Button type="button" variant="secondary" onClick={() => { setStep(previous => previous - 1); setError(undefined); }}><ArrowLeft />{t("wizard.back")}</Button>}
            <Button type="button" onClick={advance} disabled={step === 1 && sourceMode !== "manual" && refs.loading}>{t("wizard.continue")}<ArrowRight /></Button>
          </div>}
        </section>
      </div>
      <aside className="wizard-summary" aria-label={t("wizard.summary")}>
        <p className="eyebrow">{t("wizard.eyebrow")}</p><h2 className="mt-3 text-lg font-medium">{t("wizard.summary")}</h2><p className="mt-2 text-xs leading-5 text-muted-foreground">{t("wizard.summaryHint")}</p>
        <dl>{summaryLabels.map((label, index) => <div key={label}><dt>{label}<Button type="button" variant="ghost" size="icon" aria-label={t(`wizard.${editLabels[index]}`)} disabled={busy || index === step || index > step} onClick={() => edit(index)}><Pencil /></Button></dt><dd className={index === 1 ? "font-mono text-xs" : ""}>{answers[index]}</dd></div>)}</dl>
        <p className="wizard-summary-footer">{publish ? `${t("wizard.withRelease")} · ${version || t("wizard.notSelected")}` : t("wizard.buildOnly")}</p>
      </aside>
    </div>
  </>;
}
