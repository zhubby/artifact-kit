"use client";

import Link from "next/link";
import { useState, type FormEvent } from "react";
import { ArrowRight } from "lucide-react";
import { AuthShell } from "@/components/auth-shell";
import { useUI } from "@/components/providers";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { ErrorBox, Field } from "@/components/common";
import { mutation, useRemote } from "@/lib/api";
import { asError } from "@/lib/errors";
import type { AuthConfig } from "@/lib/types";

export default function Login() {
  const { t } = useUI();
  const config = useRemote<AuthConfig>("/auth/config", 60000);
  const registrationEnabled = !config.error && config.data?.registration_enabled;
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
    <AuthShell
      title={t("login.title")}
      description={t(registrationEnabled ? "register.loginDescription" : "login.description")}
      notice={t(registrationEnabled ? "register.notice" : "login.notice")}
    >
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
      {registrationEnabled && (
        <p className="mt-5 text-center text-sm text-muted-foreground">
          {t("register.noAccount")} {" "}
          <Link className="text-primary underline-offset-4 hover:underline" href="/register">
            {t("register.createAccount")}
          </Link>
        </p>
      )}
    </AuthShell>
  );
}
