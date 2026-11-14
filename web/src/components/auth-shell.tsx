"use client";

import type { ReactNode } from "react";
import {
  CheckCircle2,
  GitBranch,
  Layers3,
  Package,
  ShieldCheck,
} from "lucide-react";
import { Atmosphere } from "@/components/atmosphere";
import { BrandMark } from "@/components/brand-mark";
import { PreferencesControls } from "@/components/preferences-controls";
import { useUI } from "@/components/providers";
import AnimatedContent from "@/components/react-bits/AnimatedContent";

export function AuthShell({
  title,
  description,
  notice,
  children,
}: {
  title: string;
  description: string;
  notice: string;
  children: ReactNode;
}) {
  const { t: translate } = useUI();
  return (
    <div className="login-grid">
      <div className="login-preferences">
        <PreferencesControls />
      </div>
      <Atmosphere className="login-atmosphere" />
      <aside className="login-story">
        <div className="flex items-center gap-3 text-xl font-medium">
          <div className="brand-mark">
            <BrandMark />
          </div>
          Artifactory
          <span className="ml-2 rounded border border-border px-2 py-1 font-mono text-[9px] text-muted-foreground">
            {translate("login.controlPlane")}
          </span>
        </div>
        <AnimatedContent className="story-body my-auto py-16" stagger>
          <p className="eyebrow mb-6">{translate("login.pipeline")}</p>
          <h1>
            {translate("login.headlineFirst")}
            <br />
            {translate("login.headlineSecond")}
          </h1>
          <p className="mt-6 max-w-md text-sm leading-8 text-muted-foreground">
            {translate("login.descriptionFirst")}
            <br />
            {translate("login.descriptionSecond")}
          </p>
          <div className="mt-12 max-w-md">
            {[
              [GitBranch, translate("login.source")],
              [Layers3, translate("login.nodes")],
              [ShieldCheck, translate("login.signing")],
              [Package, translate("login.release")],
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
          <p className="eyebrow">{translate("login.eyebrow")}</p>
          <h2 className="mt-3 text-2xl font-medium tracking-tight">{title}</h2>
          <p className="mt-3 mb-8 text-sm leading-6 text-muted-foreground">
            {description}
          </p>
          {children}
          <div className="mt-10 flex gap-2 border-t border-border pt-5 text-xs leading-6 text-muted-foreground">
            <ShieldCheck className="mt-1 size-4 shrink-0" />
            <span>{notice}</span>
          </div>
        </AnimatedContent>
      </main>
    </div>
  );
}
