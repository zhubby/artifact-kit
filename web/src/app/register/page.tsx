"use client";

import Link from "next/link";
import { useRef, useState, type FormEvent } from "react";
import { ArrowRight, RefreshCw } from "lucide-react";
import { AuthShell } from "@/components/auth-shell";
import { ErrorBox, Field, Loading } from "@/components/common";
import { useUI } from "@/components/providers";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { mutation, useRemote } from "@/lib/api";
import { asError, LocalizedError } from "@/lib/errors";
import type { AuthConfig } from "@/lib/types";

export default function Register() {
  const { t: translate } = useUI();
  const config = useRemote<AuthConfig>("/auth/config", 60000);
  const [error, setError] = useState<Error>();
  const [busy, setBusy] = useState(false);
  const [draft, setDraft] = useState({ username: "", password: "", confirmPassword: "" });
  const submitting = useRef(false);

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (submitting.current) return;
    setError(undefined);
    const values = new FormData(event.currentTarget);
    const username = String(values.get("username") || "").trim();
    const password = String(values.get("password") || "");
    const encoder = new TextEncoder();
    if (!username || encoder.encode(username).length > 100) {
      setError(new LocalizedError("register.invalidUsername"));
      return;
    }
    const passwordBytes = encoder.encode(password).length;
    if (passwordBytes < 12 || passwordBytes > 256) {
      setError(new LocalizedError("register.invalidPassword"));
      return;
    }
    if (password !== values.get("confirmPassword")) {
      setError(new LocalizedError("register.passwordMismatch"));
      return;
    }
    if (config.error || !config.data?.registration_enabled) {
      setError(new LocalizedError("register.forbidden"));
      return;
    }
    submitting.current = true;
    setBusy(true);
    try {
      await mutation("/auth/register", { username, password });
      window.location.assign("/");
    } catch (error) {
      setError(asError(error, "register.failed"));
    } finally {
      submitting.current = false;
      setBusy(false);
    }
  }

  return (
    <AuthShell
      title={translate("register.title")}
      description={translate("register.description")}
      notice={translate("register.notice")}
    >
      {config.error ? (
        <>
          <ErrorBox message={config.error} />
          <Button variant="secondary" className="w-full" onClick={() => void config.reload()}>
            <RefreshCw />
            {translate("register.retryConfig")}
          </Button>
        </>
      ) : !config.data ? (
        <Loading />
      ) : !config.data.registration_enabled ? (
        <p role="status" className="text-sm leading-6 text-muted-foreground">
          {translate("register.disabled")}
        </p>
      ) : (
        <>
          <ErrorBox message={error} />
          <form onSubmit={submit} className="form-stack" aria-busy={busy}>
            <Field label={translate("login.username")} hint={translate("register.usernameHint")}>
              <Input
                name="username"
                value={draft.username}
                onChange={(event) => setDraft({ ...draft, username: event.target.value })}
                autoComplete="username"
                required
                autoFocus
                maxLength={100}
                disabled={busy}
                placeholder={translate("login.usernamePlaceholder")}
              />
            </Field>
            <Field label={translate("login.password")} hint={translate("register.passwordHint")}>
              <Input
                name="password"
                value={draft.password}
                onChange={(event) => setDraft({ ...draft, password: event.target.value })}
                type="password"
                autoComplete="new-password"
                required
                maxLength={256}
                disabled={busy}
                placeholder={translate("register.passwordPlaceholder")}
              />
            </Field>
            <Field label={translate("register.confirmPassword")}>
              <Input
                name="confirmPassword"
                value={draft.confirmPassword}
                onChange={(event) => setDraft({ ...draft, confirmPassword: event.target.value })}
                type="password"
                autoComplete="new-password"
                required
                maxLength={256}
                disabled={busy}
                placeholder={translate("register.confirmPlaceholder")}
              />
            </Field>
            <Button className="mt-2 w-full" disabled={busy}>
              {busy ? translate("register.submitting") : translate("register.submit")}
              <ArrowRight />
            </Button>
          </form>
        </>
      )}
      <p className="mt-5 text-center text-sm">
        <Link className="text-primary underline-offset-4 hover:underline" href="/login">
          {translate("register.backToLogin")}
        </Link>
      </p>
    </AuthShell>
  );
}
