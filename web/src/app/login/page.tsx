"use client";

import { PreferencesControls } from "@/components/preferences-controls";
import { Atmosphere } from "@/components/atmosphere";
import AnimatedContent from "@/components/react-bits/AnimatedContent";
import { useUI } from "@/components/providers";

import { useState, type FormEvent } from "react";
import {
  ArrowRight,
  Box,
  CheckCircle2,
  GitBranch,
  Layers3,
  Package,
  ShieldCheck,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { ErrorBox, Field } from "@/components/common";
import { mutation } from "@/lib/api";
import { asError } from "@/lib/errors";

export default function Login() {
  const { t } = useUI();
  const [error, setError] = useState<Error>();
  const [busy, setBusy] = useState(false);
  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setBusy(true);
    setError(undefined);
    const values = new FormData(event.currentTarget);
    try {
      await mutation("/auth/login", Object.fromEntries(values));
      window.location.assign("/");
    } catch (error) {
      setError(asError(error, "login.failed"));
    } finally {
      setBusy(false);
    }
  }
  return (
    <div className="login-grid">
      <div className="login-preferences">
        <PreferencesControls />
      </div>
      <Atmosphere className="login-atmosphere" />
      <aside className="login-story">
        <div className="flex items-center gap-3 text-xl font-medium">
          <div className="brand-mark">
            <Box size={22} />
          </div>
          artifactory
          <span className="ml-2 rounded border border-border px-2 py-1 font-mono text-[9px] text-muted-foreground">
            {t("login.controlPlane")}
          </span>
        </div>
        <AnimatedContent className="story-body my-auto py-16" stagger>
          <p className="eyebrow mb-6">{t("login.pipeline")}</p>
          <h1>
            {t("login.headlineFirst")}
            <br />
            {t("login.headlineSecond")}
          </h1>
          <p className="mt-6 max-w-md text-sm leading-8 text-muted-foreground">
            {t("login.descriptionFirst")}
            <br />
            {t("login.descriptionSecond")}
          </p>
          <div className="mt-12 max-w-md">
            {[
              [GitBranch, t("login.source")],
              [Layers3, t("login.nodes")],
              [ShieldCheck, t("login.signing")],
              [Package, t("login.release")],
            ].map(([Icon, label], index) => {
              const Component = Icon as typeof GitBranch;
              return (
                <div className="pipeline-line" key={index}>
                  <span className="mr-2 font-mono text-[10px] text-primary">
                    0{index + 1}
                  </span>
                  <Component size={16} />
                  <span className="text-xs">{String(label)}</span>
                  <CheckCircle2 className="ml-auto size-3.5 text-muted-foreground" />
                </div>
              );
            })}
          </div>
        </AnimatedContent>
        <p className="mt-8 font-mono text-[10px] tracking-wider text-muted-foreground">
          LINUX / MACOS / WINDOWS / ANDROID / IOS
        </p>
      </aside>
      <main className="login-form">
        <AnimatedContent className="w-full max-w-[400px] login-form-content">
          <p className="eyebrow">{t("login.eyebrow")}</p>
          <h2 className="mt-3 text-2xl font-medium tracking-tight">
            {t("login.title")}
          </h2>
          <p className="mt-3 mb-8 text-sm leading-6 text-muted-foreground">
            {t("login.description")}
          </p>
          <ErrorBox message={error} />
          <form onSubmit={submit} className="form-stack">
            <Field label={t("login.username")}>
              <Input
                name="username"
                autoComplete="username"
                required
                autoFocus
                placeholder={t("login.usernamePlaceholder")}
              />
            </Field>
            <Field label={t("login.password")}>
              <Input
                name="password"
                type="password"
                autoComplete="current-password"
                required
                placeholder={t("login.passwordPlaceholder")}
              />
            </Field>
            <Button className="mt-2 w-full" disabled={busy}>
              {busy ? t("login.verifying") : t("login.submit")}
              <ArrowRight />
            </Button>
          </form>
          <div className="mt-10 flex gap-2 border-t border-border pt-5 text-xs leading-6 text-muted-foreground">
            <ShieldCheck className="mt-1 size-4 shrink-0" />
            <span>{t("login.notice")}</span>
          </div>
        </AnimatedContent>
      </main>
    </div>
  );
}
