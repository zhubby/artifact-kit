"use client";

import { NextIntlClientProvider, useLocale, useTranslations } from "next-intl";
import { ThemeProvider } from "next-themes";
import {
  createContext,
  useContext,
  useEffect,
  useState,
  useSyncExternalStore,
  type ReactNode,
} from "react";
import zh from "@/messages/zh-CN.json";
import en from "@/messages/en.json";
import {
  effectsStorage,
  localeCookie,
  themeStorage,
  type Locale,
} from "@/lib/preferences";
import { date as formatDate } from "@/lib/utils";

const PreferencesContext = createContext<{
  locale: Locale;
  setLocale: (value: Locale) => void;
  effects: boolean;
  setEffects: (value: boolean) => void;
  motion: boolean;
  ready: boolean;
  reduced: boolean;
} | null>(null);

function subscribeReduced(callback: () => void) {
  const query = window.matchMedia("(prefers-reduced-motion: reduce)");
  query.addEventListener("change", callback);
  return () => query.removeEventListener("change", callback);
}

export function Providers({
  initialLocale,
  children,
}: {
  initialLocale: Locale;
  children: ReactNode;
}) {
  const [locale, updateLocale] = useState(initialLocale);
  const [effects, updateEffects] = useState(true);
  const [ready, setReady] = useState(false);
  const reduced = useSyncExternalStore(
    subscribeReduced,
    () => window.matchMedia("(prefers-reduced-motion: reduce)").matches,
    () => true,
  );
  useEffect(() => {
    try {
      updateEffects(localStorage.getItem(effectsStorage) !== "off");
    } catch {
      /* Preferences still work when persistence is unavailable. */
    }
    setReady(true);
    const sync = (event: StorageEvent) => {
      if (event.key === effectsStorage) updateEffects(event.newValue !== "off");
    };
    window.addEventListener("storage", sync);
    return () => window.removeEventListener("storage", sync);
  }, []);
  useEffect(() => {
    document.documentElement.lang = locale;
    document.title =
      locale === "en"
        ? "Artifactory · Build console"
        : "Artifactory · 构建控制台";
  }, [locale]);
  const motion = ready && effects && !reduced;
  useEffect(() => {
    document.documentElement.dataset.motion = motion ? "on" : "off";
  }, [motion]);
  function setLocale(value: Locale) {
    updateLocale(value);
    try {
      document.cookie = `${localeCookie}=${value}; Path=/; Max-Age=31536000; SameSite=Lax${location.protocol === "https:" ? "; Secure" : ""}`;
    } catch {
      /* Keep the in-memory locale if cookie access is restricted. */
    }
  }
  function setEffects(value: boolean) {
    updateEffects(value);
    try {
      localStorage.setItem(effectsStorage, value ? "on" : "off");
    } catch {
      /* Keep the in-memory choice. */
    }
  }
  return (
    <ThemeProvider
      attribute="data-theme"
      storageKey={themeStorage}
      defaultTheme="system"
      enableSystem
      disableTransitionOnChange
    >
      <PreferencesContext.Provider
        value={{
          locale,
          setLocale,
          effects,
          setEffects,
          motion,
          ready,
          reduced,
        }}
      >
        <NextIntlClientProvider
          locale={locale}
          messages={locale === "en" ? en : zh}
          timeZone="UTC"
        >
          {children}
        </NextIntlClientProvider>
      </PreferencesContext.Provider>
    </ThemeProvider>
  );
}

export function usePreferences() {
  const context = useContext(PreferencesContext);
  if (!context) throw new Error("Preferences require Providers");
  return context;
}

export function useUI() {
  const t = useTranslations();
  const locale = useLocale();
  return {
    t,
    locale,
    number: (value: number) => new Intl.NumberFormat(locale).format(value),
    date: (value?: string | null) => formatDate(value, locale),
  };
}
