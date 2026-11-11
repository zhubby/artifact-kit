"use client";

import { useState } from "react";
import { useTheme } from "next-themes";
import {
  Globe2,
  Monitor,
  Moon,
  SlidersHorizontal,
  Sparkles,
  Sun,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Modal } from "@/components/ui/dialog";
import { usePreferences, useUI } from "@/components/providers";
import { isLocale } from "@/lib/preferences";

function PreferenceFields({ expanded = false }: { expanded?: boolean }) {
  const { t } = useUI();
  const { locale, setLocale, effects, setEffects, ready, reduced } =
    usePreferences();
  const { theme, resolvedTheme, setTheme } = useTheme();
  const ThemeIcon =
    !ready || theme === "system"
      ? Monitor
      : resolvedTheme === "dark"
        ? Moon
        : Sun;
  return (
    <div
      className={expanded ? "preference-fields expanded" : "preference-fields"}
    >
      <label className="preference-select">
        <Globe2 size={15} />
        <span className="sr-only">{t("preferences.language")}</span>
        <select
          aria-label={t("preferences.language")}
          value={locale}
          onChange={(event) => {
            if (isLocale(event.target.value)) setLocale(event.target.value);
          }}
        >
          <option value="zh-CN">简体中文</option>
          <option value="en">English</option>
        </select>
      </label>
      <label className="preference-select">
        <ThemeIcon size={15} />
        <span className="sr-only">{t("preferences.theme")}</span>
        <select
          aria-label={t("preferences.theme")}
          value={ready ? theme : "system"}
          onChange={(event) => setTheme(event.target.value)}
        >
          <option value="system">{t("preferences.system")}</option>
          <option value="dark">{t("preferences.dark")}</option>
          <option value="light">{t("preferences.light")}</option>
        </select>
      </label>
      <Button
        type="button"
        variant="ghost"
        size={expanded ? "default" : "icon"}
        aria-label={
          effects
            ? t("preferences.pauseEffects")
            : t("preferences.enableEffects")
        }
        aria-pressed={effects}
        title={
          reduced
            ? t("preferences.reduced")
            : effects
              ? t("preferences.pauseEffects")
              : t("preferences.enableEffects")
        }
        onClick={() => setEffects(!effects)}
      >
        <Sparkles />
        {expanded && t("preferences.effects")}
      </Button>
      {expanded && reduced && (
        <p className="text-xs leading-6 text-muted-foreground">
          {t("preferences.reduced")}
        </p>
      )}
    </div>
  );
}

export function PreferencesControls() {
  const { t } = useUI();
  const [open, setOpen] = useState(false);
  return (
    <div className="preferences-controls">
      <div className="preferences-desktop">
        <PreferenceFields />
      </div>
      <Button
        type="button"
        variant="ghost"
        size="icon"
        className="preferences-mobile"
        aria-label={t("preferences.title")}
        onClick={() => setOpen(true)}
      >
        <SlidersHorizontal />
      </Button>
      <Modal
        open={open}
        onOpenChange={setOpen}
        title={t("preferences.title")}
        description={t("preferences.description")}
      >
        <PreferenceFields expanded />
      </Modal>
    </div>
  );
}
